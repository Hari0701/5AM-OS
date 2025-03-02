//! Running a program the kernel did not compile into itself.
//!
//! The first version of this file held the user program as a naked function and
//! copied a fixed 256 bytes of it into a page. It worked, and it was a stunt:
//! the program had to be written in assembly, could not refer to anything by
//! name, and the kernel had to be told in advance how big it was.
//!
//! Now `userland/` is a separate crate, compiled and linked on its own, and
//! what arrives here is an ELF file. The kernel reads its program headers and
//! does what they say. Nothing is shared between the two but the syscall
//! numbers and a register convention.
//!
//! The file is still embedded in the kernel image rather than read from disk,
//! because there is no filesystem yet. That is the next missing piece, and it
//! is a smaller one than this was: `load()` already takes a byte slice and does
//! not care where it came from.

use crate::memory::{self, PAGE_SIZE};
use crate::println;
use crate::{elf, syscall};

/// The program, built by build.rs and baked into the kernel image.
static PROGRAM: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/user.elf"));

/// Where the user stack goes. Below the load address in `userland/link.ld`, so
/// growing down moves it away from the program rather than into it.
const USER_STACK_ADDRESS: u64 = 0x10_0000;
const USER_STACK_PAGES: u64 = 4;

/// Run the copy of the program that was built into the kernel image.
pub fn run() {
    println!("  The program is a {} byte ELF file, built separately.", PROGRAM.len());
    run_bytes(PROGRAM);
}

/// Run an ELF from anywhere -- the kernel image, or a disk.
///
/// `load()` takes a byte slice and does not care where it came from, which is
/// the whole reason `exec` needed nothing new from this module.
pub fn run_bytes(program: &[u8]) {
    println!("  Reading its program headers:");

    let loaded = match unsafe { elf::load(program, true) } {
        Ok(loaded) => loaded,
        Err(error) => {
            println!("  refusing to run it: {error}");
            return;
        }
    };

    println!(
        "  {} segments loaded, {} bytes of fresh zeroed memory.",
        loaded.segments, loaded.bytes_zeroed
    );

    // The stack is the loader's job, not the file's: no program header asks for
    // one. Every process on every OS gets a stack it never requested.
    for page in 0..USER_STACK_PAGES {
        let address = USER_STACK_ADDRESS + page * PAGE_SIZE as u64;
        if memory::translate(address).is_some() {
            continue;
        }
        let Some(frame) = memory::allocator().allocate() else {
            println!("  out of physical memory for the user stack");
            return;
        };
        let flags = memory::FLAG_USER | memory::FLAG_WRITABLE;
        if let Err(error) = unsafe { memory::map_page(address, frame, flags) } {
            println!("  could not map the user stack: {error}");
            return;
        }
        unsafe { core::ptr::write_bytes(address as *mut u8, 0, PAGE_SIZE) };
    }

    let stack_top = USER_STACK_ADDRESS + USER_STACK_PAGES * PAGE_SIZE as u64;
    println!(
        "  stack     {USER_STACK_ADDRESS:#010x}  {} bytes, rw-, mapped by the kernel",
        USER_STACK_PAGES * PAGE_SIZE as u64
    );

    let before = syscall::count();
    println!();
    println!("  Jumping to {:#x} in ring 3.", loaded.entry);
    println!();

    unsafe {
        // Eight below the top, because a function expects to start life where a
        // call would have left it. See task.rs for what ignoring that costs.
        syscall::enter_ring3(loaded.entry, stack_top - 8);
    }

    println!();
    println!("  Back in ring 0, by way of {} syscalls.", syscall::count() - before);
    println!("  Nothing in that program was compiled with this kernel. The only");
    println!("  thing they share is the number in `int 0x80`.");
}
