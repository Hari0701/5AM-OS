//! Prints its arguments, which is the smallest possible proof that a program
//! can be told what to do rather than only what to be.
//!
//! `argc` arrives in RDI and `argv` in RSI: a count, and an array of pointers
//! ending in null. Real ELF hands these to `_start` on the stack instead;
//! registers are simpler to see and just as honest, so long as both sides agree
//! — which is all a calling convention has ever been.

#![no_std]
#![no_main]

const SYS_EXIT: u64 = 0;
const SYS_WRITE: u64 = 1;
const STDOUT: u64 = 1;

#[inline(always)]
unsafe fn syscall(number: u64, arg0: u64, arg1: u64, arg2: u64) -> u64 {
    let result: u64;
    unsafe {
        core::arch::asm!(
            "int 0x80",
            inlateout("rax") number => result,
            in("rdi") arg0, in("rsi") arg1, in("rdx") arg2,
        );
    }
    result
}

fn write(bytes: &[u8]) {
    unsafe { syscall(SYS_WRITE, STDOUT, bytes.as_ptr() as u64, bytes.len() as u64) };
}

/// Walk to the terminating zero. Nothing records the length; the convention is
/// that the string ends when it says it does.
unsafe fn length_of(pointer: *const u8) -> usize {
    let mut length = 0;
    while unsafe { *pointer.add(length) } != 0 && length < 256 {
        length += 1;
    }
    length
}

#[no_mangle]
pub extern "C" fn _start(argc: u64, argv: *const *const u8) -> ! {
    // Skip argv[0]: it is the program's own name, which is how a program can
    // behave differently depending on what it was called.
    for index in 1..argc as usize {
        let pointer = unsafe { *argv.add(index) };
        if pointer.is_null() {
            break;
        }
        let length = unsafe { length_of(pointer) };
        write(unsafe { core::slice::from_raw_parts(pointer, length) });
        if index + 1 < argc as usize {
            write(b" ");
        }
    }
    write(b"\n");
    unsafe { syscall(SYS_EXIT, 0, 0, 0) };
    loop {}
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {}
}
