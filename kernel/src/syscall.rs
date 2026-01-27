//! Ring 3, and the only door back in.
//!
//! Everything else in this kernel runs at the highest privilege the CPU has.
//! `explain rings` has been describing ring 3 since the first week of this
//! project, entirely from the outside — this module is where the machine
//! finally goes there.
//!
//! ## What ring 3 actually costs the code running in it
//!
//! Very little, and that is the surprising part. The instructions are the same,
//! the registers are the same, the address space is the same one the kernel is
//! using. Three things change:
//!
//!   * `cli`, `hlt`, `in`, `out`, `lgdt`, writes to CR3 — anything that touches
//!     the machine rather than the program — raise #GP instead of working.
//!   * Pages without the user bit are unreachable, even for reading.
//!   * There is exactly one way to ask for something: an interrupt.
//!
//! That last one is the whole design. A user program cannot *call* the kernel,
//! because calling means jumping to an address, and the kernel's addresses are
//! not reachable from ring 3. It can only raise an interrupt and let the CPU
//! decide where that lands — and the CPU decides from the IDT, which is
//! kernel-owned. The kernel picks its own entry points. That is the security
//! boundary, and it is enforced by hardware, not by convention.
//!
//! ## The stack switch nobody mentions
//!
//! On `int 0x80` from ring 3 the CPU does not keep using the user's stack. It
//! reads RSP from `TSS.privilege_stack_table[0]` and switches *before* pushing
//! the trap frame. It has to: the user chose RSP, and a kernel that pushed a
//! return address onto an address a user program controls would be handing over
//! the machine on the first syscall. See `gdt.rs`.
//!
//! ## Userspace is preemptible
//!
//! Ring 3 runs with the interrupt flag set, so the timer fires there like it
//! fires anywhere else. A program that loops forever without calling anything
//! is taken off the CPU regardless, which is the difference between an
//! operating system and an agreement.
//!
//! What makes that work is a detail of the trap frame rather than any new
//! machinery. When the timer interrupts ring 3, the CPU pushes the *user* SS,
//! RSP, RFLAGS, CS and RIP — a complete description of where the program was —
//! onto the kernel stack from `TSS.privilege_stack_table[0]`. The existing
//! timer entry saves the registers around it and hands the stack pointer to the
//! scheduler, exactly as it does for a kernel task. Resuming means returning
//! that stack pointer: the `iretq` sees a CS with RPL 3 and drops back to ring
//! 3 on the user's own stack.
//!
//! Nothing about CR3 needs to change on the way out. One user address space is
//! active at a time, and the kernel is mapped in all of them, so a kernel task
//! scheduled in the middle of a user program runs perfectly well in that
//! program's address space.

extern crate alloc;

use crate::memory;
use crate::{gdt, println};
use core::arch::naked_asm;

/// The interrupt a user program raises to ask for something.
///
/// `int 0x80` is Linux's historical choice, and the reason to copy it is that
/// every explanation of syscalls you have ever read uses this number. x86_64
/// has a faster dedicated `syscall` instruction; it is also a pile of MSR
/// configuration that hides what is happening. This is the version you can see.
pub const SYSCALL_VECTOR: u8 = 0x80;

pub const SYS_EXIT: u64 = 0;
pub const SYS_WRITE: u64 = 1;
pub const SYS_REPORT_CS: u64 = 2;
pub const SYS_FORK: u64 = 3;
pub const SYS_EXEC: u64 = 4;
pub const SYS_WAIT: u64 = 5;
pub const SYS_PIPE: u64 = 6;
pub const SYS_READ: u64 = 7;
pub const SYS_CLOSE: u64 = 8;

/// How many syscalls have crossed the boundary, for the shell.
static mut COUNT: u64 = 0;

pub fn count() -> u64 {
    unsafe { core::ptr::read_volatile(core::ptr::addr_of!(COUNT)) }
}

