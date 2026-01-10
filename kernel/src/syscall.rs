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

/// Kernel RSP at the moment we dropped to ring 3, so `exit` can get back.
static mut KERNEL_RSP: u64 = 0;

/// How many syscalls have crossed the boundary, for the shell.
static mut COUNT: u64 = 0;

pub fn count() -> u64 {
    unsafe { core::ptr::read_volatile(core::ptr::addr_of!(COUNT)) }
}

/// The kernel side of a syscall.
///
/// Ordinary Rust, running in ring 0, with the user's arguments in registers.
/// The returned value ends up in the user's RAX.
extern "C" fn dispatch(number: u64, arg0: u64, arg1: u64, context: *mut u64) -> u64 {
    unsafe { COUNT += 1 };

    match number {
        SYS_EXIT => {
            let id = crate::process::current();
            println!("  [syscall] pid {id} exit({arg0})");

            // A parent blocked in `wait` is owed this number, and nothing else
            // in the system can give it to them.
            if let Some(parent) = crate::process::exit_current(arg0) {
                let code = crate::process::reap_child(parent).unwrap_or(arg0);
                println!("  [syscall] pid {parent} was waiting -- resuming it with {code}");
                let (root, saved) = crate::process::take_over(parent);
                unsafe {
                    crate::memory::activate_root(root);
                    resume_user(saved, code)
                }
            }

            // Otherwise give the CPU to anything else that can use it. A
            // forked child that nobody waited for gets its turn here.
            if let Some(next) = crate::process::next_ready(id) {
                println!("  [syscall] switching to pid {next}");
                let (root, saved) = crate::process::take_over(next);
                unsafe {
                    crate::memory::activate_root(root);
                    resume_user(saved, 0)
                }
            }

            // Nothing left. Back to the shell.
            unsafe { return_to_kernel() }
        }

        SYS_EXEC => exec(arg0, arg1),

        SYS_WAIT => wait(context),

        SYS_FORK => fork(context),

        SYS_WRITE => {
            // The one check that makes this a kernel rather than a library.
            //
            // arg0 is a pointer chosen by code we do not trust. If it names a
            // kernel page, dereferencing it would leak kernel memory to ring 3
            // through a completely legitimate-looking interface -- the program
            // never left its sandbox, it just asked us to reach out of ours.
            if !memory::is_user_accessible(arg0, arg1) {
                println!("  [syscall] write({arg0:#x}, {arg1}) REFUSED");
                println!("            that address is not user-accessible.");
                return u64::MAX; // -1
            }
            if arg1 > 4096 {
                return u64::MAX;
            }

            let bytes = unsafe { core::slice::from_raw_parts(arg0 as *const u8, arg1 as usize) };
            match core::str::from_utf8(bytes) {
                Ok(text) => {
                    crate::print!("{text}");
                    arg1
                }
                Err(_) => u64::MAX,
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
        // Save everything the user might care about except RAX, which is where
        // the result goes. Nine pushes, not eight: the trap frame leaves RSP
        // eight off 16-byte alignment, and `call` into Rust has to land on a
        // properly aligned stack or the first SSE spill faults. One extra push
        // is the cheapest way to say that.
        "push rcx", "push rdx", "push rsi", "push rdi",
        "push r8",  "push r9",  "push r10", "push r11",
        "push rbx",

        // The user's calling convention -- number in rax, args in rdi and rsi --
        // shifted into the one Rust expects.
        // Fourth argument: where the saved context starts, which is exactly
        // where RSP points now. `fork` needs it -- the child is nothing but a
        // copy of this frame with a different answer in RAX.
        "mov rcx, rsp",
        "mov rdx, rsi",
        "mov rsi, rdi",
        "mov rdi, rax",
        "call {dispatch}",

        "pop rbx",
        "pop r11", "pop r10", "pop r9",  "pop r8",
        "pop rdi", "pop rsi", "pop rdx", "pop rcx",
        // RAX deliberately not restored: it carries dispatch's return value
        // back across the boundary.
        "iretq",
        dispatch = sym dispatch,
    )
}

/// Drop to ring 3 at `entry`, with `stack` as the user stack pointer.
///
/// There is no instruction for "lower your privilege". The only way down is to
/// return from an interrupt that never happened: build a trap frame that claims
/// we came from ring 3, and `iretq` into the lie. The CPU checks the RPL of the
/// CS being restored, sees 3, and obliges.
///
/// # Safety
/// `entry` and `stack` must be mapped user-accessible, and the code at `entry`
/// must eventually make an exit syscall — nothing else brings us back.
#[unsafe(naked)]
pub unsafe extern "C" fn enter_ring3(entry: u64, stack: u64) {
    naked_asm!(
        // Everything the SysV ABI says a callee must preserve. `exit` returns
        // by restoring RSP to here and popping them.
        "push rbp", "push rbx", "push r12", "push r13", "push r14", "push r15",
        // And the flags -- above all the interrupt flag. The frame below hands
        // ring 3 an RFLAGS with IF clear, and `exit` does not come back through
        // an iretq that would restore ours. Without saving it here, the kernel
        // resumes with interrupts off permanently: no timer, no scheduler, and
        // a shell that never sees another keystroke because input arrives by
        // IRQ. The program's output has already been printed by then, so the
        // machine looks like it worked and is in fact deaf.
        "pushfq",
        "mov [rip + {saved}], rsp",

        // Data segments have to be ring 3 too, or the first push in user mode
        // faults on a segment it is not allowed to use.
        "mov ax, {udata}",
        "mov ds, ax",
        "mov es, ax",

        // The frame iretq will consume, pushed in the order the CPU would have.
        "push {udata}",  // SS
        "push rsi",      // RSP -- the user stack
        // RFLAGS with the interrupt flag SET. This one bit is the whole of
        // preemptible userspace: with it clear, a ring 3 program that never
        // makes a syscall owns the machine forever, and the only thing that
        // could take it back was the program's own good manners.
        "push 0x202",
        "push {ucode}",  // CS -- RPL 3 here is what performs the transition
        "push rdi",      // RIP -- where user code begins
        "iretq",
        udata = const gdt::USER_DATA,
        ucode = const gdt::USER_CODE,
        saved = sym KERNEL_RSP,
    )
}

/// Drop into ring 3 without saving a return path.
///
/// `enter_ring3` records where the kernel was so `exit` can get back there.
/// `exec` must not: the kernel's return path was recorded when the *first*
/// program started, and it is still the right one. Saving again here would
/// point it at a syscall stack that is about to be abandoned.
///
/// # Safety
/// As `enter_ring3`, and only valid inside a process that already exists.
#[unsafe(naked)]
pub unsafe extern "C" fn enter_user(entry: u64, stack: u64) -> ! {
    naked_asm!(
        "mov ax, {udata}",
        "mov ds, ax",
        "mov es, ax",
        "push {udata}",
        "push rsi",
        "push 0x202",
        "push {ucode}",
        "push rdi",
        "iretq",
        udata = const gdt::USER_DATA,
        ucode = const gdt::USER_CODE,
    )
}

/// The exit path: abandon the syscall frame and resume the kernel.
///
/// A normal syscall returns with `iretq` back into ring 3. `exit` must not —
/// there is nothing to go back to. So it throws away the stack the trap frame
/// is sitting on, restores the RSP `enter_ring3` recorded, and returns as
/// though `enter_ring3` had simply finished.
///
/// # Safety
/// Only valid while a ring 3 program is running under `enter_ring3`.
#[unsafe(naked)]
unsafe extern "C" fn return_to_kernel() -> ! {
    naked_asm!(
        "mov rsp, [rip + {saved}]",
        // Interrupts come back on here, if they were on when we left.
        "popfq",
        // Segment registers still hold ring 3 selectors; put the kernel's back.
        "mov ax, {kdata}",
        "mov ds, ax",
        "mov es, ax",
        "pop r15", "pop r14", "pop r13", "pop r12", "pop rbx", "pop rbp",
        "ret",
        kdata = const gdt::KERNEL_DATA,
        saved = sym KERNEL_RSP,
    )
}

// --- fork, exec, wait ----------------------------------------------------

/// The classic one: one call, two returns.
///
/// Nothing is created from scratch. The child is this exact process — same
/// registers, same instruction pointer, same stack contents — with two
/// differences: a duplicated address space, and a zero where the parent gets a
/// process id. That one difference in one register is the entire way a program
/// tells which of the two it is.
fn fork(context: *mut u64) -> u64 {
    let parent_id = crate::process::current();
    let parent = crate::memory::AddressSpace::adopt(crate::memory::active_root());

    let Some(child_space) = (unsafe { parent.fork() }) else {
        println!("  [syscall] fork failed: out of memory");
        return u64::MAX;
    };

    // The child's saved state is the parent's, verbatim -- including the return
    // address that drops it back into the middle of this same `int 0x80`.
    let mut snapshot = [0u64; crate::process::CONTEXT_WORDS];
    for (index, slot) in snapshot.iter_mut().enumerate() {
        *slot = unsafe { *context.add(index) };
    }

    let root = child_space.root();
    core::mem::forget(child_space);

    match crate::process::add_child(parent_id, root, snapshot) {
        Some(child_id) => {
            println!("  [syscall] fork: pid {child_id}, address space {root:#x}, no pages copied");
            child_id as u64
        }
        None => {
            unsafe { crate::memory::AddressSpace::adopt(root).destroy() };
            println!("  [syscall] fork failed: no free process slots");
            u64::MAX
        }
    }
}

/// Replace this process with a different program.
///
/// Note what does *not* change: the process id, its parent, and the fact that
/// somebody may be waiting for it. `exec` is not a new process — it is the same
/// process wearing a different program, which is why the pair `fork` + `exec`
/// is how a shell starts anything. Fork to get a second process, exec to make
/// it something else.
///
/// It also does not return. There is nowhere to return to: the code that called
/// it no longer exists.
fn exec(path: u64, length: u64) -> u64 {
    if !memory::is_user_accessible(path, length) || length > 64 {
        println!("  [syscall] exec: bad path pointer");
        return u64::MAX;
    }
    let bytes = unsafe { core::slice::from_raw_parts(path as *const u8, length as usize) };
    let Ok(borrowed) = core::str::from_utf8(bytes) else {
        return u64::MAX;
    };
    // Copy it out of user memory now, while that memory still exists.
    //
    // `name` points into the caller's address space, and this function is about
    // to switch away from it. Holding a &str across that is a use-after-free
    // with no free involved -- the address stays valid and starts meaning
    // something else entirely. It showed up as a log line printing the new
    // program's blank memory where the filename should have been, which is a
    // gentler symptom than it deserved.
    let name: alloc::string::String = borrowed.into();
    let name = name.as_str();

    let Ok(volume) = crate::fat::mount() else {
        println!("  [syscall] exec: no filesystem");
        return u64::MAX;
    };
    let data = match volume.find(name).and_then(|entry| volume.read_file(&entry)) {
        Ok(data) => data,
        Err(error) => {
            println!("  [syscall] exec {name}: {error}");
            return u64::MAX;
        }
    };

    // Build the replacement before destroying anything. If the new program will
    // not load, the old one is still intact and `exec` can simply fail --
    // which is the entire reason a failed exec on a real system leaves the
    // caller running rather than killing it.
    let Some(fresh) = crate::memory::AddressSpace::new() else {
        return u64::MAX;
    };
    let old_root = crate::memory::active_root();
    unsafe { fresh.activate() };

    let loaded = match unsafe { crate::elf::load(&data, false) } {
        Ok(loaded) => loaded,
        Err(error) => {
            println!("  [syscall] exec {name}: {error}");
            unsafe {
                crate::memory::activate_root(old_root);
                fresh.destroy();
            }
            return u64::MAX;
        }
    };

    let Some(stack_top) = crate::user::map_stack() else {
        unsafe {
            crate::memory::activate_root(old_root);
            fresh.destroy();
        }
        return u64::MAX;
    };

    let id = crate::process::current();
    crate::process::replace_root(id, fresh.root());
    core::mem::forget(fresh);

    // Only now is the old image unreachable.
    unsafe { crate::memory::AddressSpace::adopt(old_root).destroy() };

    println!("  [syscall] exec {name}: pid {id} is now a different program");
    unsafe { enter_user(loaded.entry, stack_top - 8) }
}

/// Wait for a child to finish, and collect the number it left behind.
///
/// If a child has already finished, this is a lookup. If not, the parent has to
/// stop — and stopping means saving its registers and giving the CPU to the
/// child, which is the same switch `fork` needed and the reason the two had to
/// be built together.
fn wait(context: *mut u64) -> u64 {
    let id = crate::process::current();

    // Already finished? Then there is nothing to wait for.
    if let Some(code) = crate::process::reap_child(id) {
        println!("  [syscall] pid {id}: child already finished with {code}");
        return code;
    }

    if !crate::process::has_child(id) {
        println!("  [syscall] pid {id}: nothing to wait for");
        return u64::MAX;
    }

    let Some(child) = crate::process::next_ready(id) else {
        println!("  [syscall] pid {id}: child exists but cannot run");
        return u64::MAX;
    };

    // Stop being runnable, remember where we were, and hand the CPU over. The
    // child's `exit` is what brings us back, with its code in RAX.
    crate::process::save_context(id, context);
    crate::process::begin_wait(id);

    println!("  [syscall] pid {id} waits; running pid {child}");
    let (root, saved) = crate::process::take_over(child);
    unsafe {
        crate::memory::activate_root(root);
        resume_user(saved, 0)
    }
}

/// Resume a saved user context, putting `result` in RAX.
///
/// The pops here mirror `syscall_entry` exactly, because the buffer was filled
/// from that function's stack. RAX is not restored from it -- it is supplied,
/// and that is the whole trick: the same saved frame resumed with 0 is a forked
/// child, and resumed with an exit code is a parent returning from `wait`.
///
/// # Safety
/// `context` must point at CONTEXT_WORDS qwords laid out as syscall_entry's
/// stack, and the address space it belongs to must already be active.
#[unsafe(naked)]
unsafe extern "C" fn resume_user(context: *const u64, result: u64) -> ! {
    naked_asm!(
        "mov rsp, rdi",
        "mov rax, rsi",
        "pop rbx",
        "pop r11", "pop r10", "pop r9",  "pop r8",
        "pop rdi", "pop rsi", "pop rdx", "pop rcx",
        "iretq",
    )
}
