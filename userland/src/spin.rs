//! A program that never asks the kernel for anything.
//!
//! It says one thing, then loops for a long time making no syscalls at all,
//! then says another thing and exits. While it is looping there is nothing
//! cooperative about it: it does not yield, sleep, or call anything, and it
//! would run forever if nobody stopped it.
//!
//! That is the point. With ring 3 running interrupts disabled, this program
//! owns the machine until it decides otherwise. With them enabled, the timer
//! takes the CPU away on a schedule the program has no say in — and any kernel
//! task that has work to do keeps making progress underneath it.

#![no_std]
#![no_main]

const SYS_EXIT: u64 = 0;
const SYS_WRITE: u64 = 1;

#[inline(always)]
unsafe fn syscall(number: u64, arg0: u64, arg1: u64) -> u64 {
    let result: u64;
    unsafe {
        core::arch::asm!("int 0x80", inlateout("rax") number => result, in("rdi") arg0, in("rsi") arg1);
    }
    result
}

fn write(text: &str) {
    unsafe { syscall(SYS_WRITE, text.as_ptr() as u64, text.len() as u64) };
}

#[no_mangle]
pub extern "C" fn _start() -> ! {
    write("  spin:   looping with no syscalls at all. Nothing here yields.\n");

    let mut total: u64 = 0;
    for index in 0..400_000_000u64 {
        // read_volatile so the whole loop cannot be optimised into nothing.
        total = total.wrapping_add(unsafe { core::ptr::read_volatile(&index) });
    }

    if total == 0 {
        write("  spin:   (unreachable)\n");
    }
    write("  spin:   finished. Anything printed above me ran while I spun.\n");
    unsafe { syscall(SYS_EXIT, 0, 0) };
    loop {}
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {}
}