/// The kernel side of a syscall.
///
/// Ordinary Rust, running in ring 0, with the user's arguments in registers.
/// The returned value ends up in the user's RAX.
extern "C" fn dispatch(
    number: u64,
    arg0: u64,
    arg1: u64,
    arg2: u64,
    context: *mut u64,
) -> u64 {
    // Syscalls run with interrupts ON.
    //
    // The IDT entry is an interrupt gate, so the CPU cleared IF on the way in.
    // That was right when a syscall could not block: nothing here would ever
    // wait, so there was nothing to be woken for. It stopped being right the
    // moment `wait` and `exit` began parking the task -- `hlt` with interrupts
    // disabled is not a pause, it is the end of the machine, and no interrupt
    // can ever arrive to undo it.
    //
    // Turning them on here is safe now for a reason that was not true before:
    // every task has its own kernel stack. A syscall interrupted halfway
    // through is just a task with a deeper saved frame, and shared kernel state
    // is already guarded by locks that disable interrupts for their own short
    // critical sections.
    //
    // The frame is fully saved by the time this runs, which is what makes the
    // preemption harmless.
    crate::interrupts::enable();
    unsafe { COUNT += 1 };

    match number {
        SYS_EXIT => {
            let id = crate::task::current_id();
            println!("  [syscall] task {id} exit({arg0})");
            // Mark finished and wake whoever is waiting. The scheduler will
            // simply never pick this task again; there is no unwinding to do
            // and nowhere to return to.
            crate::task::finish(id, arg0);
            loop {
                crate::task::yield_now();
            }
        }

        SYS_FORK => fork(context),

        SYS_EXEC => exec(arg0, arg1, context),

        SYS_WAIT => {
            let id = crate::task::current_id();
            match crate::task::wait_any_child(id) {
                Some(code) => code,
                None => {
                    println!("  [syscall] task {id}: nothing to wait for");
                    u64::MAX
                }
            }
        }

        // write(fd, pointer, length). The fd is the point: 1 is the console
        // only because the table says so, and a shell can make it a pipe
        // instead without the program being told.
        SYS_WRITE => write(arg0, arg1, arg2),

        SYS_READ => read(arg0, arg1, arg2),

        SYS_PIPE => make_pipe(arg0),

        SYS_CLOSE => {
            let id = crate::task::current_id();
            if crate::task::close_descriptor(id, arg0 as usize) {
                0
            } else {
                u64::MAX
            }
        }

        // The user program reads its own CS and passes it here. Reading CS in
        // *this* handler would be useless -- by the time we run, the CPU has
        // already switched to the kernel's selector, so it would report ring 0
        // no matter who called. The privilege level a program is running at is
        // something only that program can observe about itself.
        SYS_REPORT_CS => {
            let ring = arg0 & 0b11;
            let kernel_ring = gdt::KERNEL_CODE & 0b11;
            println!();
            println!("  [syscall] the caller reports cs = {arg0:#x}");
            println!("            low two bits = {ring}, so it is running in ring {ring}");
            println!("            the kernel's own cs is {:#x}, ring {}", gdt::KERNEL_CODE, kernel_ring);
            ring
        }

        _ => {
            println!("  [syscall] unknown call {number}");
            u64::MAX
        }
    }
}

/// Where `int 0x80` lands.
///
/// Hand-written for the same reason the timer is: `extern "x86-interrupt"`
/// preserves registers, and preserving registers is exactly wrong here — the
/// arguments *are* the registers, and the return value has to survive back into
/// user mode.
///
/// # Safety
/// Installed as IDT vector 0x80 with DPL 3. Never called from Rust.
#[unsafe(naked)]
pub unsafe extern "C" fn syscall_entry() {
    naked_asm!(
        // The same fifteen registers, in the same order, as `timer_entry`.
        //
        // They used to be nine, chosen for stack alignment, and that difference
        // is what kept the two schedulers apart: a task saved by the timer and
        // a task saved by a syscall had different shapes, so each needed its own
        // way to be resumed. Saving identically makes a task a task, and the
        // scheduler stops caring how it stopped running.
        "push rax", "push rcx", "push rdx", "push rbx",
        "push rbp", "push rsi", "push rdi",
        "push r8",  "push r9",  "push r10", "push r11",
        "push r12", "push r13", "push r14", "push r15",

        // Fifteen registers plus the CPU's five-word frame is 160 bytes, so RSP
        // is 16-aligned here and `call` leaves the eight-byte offset the ABI
        // expects.
        // Shift the user's registers into the C argument registers, highest
        // first so nothing is clobbered before it has been read. The context
        // pointer goes last, in the fifth slot -- `fork` needs it, because the
        // child is this frame with a different answer in RAX.
        "mov r8, rsp",
        "mov rcx, rdx",
        "mov rdx, rsi",
        "mov rsi, rdi",
        "mov rdi, rax",
        "call {dispatch}",

        // Put the result where the user's RAX will be restored from, rather
        // than leaving it in RAX and skipping the pop. `exec` rewrites this
        // whole frame, and this way it does not have to know that.
        "mov [rsp + 112], rax",

        "pop r15", "pop r14", "pop r13", "pop r12",
        "pop r11", "pop r10", "pop r9",  "pop r8",
        "pop rdi", "pop rsi", "pop rbp",
        "pop rbx", "pop rdx", "pop rcx", "pop rax",
        "iretq",
        dispatch = sym dispatch,
    )
}

