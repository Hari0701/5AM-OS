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

extern crate alloc;

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

/// Start `sh.elf` as the first user process, and wait for it.
///
/// Everything a running system does descends from one program the kernel
/// starts by hand. Here that is the shell, read off the same filesystem as
/// anything else, given no privilege the other programs lack, and waited for
/// exactly as any parent waits for any child.
pub fn start_init() {
    let Ok(volume) = crate::fat::mount() else {
        println!("[init] no disk -- falling back to the kernel shell");
        println!();
        return;
    };
    let program = match volume
        .find("sh.elf")
        .and_then(|entry| volume.read_file(&entry))
    {
        Ok(data) => data,
        Err(error) => {
            println!("[init] sh.elf: {error} -- falling back to the kernel shell");
            println!();
            return;
        }
    };

    println!("[init] starting sh.elf as the first user process");
    run_bytes_named(&program, "init", false);
    println!();
    println!("[init] the first process exited. The kernel shell is the fallback;");
    println!("       a real machine would call this a panic.");
    println!();
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
    run_bytes_named(program, "prog", true)
}

pub fn run_bytes_named(program: &[u8], name: &str, verbose: bool) {
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
    if verbose {
        println!("  Private address space at {:#x}.", space.root());
        println!("  Reading its program headers:");
    }

    let loaded = match unsafe { elf::load(program, verbose) } {
        Ok(loaded) => loaded,
        Err(error) => {
            println!("  refusing to run it: {error}");
            unsafe { memory::activate_root(kernel_root) };
            unsafe { space.destroy() };
            return;
        }
    };

    if verbose {
        println!(
            "  {} segments loaded, {} bytes of fresh zeroed memory.",
            loaded.segments, loaded.bytes_zeroed
        );
    }

    let Some(stack_top) = map_stack() else {
        println!("  could not map the user stack");
        unsafe { memory::activate_root(kernel_root) };
        unsafe { space.destroy() };
        return;
    };
    if verbose {
        println!(
            "  stack     {USER_STACK_ADDRESS:#010x}  {} bytes, rw-, mapped by the kernel",
            USER_STACK_PAGES * PAGE_SIZE as u64
        );
    }

    let before = syscall::count();
    let root = space.root();
    core::mem::forget(space);

    // Spawn it as a task rather than diving into ring 3 from here.
    //
    // This is what unifying the schedulers bought. The shell used to *become*
    // the user program -- one call that did not return until the program
    // exited -- so nothing else could happen in between and the two of them
    // shared a single kernel stack. Now a user program is a task with an
    // address space, the scheduler switches to it like anything else, and the
    // shell simply waits for it the same way it waits for a kernel task.
    let arguments = [alloc::string::String::from(name)];
    let (stack, argc, argv) = place_arguments(stack_top, &arguments);
    let id = match crate::task::spawn_user(name, loaded.entry, stack, root, Some(0), argc, argv) {
        Ok(id) => id,
        Err(error) => {
            println!("  could not start it: {error}");
            unsafe { memory::activate_root(kernel_root) };
            unsafe { memory::AddressSpace::adopt(root).destroy() };
            return;
        }
    };

    // Back to the kernel's own address space. The scheduler will install the
    // program's when it runs it.
    unsafe { memory::activate_root(kernel_root) };

    if verbose {
        println!();
        println!("  Task {id} is now running it in ring 3.");
        println!();
    }

    let code = crate::task::wait_any_child(0);

    // Reclaim whatever the program and any children it forked left behind.
    let mut freed = 0;
    for root in crate::task::orphan_address_spaces() {
        freed += unsafe { memory::AddressSpace::adopt(root).destroy() };
    }
    crate::task::reap_finished();

    if verbose {
        println!();
        match code {
            Some(code) => println!("  Task {id} exited with {code}."),
            None => println!("  Task {id} is gone."),
        }
        println!("  {} syscalls, {freed} frames returned.", syscall::count() - before);
    }
}

/// Copy argument strings onto the top of a user stack.
///
/// The strings themselves have to *be* in the new address space -- the shell's
/// copies are in memory this program will never see. So they are carried across
/// in kernel memory and written out here, once the new space is active.
///
/// Returns the stack pointer to start with, the count, and the address of the
/// pointer array. What comes back is the shape every C program has expected
/// since 1972: `argc`, and an array of pointers ending in null.
pub fn place_arguments(stack_top: u64, arguments: &[alloc::string::String]) -> (u64, u64, u64) {
    let mut cursor = stack_top;
    let mut addresses = alloc::vec::Vec::with_capacity(arguments.len());

    for argument in arguments {
        cursor -= argument.len() as u64 + 1;
        cursor &= !0x7;
        unsafe {
            core::ptr::copy_nonoverlapping(argument.as_ptr(), cursor as *mut u8, argument.len());
            // The terminating zero is not decoration: it is the only thing that
            // tells the program where the string ends.
            *((cursor + argument.len() as u64) as *mut u8) = 0;
        }
        addresses.push(cursor);
    }

    cursor -= (addresses.len() as u64 + 1) * 8;
    cursor &= !0xF;
    let argv = cursor;
    for (index, address) in addresses.iter().enumerate() {
        unsafe { *((argv + (index * 8) as u64) as *mut u64) = *address };
    }
    // The null that ends the array, so a program can walk it without being told
    // how long it is.
    unsafe { *((argv + (addresses.len() * 8) as u64) as *mut u64) = 0 };

    let stack = (argv - 64) & !0xF;
    (stack - 8, addresses.len() as u64, argv)
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
