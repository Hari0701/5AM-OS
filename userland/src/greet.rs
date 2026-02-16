//! Writes one line to standard output and exits.
//!
//! It has no idea whether that output is a console or a pipe, and that is the
//! entire point: the shell decided before this program started running, and
//! nothing in here had to be told.

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

#[no_mangle]
pub extern "C" fn _start() -> ! {
    let text = "hello from one side of a pipe\n";
    unsafe { syscall(SYS_WRITE, STDOUT, text.as_ptr() as u64, text.len() as u64) };
    unsafe { syscall(SYS_EXIT, 0, 0, 0) };
    loop {}
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {}
}