// --- fork, exec, wait ----------------------------------------------------

/// One call, two returns.
///
/// The child is this exact task -- same saved registers, same instruction
/// pointer, same stack contents -- with two differences: a duplicated address
/// space, and a zero where the parent gets a task id. Because the timer and the
/// syscall path now save in the same shape, copying it is a memcpy and a store.
fn fork(context: *mut u64) -> u64 {
    let parent_id = crate::task::current_id();
    let parent = memory::AddressSpace::adopt(memory::active_root());

    let Some(child_space) = (unsafe { parent.fork() }) else {
        println!("  [syscall] fork failed: out of memory");
        return u64::MAX;
    };
    let root = child_space.root();
    core::mem::forget(child_space);

    match crate::task::fork_from(parent_id, root, context) {
        Ok(child) => {
            println!("  [syscall] fork: task {child}, address space {root:#x}, no pages copied");
            child as u64
        }
        Err(error) => {
            unsafe { memory::AddressSpace::adopt(root).destroy() };
            println!("  [syscall] fork failed: {error}");
            u64::MAX
        }
    }
}

/// Replace this task's program with another one.
///
/// Note what does *not* change: the task id, its parent, and the fact that
/// somebody may be waiting for it. `exec` is not a new process -- it is the same
/// one wearing a different program, which is why `fork` + `exec` is how a shell
/// starts anything.
///
/// It does not return either, and the way it does not is the neat part. Rather
/// than jumping to ring 3 itself, it **rewrites the trap frame it was called
/// from**: new entry point, new stack, registers zeroed. `syscall_entry` then
/// pops that frame and `iretq`s exactly as it always would, and lands somewhere
/// else entirely.
fn exec(path: u64, length: u64, context: *mut u64) -> u64 {
    if !memory::is_user_accessible(path, length) || length > 64 {
        println!("  [syscall] exec: bad path pointer");
        return u64::MAX;
    }
    let bytes = unsafe { core::slice::from_raw_parts(path as *const u8, length as usize) };
    let Ok(borrowed) = core::str::from_utf8(bytes) else {
        return u64::MAX;
    };
    // Copy it out of user memory now, while that memory still exists. This
    // function is about to switch address spaces, and a &str held across that
    // is a use-after-free with no free involved: the address stays valid and
    // quietly starts meaning something else.
    let name: alloc::string::String = borrowed.into();

    let Ok(volume) = crate::fat::mount() else {
        println!("  [syscall] exec: no filesystem");
        return u64::MAX;
    };
    let data = match volume.find(&name).and_then(|entry| volume.read_file(&entry)) {
        Ok(data) => data,
        Err(error) => {
            println!("  [syscall] exec {name}: {error}");
            return u64::MAX;
        }
    };

    // Build the replacement before destroying anything. If the new program will
    // not load, the old one is still intact and exec simply fails -- which is
    // why a failed exec on a real system leaves the caller running.
    let Some(fresh) = memory::AddressSpace::new() else {
        return u64::MAX;
    };
    let old_root = memory::active_root();
    unsafe { fresh.activate() };

    let loaded = match unsafe { crate::elf::load(&data, false) } {
        Ok(loaded) => loaded,
        Err(error) => {
            println!("  [syscall] exec {name}: {error}");
            unsafe {
                memory::activate_root(old_root);
                fresh.destroy();
            }
            return u64::MAX;
        }
    };

    let Some(stack_top) = crate::user::map_stack() else {
        unsafe {
            memory::activate_root(old_root);
            fresh.destroy();
        }
        return u64::MAX;
    };

    let id = crate::task::current_id();
    crate::task::set_address_space(id, fresh.root());
    core::mem::forget(fresh);
    unsafe { memory::AddressSpace::adopt(old_root).destroy() };

    // Rewrite the frame this syscall is going to return through: fifteen zeroed
    // registers, then RIP, CS, RFLAGS, RSP, SS.
    unsafe {
        for index in 0..15 {
            *context.add(index) = 0;
        }
        *context.add(15) = loaded.entry;
        *context.add(16) = gdt::USER_CODE as u64;
        *context.add(17) = 0x202;
        *context.add(18) = stack_top - 8;
        *context.add(19) = gdt::USER_DATA as u64;
    }

    println!("  [syscall] exec {name}: task {id} is now a different program");
    0
}

