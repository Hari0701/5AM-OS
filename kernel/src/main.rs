//! 5AM-OS — a kernel that explains itself.
//!
//! The goal of this project is not to be a useful operating system. It is to be
//! a *legible* one: every stage of boot says what it is doing, why that step is
//! necessary, and what the machine looked like before and after.
//!
//! Read this file top to bottom and you have read the entire life of the kernel
//! so far.

#![no_std] // No standard library: std assumes an OS underneath. We are the OS.
#![no_main] // No `main`: there is no runtime to call it. The bootloader jumps
            // straight to the symbol named by entry_point! below.
#![feature(abi_x86_interrupt)] // Lets us write interrupt handlers as plain fns.

mod ai;
mod ata;
extern crate alloc;

mod elf;
mod fat;
mod font;
mod fpu;
mod framebuffer;
mod gdt;
mod heap;
mod interrupts;
mod llm;
mod memory;
mod keyboard;
mod narrate;
mod oracle;
mod pipe;
mod selftest;
mod serial;
mod shell;
mod smp;
mod signal;
mod sync;
mod swap;
mod syscall;
mod task;
mod user;

use bootloader_api::BootInfo;
use core::panic::PanicInfo;

// Hand the bootloader the address of our entry point, type-checked.
//
// The macro generates an `extern "C"` symbol with the exact signature the
// bootloader expects. Getting this wrong by hand is a triple fault with no
// error message, which is the classic first day of OS development.
// (Plain comments, not doc comments: rustdoc cannot document a macro call.)
/// Ask the bootloader to map all of physical memory before we start.
///
/// Without this, the kernel cannot read its own page tables: entries hold
/// physical addresses, and there would be no virtual address that reaches them.
/// See the module comment in memory.rs — this one line is what makes paging
/// implementable at all.
pub static BOOTLOADER_CONFIG: bootloader_api::BootloaderConfig = {
    let mut config = bootloader_api::BootloaderConfig::new_default();
    config.mappings.physical_memory =
        Some(bootloader_api::config::Mapping::Dynamic);
    config
};

bootloader_api::entry_point!(kernel_main, config = &BOOTLOADER_CONFIG);

