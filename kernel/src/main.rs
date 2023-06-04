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

mod gdt;
mod interrupts;
mod keyboard;
mod narrate;
mod serial;

use bootloader_api::BootInfo;
use core::panic::PanicInfo;

// Hand the bootloader the address of our entry point, type-checked.
//
// The macro generates an `extern "C"` symbol with the exact signature the
// bootloader expects. Getting this wrong by hand is a triple fault with no
// error message, which is the classic first day of OS development.
// (Plain comments, not doc comments: rustdoc cannot document a macro call.)
bootloader_api::entry_point!(kernel_main);

/// The first Rust code that runs in 5AM-OS.
///
/// By the time we arrive here the machine is already in a state that took the
/// bootloader real work to reach: 64-bit long mode, paging on, a stack under
/// us. `boot_info` is its handover note — where memory is, and what it did.
fn kernel_main(boot_info: &'static mut BootInfo) -> ! {
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
    narrate::step("gdt", "describing memory segments in our own words");
    unsafe { gdt::init() };

    // Handlers must be installed before interrupts are enabled, or the first
    // timer tick lands on an empty vector.
    narrate::step("idt", "installing fault handlers and remapping the PIC");
    unsafe { interrupts::init() };

    narrate::step("ps2 ", "waking the keyboard controller and draining its buffer");
    unsafe { keyboard::init() };

    narrate::step("sti", "enabling interrupts — the machine can now interrupt us");
    interrupts::enable();

    narrate::ready();

    halt_forever();
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
