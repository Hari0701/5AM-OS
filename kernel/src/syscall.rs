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
//! ## What is deliberately not here yet
//!
//! User mode runs with interrupts disabled. The timer would otherwise fire in
//! ring 3, land in `task.rs`, and try to switch stacks with a privilege change
//! half-done. Preemptible userspace needs the scheduler to understand rings,
//! which is the next thing, not this thing.

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
extern "C" fn dispatch(number: u64, arg0: u64, arg1: u64) -> u64 {
    unsafe { COUNT += 1 };

    match number {
        SYS_EXIT => {
            println!("  [syscall] exit({arg0}) -- leaving ring 3");
            // Never returns: unwinds all the way out of enter_ring3.
            unsafe { return_to_kernel() }
        }

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
        "push 0x2",      // RFLAGS: interrupts OFF in user mode, see module docs
        "push {ucode}",  // CS -- RPL 3 here is what performs the transition
        "push rdi",      // RIP -- where user code begins
        "iretq",
        udata = const gdt::USER_DATA,
        ucode = const gdt::USER_CODE,
        saved = sym KERNEL_RSP,
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
