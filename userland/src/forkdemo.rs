//! One call, two returns — and a test of whether the copy really is private.
//!
//! `fork` duplicates this process. Both copies continue from the same
//! instruction with the same registers and the same memory contents; the only
//! difference in the entire universe is the value in one register. That is how
//! a program tells which of the two it is, and it is the whole interface.
//!
//! The interesting part is what the memory does. The kernel does not copy any
//! of it at fork time — both processes point at the same physical pages, marked
//! read-only. The first write from either side takes a fault the program never
//! sees, gets a private copy, and carries on.
//!
//! This program proves that from the outside: the child reads the shared
//! variable *before* writing to it, and reports whether it can see the value
//! the parent stored after the fork. If it can, the two are sharing memory they
//! should not be.

#![no_std]
#![no_main]

const SYS_EXIT: u64 = 0;
const SYS_WRITE: u64 = 1;
const SYS_FORK: u64 = 3;

#[inline(always)]
unsafe fn syscall(number: u64, arg0: u64, arg1: u64) -> u64 {
    let result: u64;
    unsafe {
        core::arch::asm!(
            "int 0x80",
            inlateout("rax") number => result,
            in("rdi") arg0,
            in("rsi") arg1,
        );
    }
    result
}

fn write(text: &str) -> u64 {
    unsafe { syscall(SYS_WRITE, text.as_ptr() as u64, text.len() as u64) }
}

fn exit(code: u64) -> ! {
    unsafe { syscall(SYS_EXIT, code, 0) };
    loop {}
}

/// Lives in `.data`, so it is a writable page both processes will share until
/// one of them writes to it.
static mut SHARED: u64 = 100;

#[no_mangle]
pub extern "C" fn _start() -> ! {
    write("  before fork: one process, SHARED = 100\n");

    let result = unsafe { syscall(SYS_FORK, 0, 0) };

    // Read before writing. A read of a copy-on-write page is allowed and
    // costs nothing -- it is only the write that has to be intercepted.
    let seen = unsafe { core::ptr::read_volatile(core::ptr::addr_of!(SHARED)) };

    if result == 0 {
        // The child. It runs after the parent has already stored 9.
        if seen == 100 {
            write("  child:  SHARED is still 100 -- a private copy, as it should be\n");
        } else {
            write("  child:  SHARED changed under me -- COPY ON WRITE IS BROKEN\n");
        }
        unsafe { core::ptr::write_volatile(core::ptr::addr_of_mut!(SHARED), 7) };
        write("  child:  wrote 7 to my own copy, exiting\n");
    } else {
        write("  parent: fork returned non-zero, so I am the original\n");
        unsafe { core::ptr::write_volatile(core::ptr::addr_of_mut!(SHARED), 9) };
        write("  parent: wrote 9 to my own copy, exiting\n");
    }

    exit(0)
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    write("  the fork demo panicked\n");
    exit(1)
}
