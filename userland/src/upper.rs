//! Reads standard input until end of file and writes it back in capitals.
//!
//! A filter: it knows nothing about who is upstream or downstream, only that
//! bytes arrive on descriptor 0 and leave on descriptor 1. Every Unix tool ever
//! written works this way, and it is why they combine at all.
//!
//! Note what ends it: a read returning zero. That is not a signal or a marker
//! in the data -- it is the kernel reporting that every write end of the pipe
//! has been closed.

#![no_std]
#![no_main]

const SYS_EXIT: u64 = 0;
const SYS_WRITE: u64 = 1;
const SYS_READ: u64 = 7;
const STDIN: u64 = 0;
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

static mut BUFFER: [u8; 128] = [0; 128];

#[no_mangle]
pub extern "C" fn _start() -> ! {
    loop {
        let buffer = unsafe { core::ptr::addr_of_mut!(BUFFER) as *mut u8 };
        let count = unsafe { syscall(SYS_READ, STDIN, buffer as u64, 128) };
        if count == 0 || count == u64::MAX {
            break;
        }
        for index in 0..count as usize {
            unsafe {
                let byte = *buffer.add(index);
                *buffer.add(index) = byte.to_ascii_uppercase();
            }
        }
        unsafe { syscall(SYS_WRITE, STDOUT, buffer as u64, count) };
    }
    unsafe { syscall(SYS_EXIT, 0, 0, 0) };
    loop {}
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {}
}
