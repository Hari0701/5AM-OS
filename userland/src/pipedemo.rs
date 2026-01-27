//! Two processes and a pipe.
//!
//! Everything else these programs share, they share by accident of being forked
//! from one another — copy-on-write memory neither can write to without getting
//! a private copy, and an exit code the parent collects at the end. A pipe is
//! the first thing built to be shared on purpose.
//!
//! The shape is the one every shell uses. Make the pipe *before* forking, so
//! both processes inherit both ends. Then each closes the end it does not want,
//! and what is left is a one-way channel with a writer at one end and a reader
//! at the other.
//!
//! Closing the unused end is not tidiness. The reader learns there is no more
//! data only when *every* write end is gone, so a parent that keeps the write
//! end it inherited will wait forever for itself.

#![no_std]
#![no_main]

const SYS_EXIT: u64 = 0;
const SYS_WRITE: u64 = 1;
const STDOUT: u64 = 1;
const SYS_FORK: u64 = 3;
const SYS_WAIT: u64 = 5;
const SYS_PIPE: u64 = 6;
const SYS_READ: u64 = 7;
const SYS_CLOSE: u64 = 8;

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

fn write(fd: u64, text: &str) -> u64 {
    unsafe { syscall(SYS_WRITE, fd, text.as_ptr() as u64, text.len() as u64) }
}

fn say(text: &str) {
    write(STDOUT, text);
}

fn exit(code: u64) -> ! {
    unsafe { syscall(SYS_EXIT, code, 0, 0) };
    loop {}
}

static mut BUFFER: [u8; 128] = [0; 128];

#[no_mangle]
pub extern "C" fn _start() -> ! {
    // Two descriptors come back through this array, because there is no room
    // for two answers in one register.
    let mut ends: [u64; 2] = [0; 2];
    if unsafe { syscall(SYS_PIPE, ends.as_mut_ptr() as u64, 0, 0) } != 0 {
        say("  pipe: could not create one\n");
        exit(1);
    }
    let (read_end, write_end) = (ends[0], ends[1]);

    say("  pipe: made a pipe, now forking so both sides inherit it\n");

    if unsafe { syscall(SYS_FORK, 0, 0, 0) } == 0 {
        // Child: the writer. Close the end it will never use.
        unsafe { syscall(SYS_CLOSE, read_end, 0, 0) };
        say("  child:  closed the read end, writing into the pipe\n");
        write(write_end, "hello down the pipe");
        write(write_end, " -- and a second write");
        say("  child:  done writing, closing the write end\n");
        unsafe { syscall(SYS_CLOSE, write_end, 0, 0) };
        exit(0);
    }

    // Parent: the reader. Closing the write end is what makes end-of-file
    // possible at all -- while this process holds one, the pipe still has a
    // writer and a read on an empty pipe would block forever.
    unsafe { syscall(SYS_CLOSE, write_end, 0, 0) };
    say("  parent: closed the write end, reading until end of file\n");

    let mut total = 0usize;
    loop {
        let buffer = unsafe { core::ptr::addr_of_mut!(BUFFER) as *mut u8 };
        let count = unsafe { syscall(SYS_READ, read_end, buffer as u64, 64) };
        if count == 0 || count == u64::MAX {
            break;
        }
        say("  parent: read \"");
        unsafe { syscall(SYS_WRITE, STDOUT, buffer as u64, count) };
        say("\"\n");
        total += count as usize;
    }

    if total > 0 {
        say("  parent: end of file -- every write end is closed\n");
    } else {
        say("  parent: got nothing at all\n");
    }

    unsafe { syscall(SYS_CLOSE, read_end, 0, 0) };
    unsafe { syscall(SYS_WAIT, 0, 0, 0) };
    say("  parent: child collected. Two processes, one channel.\n");
    exit(0)
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    exit(1)
}
