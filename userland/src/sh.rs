//! A shell, in ring 3, with no privileges any other program lacks.
//!
//! The one built into this kernel was always in the wrong place. It could read
//! the keyboard because it *was* the kernel; it started programs by calling a
//! function; and it could not have been replaced without rebuilding the machine.
//! None of that is how a shell works anywhere, and all of it hid the thing a
//! shell is actually for.
//!
//! Because a shell is not a special program. It is the smallest possible
//! demonstration that `fork`, `exec`, `wait` and `pipe` are enough — three
//! system calls to start a program and one to connect two of them, and every
//! command line you have ever typed is those four in a loop.
//!
//! ## What each call is for
//!
//! `fork` makes a second process, because the shell must survive the command it
//! runs. `exec` lets that copy stop being a shell and become the program. `wait`
//! lets the original find out when it finished. And `pipe`, with `dup2`, puts
//! one program's output where another's input should be — without either program
//! knowing it happened, which is exactly why they can be written independently
//! and still compose.

#![no_std]
#![no_main]

const SYS_EXIT: u64 = 0;
const SYS_WRITE: u64 = 1;
const SYS_FORK: u64 = 3;
const SYS_EXEC: u64 = 4;
const SYS_WAIT: u64 = 5;
const SYS_PIPE: u64 = 6;
const SYS_READ: u64 = 7;
const SYS_CLOSE: u64 = 8;
const SYS_DUP2: u64 = 9;

const STDIN: u64 = 0;
const STDOUT: u64 = 1;

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

fn write(fd: u64, bytes: &[u8]) -> u64 {
    unsafe { syscall(SYS_WRITE, fd, bytes.as_ptr() as u64, bytes.len() as u64) }
}

fn say(text: &str) {
    write(STDOUT, text.as_bytes());
}

fn exit(code: u64) -> ! {
    unsafe { syscall(SYS_EXIT, code, 0, 0) };
    loop {}
}

static mut LINE: [u8; 128] = [0; 128];
static mut INPUT: [u8; 32] = [0; 32];

/// Read one line, echoing as it goes.
///
/// The echo is the shell's job, not the kernel's. Nothing has been printed by
/// the time these bytes arrive — which is why a password prompt can simply
/// decline to echo, using the same interface as everything else.
fn read_line() -> usize {
    let mut length = 0usize;

    loop {
        let buffer = unsafe { core::ptr::addr_of_mut!(INPUT) as *mut u8 };
        let count = unsafe { syscall(SYS_READ, STDIN, buffer as u64, 32) };
        if count == 0 || count == u64::MAX {
            return length;
        }

        for index in 0..count as usize {
            let byte = unsafe { *buffer.add(index) };
            match byte {
                b'\n' | b'\r' => {
                    say("\n");
                    return length;
                }
                0x08 | 0x7F => {
                    if length > 0 {
                        length -= 1;
                        // Back up, cover the character, back up again. A
                        // terminal has no notion of deleting.
                        say("\x08 \x08");
                    }
                }
                0x20..=0x7E => {
                    if length < 128 {
                        unsafe { LINE[length] = byte };
                        length += 1;
                        write(STDOUT, &[byte]);
                    }
                }
                _ => {}
            }
        }
    }
}

fn trim(bytes: &[u8]) -> &[u8] {
    let mut start = 0;
    let mut end = bytes.len();
    while start < end && bytes[start] == b' ' {
        start += 1;
    }
    while end > start && bytes[end - 1] == b' ' {
        end -= 1;
    }
    &bytes[start..end]
}

fn equals(bytes: &[u8], text: &str) -> bool {
    bytes == text.as_bytes()
}

/// Replace this process with `name`. Only returns if that failed.
fn exec(name: &[u8]) {
    unsafe { syscall(SYS_EXEC, name.as_ptr() as u64, name.len() as u64, 0) };
}

/// Run one command and wait for it.
fn run(command: &[u8]) {
    if unsafe { syscall(SYS_FORK, 0, 0, 0) } == 0 {
        exec(command);
        say("  sh: no such program\n");
        exit(127);
    }
    unsafe { syscall(SYS_WAIT, 0, 0, 0) };
}

/// Run `left | right`: the classic, and the reason all of this exists.
///
/// Both children are forked before either is waited for, so they run at the
/// same time and the pipe carries data between them rather than buffering an
/// entire program's output. The shell closes both ends itself afterwards --
/// while it holds the write end, the reader can never see end of file.
fn run_pipeline(left: &[u8], right: &[u8]) {
    let mut ends: [u64; 2] = [0; 2];
    if unsafe { syscall(SYS_PIPE, ends.as_mut_ptr() as u64, 0, 0) } != 0 {
        say("  sh: could not create a pipe\n");
        return;
    }
    let (read_end, write_end) = (ends[0], ends[1]);

    // Left: its output becomes the pipe.
    if unsafe { syscall(SYS_FORK, 0, 0, 0) } == 0 {
        unsafe {
            syscall(SYS_DUP2, write_end, STDOUT, 0);
            syscall(SYS_CLOSE, read_end, 0, 0);
            syscall(SYS_CLOSE, write_end, 0, 0);
        }
        exec(left);
        exit(127);
    }

    // Right: its input becomes the pipe.
    if unsafe { syscall(SYS_FORK, 0, 0, 0) } == 0 {
        unsafe {
            syscall(SYS_DUP2, read_end, STDIN, 0);
            syscall(SYS_CLOSE, read_end, 0, 0);
            syscall(SYS_CLOSE, write_end, 0, 0);
        }
        exec(right);
        exit(127);
    }

    // The shell wants neither end. Holding the write end would keep the reader
    // waiting for a writer that is only ever going to be this process.
    unsafe {
        syscall(SYS_CLOSE, read_end, 0, 0);
        syscall(SYS_CLOSE, write_end, 0, 0);
        syscall(SYS_WAIT, 0, 0, 0);
        syscall(SYS_WAIT, 0, 0, 0);
    }
}

#[no_mangle]
pub extern "C" fn _start() -> ! {
    say("\n  A shell, running in ring 3 like anything else.\n");
    say("  Try:  hello.elf        bye.elf        greet.elf | upper.elf\n");
    say("  `exit` gives the machine back to the kernel shell.\n\n");

    loop {
        say("$ ");
        let length = read_line();
        let line = unsafe { &LINE[..length] };
        let line = trim(line);

        if line.is_empty() {
            continue;
        }
        if equals(line, "exit") {
            say("  sh: goodbye\n");
            exit(0);
        }

        // Split on the first pipe, if there is one.
        match line.iter().position(|&b| b == b'|') {
            Some(bar) => {
                let left = trim(&line[..bar]);
                let right = trim(&line[bar + 1..]);
                if left.is_empty() || right.is_empty() {
                    say("  sh: a pipe needs a program on both sides\n");
                } else {
                    run_pipeline(left, right);
                }
            }
            None => run(line),
        }
    }
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    exit(1)
}
