//! Catches Ctrl-C, three times, then gives up and lets it kill.
//!
//! The interesting part of this program is the four instructions at the bottom.
//! A signal handler is an ordinary function that the kernel arranged to be
//! called by writing a frame onto this program's own stack — so when it
//! returns, it returns *somewhere*, and that somewhere has to undo the
//! arrangement.
//!
//! `restorer` is that somewhere. Linux keeps the same stub in the vDSO, a page
//! the kernel maps into every process; here the program supplies it, which is
//! more honest for a kernel you are meant to read. Two instructions and a
//! syscall, and the interrupted code resumes with every register as it was.

#![no_std]
#![no_main]

const SYS_EXIT: u64 = 0;
const SYS_WRITE: u64 = 1;
const SYS_SIGNAL: u64 = 11;
const SYS_SIGRETURN: u64 = 12;
const STDOUT: u64 = 1;
const SIGINT: u64 = 2;

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

fn write(text: &str) {
    unsafe { syscall(SYS_WRITE, STDOUT, text.as_ptr() as u64, text.len() as u64) };
}

static mut CAUGHT: u64 = 0;

/// Called by nobody. The kernel put a frame on the stack that makes it look
/// like it was.
extern "C" fn on_interrupt(_signal: u64) {
    unsafe {
        CAUGHT += 1;
        let count = core::ptr::read_volatile(core::ptr::addr_of!(CAUGHT));
        match count {
            1 => write("\n  catcher: caught one. Two more and I stop catching.\n"),
            2 => write("\n  catcher: caught two.\n"),
            _ => write("\n  catcher: three. Next one is fatal -- handler removed.\n"),
        }
        if count >= 3 {
            // Back to the default action, which for SIGINT is death.
            syscall(SYS_SIGNAL, SIGINT, 0, 0);
        }
    }
}

/// Where a handler returns to.
///
/// The handler's `ret` lands here with the stack pointing exactly at the frame
/// the kernel saved, which is why `sigreturn` needs no argument: the answer is
/// already in RSP.
#[unsafe(naked)]
unsafe extern "C" fn restorer() {
    core::arch::naked_asm!(
        "mov rax, {number}",
        "int 0x80",
        // Never reached: the kernel resumes the interrupted code instead.
        "ud2",
        number = const SYS_SIGRETURN,
    )
}

#[no_mangle]
pub extern "C" fn _start() -> ! {
    unsafe {
        syscall(
            SYS_SIGNAL,
            SIGINT,
            on_interrupt as usize as u64,
            restorer as usize as u64,
        )
    };

    write("  catcher: looping. Press Ctrl-C.\n");

    let mut spins: u64 = 0;
    loop {
        // No syscalls in here at all: the only way this program can be
        // interrupted is the timer, which is the point.
        for _ in 0..20_000_000u64 {
            spins = spins.wrapping_add(unsafe { core::ptr::read_volatile(&spins) });
        }
        write("  catcher: still here\n");
    }
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    unsafe { syscall(SYS_EXIT, 1, 0, 0) };
    loop {}
}
