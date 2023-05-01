//! The teaching layer.
//!
//! Every other module does the work; this one explains it. The rule for this
//! file: never describe the machine in the abstract when the real value can be
//! read out of it. If we talk about the stack pointer, we print the actual
//! stack pointer.

use crate::println;
use bootloader_api::BootInfo;
use bootloader_api::info::MemoryRegionKind;
use core::arch::asm;

pub fn banner() {
    println!();
    println!("  +--------------------------------------------------+");
    println!("  |  5AM-OS                                          |");
    println!("  |  an operating system that explains itself        |");
    println!("  +--------------------------------------------------+");
    println!();
}

/// What happened before our first instruction.
pub fn what_just_happened(boot_info: &BootInfo) {
    println!("[boot] You are reading this from inside a kernel.");
    println!();
    println!("       Before this line ran, three things had to happen, and none");
    println!("       of them were done by code in this repository:");
    println!();
    println!("       1. Firmware (BIOS/UEFI) powered up the CPU in 16-bit real");
    println!("          mode -- the same mode an 8086 booted in, in 1978. In");
    println!("          that mode there is 1MB of addressable memory and no");
    println!("          memory protection whatsoever.");
    println!();
    println!("       2. The bootloader walked the CPU through 40 years of");
    println!("          history in a few milliseconds: real mode -> protected");
    println!("          mode (32-bit) -> long mode (64-bit). Each step needs a");
    println!("          descriptor table built and a control register flipped,");
    println!("          in a strict order. Get it wrong and the CPU triple");
    println!("          faults, which on real hardware means it reboots.");
    println!();
    println!("       3. It built page tables, turned on paging, and mapped the");
    println!("          kernel somewhere sane -- so the addresses we use are");
    println!("          already virtual, not physical.");
    println!();
    println!(
        "       Bootloader API version: {}.{}.{}",
        boot_info.api_version.version_major(),
        boot_info.api_version.version_minor(),
        boot_info.api_version.version_patch(),
    );
    println!();
}

/// Read live CPU state and show it. Nothing here is hardcoded.
pub fn where_are_we() {
    let (cs, rsp, cr0, cr3, cr4, rip) = read_cpu_state();

    println!("[cpu ] Where the machine actually is right now:");
    println!();
    println!("       RIP  = {rip:#018x}   <- the instruction that read this");
    println!("       RSP  = {rsp:#018x}   <- top of our stack");
    println!("       CS   = {cs:#06x}               <- code segment selector");
    println!();
    println!("       CR0  = {cr0:#018x}");
    println!(
        "              PE (bit 0)  = {}   protected mode on",
        bit(cr0, 0)
    );
    println!(
        "              PG (bit 31) = {}   paging on",
        bit(cr0, 31)
    );
    println!("       CR3  = {cr3:#018x}");
    println!("              ^ physical address of the level-4 page table.");
    println!("                Every memory access you make is translated");
    println!("                through the tree rooted at that address.");
    println!("       CR4  = {cr4:#018x}");
    println!(
        "              PAE (bit 5) = {}   64-bit paging requires this",
        bit(cr4, 5)
    );
    println!();
    println!("       CS = {cs:#06x} means ring {}. Ring 0 is full control of the",
        cs & 0b11);
    println!("       machine: every instruction, every register, all memory.");
    println!("       Your applications normally run in ring 3, where most of");
    println!("       that is forbidden. There is nothing above us to say no.");
    println!();
}

/// Print the physical memory map the firmware handed us.
pub fn memory_map(boot_info: &BootInfo) {
    println!("[mem ] The firmware's map of physical RAM:");
    println!();
    println!("       {:<20} {:<20} {:>12}  {}", "START", "END", "SIZE", "KIND");

    let mut usable = 0u64;
    let mut total = 0u64;
    let mut shown = 0;

    for region in boot_info.memory_regions.iter() {
        let size = region.end - region.start;
        total += size;
        if region.kind == MemoryRegionKind::Usable {
            usable += size;
        }

        // Long maps are mostly noise; the shape is the lesson.
        if shown < 12 {
            println!(
                "       {:#018x}  {:#018x}  {:>10} KiB  {:?}",
                region.start,
                region.end,
                size / 1024,
                region.kind
            );
            shown += 1;
        }
    }

    let hidden = boot_info.memory_regions.len().saturating_sub(shown);
    if hidden > 0 {
        println!("       ... and {hidden} more regions");
    }

    println!();
    println!(
        "       {} regions, {} MiB total, {} MiB usable.",
        boot_info.memory_regions.len(),
        total / 1024 / 1024,
        usable / 1024 / 1024
    );
    println!();
    println!("       Notice it is not one clean block. Physical memory is full");
    println!("       of holes: firmware code, ACPI tables, memory-mapped");
    println!("       devices. A kernel cannot assume RAM is contiguous, which");
    println!("       is the entire reason page tables exist -- they let us");
    println!("       present a tidy virtual space on top of this mess.");
    println!();
}

fn bit(value: u64, n: u32) -> u8 {
    ((value >> n) & 1) as u8
}

/// Read the control registers and friends.
///
/// These are readable only in ring 0 — one more thing that is unremarkable
/// here and impossible in an application.
fn read_cpu_state() -> (u16, u64, u64, u64, u64, u64) {
    let (cs, rsp, cr0, cr3, cr4, rip): (u16, u64, u64, u64, u64, u64);
    unsafe {
        asm!(
            "mov {cs:x}, cs",
            "mov {rsp}, rsp",
            "mov {cr0}, cr0",
            "mov {cr3}, cr3",
            "mov {cr4}, cr4",
            // RIP is not readable directly; lea against a RIP-relative zero
            // offset gives us the address of the next instruction.
            "lea {rip}, [rip + 0]",
            cs = out(reg) cs,
            rsp = out(reg) rsp,
            cr0 = out(reg) cr0,
            cr3 = out(reg) cr3,
            cr4 = out(reg) cr4,
            rip = out(reg) rip,
            options(nomem, nostack, preserves_flags),
        );
    }
    (cs, rsp, cr0, cr3, cr4, rip)
}

/// One line per initialisation step, printed as it happens.
pub fn step(tag: &str, what: &str) {
    println!("[{tag:<4}] {what}");
}

pub fn ready() {
    println!();
    println!("[ok  ] The kernel is now interrupt-driven. Nothing below this");
    println!("       point runs unless hardware asks for it, or you type.");
    println!();
}
