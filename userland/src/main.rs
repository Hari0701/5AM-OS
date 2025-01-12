//! The first program in this project that is genuinely separate from the kernel.
//!
//! It is compiled on its own, linked on its own, and ends up as an ELF file the
//! kernel has to *parse* rather than a blob it copies. Nothing here can see a
//! kernel symbol, call a kernel function, or be inlined into one — the only
//! thing the two share is the number in `int 0x80` and the meaning of a few
//! registers.
//!
//! That is what an ABI is, and this is the smallest honest example of one.

#![no_std]
#![no_main]

const SYS_EXIT: u64 = 0;
const SYS_WRITE: u64 = 1;
const SYS_REPORT_CS: u64 = 2;

/// Ask the kernel for something.
///
/// The whole calling convention: number in RAX, arguments in RDI and RSI,
/// result back in RAX. The kernel's handler preserves everything else, so RAX
/// is the only register the compiler needs to be told about.
#[inline(always)]
unsafe fn syscall(number: u64, arg0: u64, arg1: u64) -> u64 {
    let result: u64;
    unsafe {
        core::arch::asm!(
            "int 0x80",
            inlateout("rax") number => result,
            in("rdi") arg0,
            in("rsi") arg1,
        );
    }
    result
}

fn write(text: &str) -> u64 {
    unsafe { syscall(SYS_WRITE, text.as_ptr() as u64, text.len() as u64) }
}

fn exit(code: u64) -> ! {
    unsafe { syscall(SYS_EXIT, code, 0) };
    // The kernel does not come back from exit. If it somehow did, stopping
    // here beats running into whatever follows.
    loop {}
}

/// Lives in `.bss`: space the file promises but does not contain.
///
/// This is the interesting part of a program header. The segment's `filesz` is
/// smaller than its `memsz`, and the difference is memory the loader must
/// provide as zeroes. Get that wrong and a program starts life with whatever
/// the previous owner of those frames left behind — which is both a bug and a
/// serious information leak.
static mut SCRATCH: [u8; 8192] = [0; 8192];

#[no_mangle]
pub extern "C" fn _start() -> ! {
    write("  hello from ring 3 -- a real ELF, loaded from its program headers\n");

    // Read our own privilege level rather than trusting anyone about it.
    let cs: u64;
    unsafe {
        core::arch::asm!("mov {}, cs", out(reg) cs, options(nomem, nostack, preserves_flags))
    };
    unsafe { syscall(SYS_REPORT_CS, cs, 0) };

    // Was .bss actually zeroed? Check every byte rather than a sample: the
    // failure this catches is a partially-cleared page, which a spot check
    // would happily miss.
    let scratch = unsafe { &*core::ptr::addr_of!(SCRATCH) };
    let clean = scratch.iter().all(|&byte| byte == 0);
    if clean {
        write("  .bss is all zeroes -- 8192 bytes the file never contained\n");
    } else {
        write("  .bss is NOT zeroed. The loader skipped memsz > filesz.\n");
    }

    // Prove the memory is real by using it, then reading it back.
    let scratch = unsafe { &mut *core::ptr::addr_of_mut!(SCRATCH) };
    scratch[0] = b'o';
    scratch[1] = b'k';
    if scratch[0] == b'o' && scratch[1] == b'k' {
        write("  ...and writable, so the segment is mapped read-write\n");
    }

    // Ask the kernel to read from its own address space. It must refuse: this
    // is the pointer check that makes the boundary mean something.
    unsafe { syscall(SYS_WRITE, 0x1000_0000_0000, 16) };

    exit(0);
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    write("  the user program panicked\n");
    exit(1)
}
