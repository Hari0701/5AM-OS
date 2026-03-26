//! Recurses until the stack it was given is not big enough, and keeps going.
//!
//! The program was started with exactly one page of stack mapped. Everything
//! below that is a promise: an address range the kernel agreed to make real if
//! this program ever reached it. Each new frame that crosses a page boundary is
//! a fault the program never sees — the kernel allocates, maps, and re-runs the
//! instruction that failed.
//!
//! That is why asking for a large stack costs nothing until it is used, and why
//! a program can be given far more address space than the machine has memory.

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

fn write(text: &str) {
    unsafe { syscall(SYS_WRITE, STDOUT, text.as_ptr() as u64, text.len() as u64) };
}

/// Each call takes about a kilobyte, so a page is gone every four levels.
///
/// `read_volatile` on the local array keeps the compiler from noticing that
/// none of this is used and optimising the recursion into nothing.
#[inline(never)]
fn descend(depth: u64) -> u64 {
    let mut scratch = [0u8; 1024];
    // Volatile stores spread across the whole array, because otherwise the
    // compiler keeps only the bytes it can see being used and the frame ends up
    // a few words wide -- which is a perfectly good optimisation and completely
    // defeats the point of the program.
    unsafe {
        let base = scratch.as_mut_ptr();
        let mut offset = 0;
        while offset < 1024 {
            core::ptr::write_volatile(base.add(offset), depth as u8);
            offset += 64;
        }
    }

    if depth == 0 {
        return unsafe { core::ptr::read_volatile(scratch.as_ptr()) } as u64;
    }

    match depth {
        48 => write("  deep: 16 levels down\n"),
        32 => write("  deep: 32 levels down\n"),
        16 => write("  deep: 48 levels down\n"),
        _ => {}
    }

    let below = descend(depth - 1);
    below + unsafe { core::ptr::read_volatile(scratch.as_ptr()) } as u64
}

#[no_mangle]
pub extern "C" fn _start() -> ! {
    write("  deep: started with one page of stack. Recursing.\n");
    let total = descend(64);
    if total > 0 {
        write("  deep: came back up. Every frame below the first page was\n");
        write("        a page the kernel made real while I was running.\n");
    }
    unsafe { syscall(SYS_EXIT, 0, 0, 0) };
    loop {}
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    unsafe { syscall(SYS_EXIT, 1, 0, 0) };
    loop {}
}