/// The first Rust code that runs in 5AM-OS.
///
/// By the time we arrive here the machine is already in a state that took the
/// bootloader real work to reach: 64-bit long mode, paging on, a stack under
/// us. `boot_info` is its handover note — where memory is, and what it did.
fn kernel_main(boot_info: &'static mut BootInfo) -> ! {
    // FIRST. Before the serial port, before a single println.
    //
    // This kernel is compiled with SSE enabled, and LLVM does not reserve XMM
    // registers for float math — it uses them for ordinary struct and memory
    // copies too. So SSE instructions appear in code that has nothing to do
    // with arithmetic, including the code that would print a complaint.
    //
    // Enable it later and the machine dies silently mid-boot, which is exactly
    // what happened the first time this was written.
    unsafe { fpu::init() };

    // Claim the display before anything else touches boot_info. `take()` needs
    // the mutable reference, and everything after this line only has a shared
    // one -- so this has to happen here or not at all.
    if let Some(buffer) = boot_info.framebuffer.take() {
        let info = buffer.info();
        unsafe { framebuffer::init(buffer.into_buffer(), info) };
    }

    // Downgrade to a shared reference: nothing mutates the boot info again.
    let boot_info: &'static BootInfo = boot_info;
    // SAFETY: nothing else has touched the UART, and we are single-threaded
    // with interrupts still off.
    unsafe {
        let console = &mut *core::ptr::addr_of_mut!(serial::CONSOLE);
        console.init();
    }

    narrate::banner();
    narrate::what_just_happened(boot_info);
    narrate::where_are_we();
    narrate::memory_map(boot_info);

    // Order matters here, and each step depends on the one before it.
    //
    // The GDT must exist before the IDT, because every IDT entry names a code
    // segment selector from the GDT — and the double fault entry names an IST
    // slot that lives in the TSS, which the GDT points at.
    narrate::step("fpu ", "SSE already enabled -- it had to be, to get this far");
    narrate::step("gdt", "describing memory segments in our own words");
    unsafe { gdt::init() };

    // Handlers must be installed before interrupts are enabled, or the first
    // timer tick lands on an empty vector.
    narrate::step("idt", "installing fault handlers and remapping the PIC");
    unsafe { interrupts::init() };

    narrate::step("com2", "opening the AI bridge channel on the second serial port");
    unsafe { ai::init() };

    narrate::step("ps2 ", "waking the keyboard controller and draining its buffer");
    unsafe { keyboard::init() };

    // Let the serial console be an input device too, not just an output one.
    // This is what makes the OS usable in a plain terminal -- and over SSH,
    // and on hardware with no keyboard attached.
    narrate::step("com1", "accepting input on the serial console as well");
    unsafe {
        serial::drain_console();
        let console = &mut *core::ptr::addr_of_mut!(serial::CONSOLE);
        console.enable_receive_interrupt();
    }

    // Memory management, in the only order that works: know which frames are
    // free, then map some of them, then hand out pieces of what you mapped.
    match boot_info.physical_memory_offset.into_option() {
        Some(offset) => {
            narrate::step("mem ", "building the physical frame allocator");
            unsafe { memory::init(&boot_info.memory_regions, offset) };

            // Ask the CPU to honour the no-execute bit before anything sets it.
            // Until this succeeds, a page marked non-executable is executable
            // anyway and nothing says so.
            let nx = unsafe { memory::enable_no_execute() };
            unsafe { memory::set_no_execute_active(nx) };
            if nx {
                println!("[mem ] no-execute enabled (EFER.NXE)");
            } else {
                println!("[mem ] this CPU has no NX bit; every page stays executable");
            }

            narrate::step("heap", "mapping a heap -- Vec and String work after this line");
            match unsafe { heap::init() } {
                Ok(()) => {}
                Err(reason) => println!("[heap] failed: {reason}"),
            }
        }
        None => println!("[mem ] no physical memory mapping -- allocator disabled"),
    }

    // Hand the ramdisk to the model loader. The bootloader has already placed
    // it in memory; there is no filesystem involved and nothing is copied.
    match (boot_info.ramdisk_addr.into_option(), boot_info.ramdisk_len) {
        (Some(addr), len) if len > 0 => {
            narrate::step("llm ", "parsing the neural network out of the ramdisk");
            unsafe { llm::init(addr as *const u8, len as usize) };
        }
        _ => narrate::step("llm ", "no ramdisk -- booting without the model"),
    }

    narrate::step("task", "registering the shell as task 0 -- preemption on");
    task::init();

    narrate::step("sti", "enabling interrupts — the machine can now interrupt us");
    interrupts::enable();

    narrate::ready();

    // Hand the memory map to the shell, then never return.
    shell::set_boot_info(boot_info);
    shell::run();
}

/// Stop the CPU in the cheapest way available.
///
/// `hlt` parks the core until the next interrupt, drawing almost no power. The
/// loop is because interrupts *do* wake it — the CPU would otherwise wander
/// past this point into whatever bytes follow.
pub fn halt_forever() -> ! {
    loop {
        // SAFETY: hlt is always safe to execute in ring 0.
        unsafe { core::arch::asm!("hlt", options(nomem, nostack, preserves_flags)) };
    }
}

/// Where Rust sends every `panic!`, failed assert, and array bounds check.
///
/// In a normal program this would unwind the stack and print a backtrace. Here
/// there is no unwinder and no process to exit, so this function must never
/// return — hence `-> !`.
#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    println!("\n=== 5AM-OS PANIC ===");
    println!("{info}");
    println!("The kernel cannot continue. Halting.");
    halt_forever();
}