// --- descriptors and pipes ------------------------------------------------

/// Check a user buffer once, in one place, before anything dereferences it.
fn user_slice(pointer: u64, length: u64) -> Option<(u64, usize)> {
    if length > 4096 || !memory::is_user_accessible(pointer, length) {
        return None;
    }
    Some((pointer, length as usize))
}

fn write(fd: u64, pointer: u64, length: u64) -> u64 {
    let Some((pointer, length)) = user_slice(pointer, length) else {
        println!("  [syscall] write: refusing {pointer:#x} len {length}");
        return u64::MAX;
    };
    let bytes = unsafe { core::slice::from_raw_parts(pointer as *const u8, length) };

    let id = crate::task::current_id();
    match crate::task::descriptor(id, fd as usize) {
        crate::task::Descriptor::Console => match core::str::from_utf8(bytes) {
            Ok(text) => {
                crate::print!("{text}");
                length as u64
            }
            Err(_) => u64::MAX,
        },
        crate::task::Descriptor::PipeWrite(pipe) => crate::pipe::write(pipe, bytes) as u64,
        // Writing to the read end is not a slip the kernel should paper over.
        crate::task::Descriptor::PipeRead(_) | crate::task::Descriptor::Free => u64::MAX,
    }
}

fn read(fd: u64, pointer: u64, length: u64) -> u64 {
    let Some((pointer, length)) = user_slice(pointer, length) else {
        return u64::MAX;
    };

    let id = crate::task::current_id();
    match crate::task::descriptor(id, fd as usize) {
        crate::task::Descriptor::PipeRead(pipe) => {
            let out = unsafe { core::slice::from_raw_parts_mut(pointer as *mut u8, length) };
            crate::pipe::read(pipe, out) as u64
        }
        // Reading the console would need the keyboard to belong to a process
        // rather than to the shell, which is a question about terminals and
        // sessions rather than about pipes.
        crate::task::Descriptor::Console => u64::MAX,
        _ => u64::MAX,
    }
}

/// `pipe(&mut [read_fd, write_fd])`.
///
/// Two descriptors out of one call, which is why it takes a pointer rather than
/// returning a value: there is no room for two answers in one register, and
/// packing them would make the caller unpack something the kernel invented.
fn make_pipe(pointer: u64) -> u64 {
    if !memory::is_user_accessible(pointer, 16) {
        println!("  [syscall] pipe: that is not user memory");
        return u64::MAX;
    }

    let Some(pipe) = crate::pipe::create() else {
        println!("  [syscall] pipe: none left");
        return u64::MAX;
    };

    let id = crate::task::current_id();
    let Some(read_fd) = crate::task::add_descriptor(id, crate::task::Descriptor::PipeRead(pipe))
    else {
        crate::pipe::close_reader(pipe);
        crate::pipe::close_writer(pipe);
        return u64::MAX;
    };
    let Some(write_fd) = crate::task::add_descriptor(id, crate::task::Descriptor::PipeWrite(pipe))
    else {
        crate::task::close_descriptor(id, read_fd);
        crate::pipe::close_writer(pipe);
        return u64::MAX;
    };

    unsafe {
        *(pointer as *mut u64) = read_fd as u64;
        *((pointer as *mut u64).add(1)) = write_fd as u64;
    }
    println!("  [syscall] pipe {pipe}: read fd {read_fd}, write fd {write_fd}");
    0
}
