//! A program that is not the kernel.
//!
//! Every line of code in this repository up to now has run in ring 0, in the
//! kernel's own address space, compiled into the kernel's own binary. This one
//! runs somewhere else, and cannot reach any of it.
//!
//! ## Why it is written in assembly
//!
//! Not to be clever — because of where it has to live. This program is compiled
//! into the kernel image like everything else, then **copied** into a freshly
//! allocated page that ring 3 is allowed to execute. Code that has been moved
//! cannot refer to anything by absolute address: the kernel's addresses are
//! unreachable from ring 3, and its own address is not what it was at compile
//! time. So it may only use RIP-relative references to things inside itself,
//! which is exactly the constraint a real program is under before a loader has
//! relocated it. Rust will not promise that; assembly you wrote yourself will.
//!
//! A proper OS solves this with an ELF loader and a separate compilation unit.
//! That is the honest next step, and this is the smallest thing that
//! demonstrates the boundary without it.

use crate::memory::{self, PAGE_SIZE};
use crate::println;
use crate::syscall;

/// Where the user program is mapped. Low, canonical, and nowhere near the
/// kernel — which lives up at 0x1000_0000_0000 — so a mistake in one is
/// unlikely to look like a valid address in the other.
const USER_CODE_ADDRESS: u64 = 0x20_0000;
const USER_STACK_ADDRESS: u64 = 0x21_0000;

/// How much of `program` to copy.
///
/// There is no symbol for "end of this naked function", so this is a fixed
/// window that comfortably exceeds it. Copying a little extra kernel code into
/// a user page is harmless here — it sits past the exit syscall and is never
/// executed — but it is the kind of shortcut an ELF loader exists to remove.
const PROGRAM_SIZE: usize = 256;

/// The whole of userspace, so far.
///
/// Reads its own CS to prove where it is, writes a string through a syscall,
/// then deliberately asks the kernel to print from a kernel address to show
/// what happens when a program reaches somewhere it should not.
#[unsafe(naked)]
unsafe extern "C" fn program() {
    core::arch::naked_asm!(
        // 1. write(message, len) -- a legitimate call with a pointer into our
        //    own page. RIP-relative, because we have been moved.
        //
        //    Both operands are computed from labels inside this function, with
        //    the length as the distance between them. Naming an assembler
        //    symbol directly would not work: in Intel syntax `mov rsi, label`
        //    is a *load from* that address, not the constant, which faults
        //    somewhere in the first page and looks nothing like the real cause.
        "mov rax, {write}",
        "lea rdi, [rip + 3f]",
        "lea rsi, [rip + 4f]",
        "sub rsi, rdi",
        "int 0x80",

        // 2. report our privilege level, read from the CPU rather than assumed.
        "mov rax, {report}",
        "mov rdi, cs",
        "int 0x80",

        // 3. ask the kernel to print from an address inside the kernel. A real
        //    program does this by accident, with a stale or uninitialised
        //    pointer. The kernel must refuse.
        "mov rax, {write}",
        "mov rdi, {kernel_address}",
        "mov rsi, 16",
        "int 0x80",

        // 4. exit. The only instruction here that does not come back.
        "mov rax, {exit}",
        "mov rdi, 0",
        "int 0x80",

        // Unreachable: if exit ever returned, stopping loudly beats running on
        // into whatever bytes follow.
        "2: jmp 2b",

        "3:",
        ".ascii \"  hello from ring 3 -- printed by a syscall\\n\"",
        "4:",

        write = const syscall::SYS_WRITE,
        report = const syscall::SYS_REPORT_CS,
        exit = const syscall::SYS_EXIT,
        kernel_address = const 0x1000_0000_0000u64,
    )
}

/// Map two user pages, copy the program in, and drop to ring 3.
pub fn run() {
    println!("  Mapping two pages that ring 3 is allowed to touch:");

    let code_frame = match memory::allocator().allocate() {
        Some(frame) => frame,
        None => {
            println!("  out of physical memory");
            return;
        }
    };
    let stack_frame = match memory::allocator().allocate() {
        Some(frame) => frame,
        None => {
            println!("  out of physical memory");
            return;
        }
    };

    // The code page starts out writable because we are about to write to it,
    // and is sealed further down once the program is in place. Nothing here
    // enforces no-execute -- that needs the NX bit and EFER.NXE, which this
    // kernel does not set up, so every user page is executable whether we want
    // it to be or not.
    let rw = memory::FLAG_USER | memory::FLAG_WRITABLE;
    if let Err(error) = unsafe { memory::map_page(USER_CODE_ADDRESS, code_frame, rw) } {
        println!("  could not map user code: {error}");
        return;
    }
    if let Err(error) = unsafe { memory::map_page(USER_STACK_ADDRESS, stack_frame, rw) } {
        println!("  could not map user stack: {error}");
        return;
    }

    println!("    code  {USER_CODE_ADDRESS:#x}  user, read-execute");
    println!("    stack {USER_STACK_ADDRESS:#x}  user, read-write");

    // Copy the program out of the kernel and into a page ring 3 can execute.
    unsafe {
        core::ptr::copy_nonoverlapping(
            program as *const u8,
            USER_CODE_ADDRESS as *mut u8,
            PROGRAM_SIZE,
        );
    }

    // Now take the write permission away. This is the smallest possible
    // version of what a loader does, and the read-only bit binds the kernel
    // too: with CR0.WP set, ring 0 writing here faults exactly like ring 3.
    if let Err(error) = unsafe { memory::set_flags(USER_CODE_ADDRESS, memory::FLAG_USER) } {
        println!("  could not seal user code: {error}");
        return;
    }
    println!("    code page sealed read-only, with the program already in it");

    let before = syscall::count();
    println!();
    println!("  Dropping to ring 3. Nothing below this line is privileged.");
    println!();

    unsafe {
        syscall::enter_ring3(
            USER_CODE_ADDRESS,
            // Top of the stack page, minus a slot: the ABI wants a function to
            // start life eight off 16-byte alignment, because a call would have
            // pushed a return address. See task.rs for what ignoring that costs.
            USER_STACK_ADDRESS + PAGE_SIZE as u64 - 8,
        );
    }

    println!();
    println!("  Back in ring 0, by way of {} syscalls.", syscall::count() - before);
    println!("  The only route back was the exit syscall -- ring 3 had no");
    println!("  instruction that could have returned here on its own.");
}
