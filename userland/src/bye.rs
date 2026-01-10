//! A program whose only job is to be replaced *into* and to exit with a
//! distinctive number, so a waiting parent can prove it collected the right one.

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
    write("  bye:    I am a different program now. Exiting with 42.\n");
    unsafe { syscall(SYS_EXIT, 42, 0) };
    loop {}
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {}
}
