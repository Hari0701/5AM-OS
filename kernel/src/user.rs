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

/// The program baked into the kernel image, for tests that need a known-good
/// ELF without a disk.
pub fn embedded_program() -> &'static [u8] {
    PROGRAM
}

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
    // A private address space, so this program cannot see any other one's
    // memory -- which until now it could, because every task shared a single
    // set of page tables and ring 3 only fenced userspace off from the kernel.
    let Some(space) = memory::AddressSpace::new() else {
        println!("  out of physical memory for an address space");
        return;
    };
    let kernel_root = memory::active_root();

    // Switch before loading, so every mapping below lands in the new space and
    // the copies go to the right place. Safe because the kernel is mapped
    // identically in both -- see AddressSpace for why that is not optional.
    unsafe { space.activate() };
    println!("  Private address space at {:#x}.", space.root());
    println!("  Reading its program headers:");

    let loaded = match unsafe { elf::load(program, true) } {
        Ok(loaded) => loaded,
        Err(error) => {
            println!("  refusing to run it: {error}");
            unsafe { memory::activate_root(kernel_root) };
            unsafe { space.destroy() };
            return;
        }
    };

    println!(
        "  {} segments loaded, {} bytes of fresh zeroed memory.",
        loaded.segments, loaded.bytes_zeroed
    );

    let Some(stack_top) = map_stack() else {
        println!("  could not map the user stack");
        unsafe { memory::activate_root(kernel_root) };
        unsafe { space.destroy() };
        return;
    };
    println!(
        "  stack     {USER_STACK_ADDRESS:#010x}  {} bytes, rw-, mapped by the kernel",
        USER_STACK_PAGES * PAGE_SIZE as u64
    );

    let before = syscall::count();
    println!();
    println!("  Jumping to {:#x} in ring 3.", loaded.entry);
    println!();

    // From here it is a process: it may fork, exec and exit, and the process
    // table owns its address space.
    let root = space.root();
    core::mem::forget(space);
    crate::process::install_first(root);

    unsafe {
        syscall::enter_ring3(loaded.entry, stack_top - 8);
    }

    // Every process is finished. Switch back and reclaim all of them.
    unsafe { memory::activate_root(kernel_root) };
    let freed = unsafe { crate::process::destroy_all(kernel_root) };

    println!();
    println!("  Back in ring 0, by way of {} syscalls.", syscall::count() - before);
    println!("  {freed} frames returned; every address space destroyed.");
}

/// Map a fresh stack into the active address space, returning its top.
///
/// No program header ever asks for a stack. Every process on every operating
/// system gets one it never requested, and this is where that happens here.
/// `exec` calls it again for the replacement program.
pub fn map_stack() -> Option<u64> {
    for page in 0..USER_STACK_PAGES {
        let address = USER_STACK_ADDRESS + page * PAGE_SIZE as u64;
        if memory::translate(address).is_some() {
            continue;
        }
        let frame = memory::allocator().allocate()?;
        let flags = memory::FLAG_USER | memory::FLAG_WRITABLE;
        if unsafe { memory::map_page(address, frame, flags) }.is_err() {
            return None;
        }
        unsafe { core::ptr::write_bytes(address as *mut u8, 0, PAGE_SIZE) };
    }
    Some(USER_STACK_ADDRESS + USER_STACK_PAGES * PAGE_SIZE as u64)
}
