//! fork, exec, wait — the three calls every shell has used to start every
//! program for fifty years.
//!
//! Each is nearly useless alone. `fork` gives you a copy of yourself, which is
//! rarely what you wanted. `exec` replaces you, so you could never start
//! something *and* carry on. `wait` needs somebody else to wait for. Together
//! they are how one process becomes two, one of the two becomes a different
//! program entirely, and the original finds out how it went.

#![no_std]
#![no_main]

const SYS_EXIT: u64 = 0;
const SYS_WRITE: u64 = 1;
/// File descriptor 1. Not a law -- just the slot the kernel filled in with the
/// console, and the one every program has agreed to mean "my output" since
/// before any of this existed.
const STDOUT: u64 = 1;
const SYS_FORK: u64 = 3;
const SYS_EXEC: u64 = 4;
const SYS_WAIT: u64 = 5;

#[inline(always)]
unsafe fn syscall(number: u64, arg0: u64, arg1: u64, arg2: u64) -> u64 {
    let result: u64;
    unsafe {
        core::arch::asm!(
            "int 0x80",
            inlateout("rax") number => result,
            in("rdi") arg0,
            in("rsi") arg1,
            in("rdx") arg2,
        );
    }
    result
}

fn write(text: &str) {
    unsafe { syscall(SYS_WRITE, STDOUT, text.as_ptr() as u64, text.len() as u64) };
}

fn exit(code: u64) -> ! {
    unsafe { syscall(SYS_EXIT, code, 0, 0) };
    loop {}
}

#[no_mangle]
pub extern "C" fn _start() -> ! {
    write("  spawner: about to fork\n");

    let child = unsafe { syscall(SYS_FORK, 0, 0, 0) };

    if child == 0 {
        // The child. It is still this program -- same code, same memory -- so
        // the first thing it does is stop being this program.
        write("  child:   I am the copy. Replacing myself with bye.elf\n");
        let name = "bye.elf";
        unsafe { syscall(SYS_EXEC, name.as_ptr() as u64, name.len() as u64, 0) };
        // Only reached if exec failed, because a successful exec has nowhere
        // to return to -- the code that called it no longer exists.
        write("  child:   exec failed, so I am still the old program\n");
        exit(1);
    }

    write("  spawner: forked a child, now waiting for it\n");
    let code = unsafe { syscall(SYS_WAIT, 0, 0, 0) };

    if code == 42 {
        write("  spawner: the child exited with 42, which is bye.elf's number\n");
        write("  spawner: fork + exec + wait, all three\n");
    } else {
        write("  spawner: wrong exit code back from wait\n");
    }

    exit(0)
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    exit(1)
}
