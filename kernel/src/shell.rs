//! The shell — where 5AM-OS stops being a log and starts being a system.
//!
//! Every command here reads the *live* machine. `explain gdt` does not print a
//! description of what a GDT is; it asks the CPU where its GDT is, walks it,
//! and decodes the bytes that are actually there. If you change the kernel, the
//! explanation changes with it, because there is nothing to keep in sync.

use crate::keyboard::{self, Key};
use crate::{gdt, interrupts, print, println};
use bootloader_api::BootInfo;
use bootloader_api::info::MemoryRegionKind;
use core::arch::asm;

const MAX_LINE: usize = 128;

/// Stashed at boot so commands can reach the memory map.
static mut BOOT_INFO: Option<&'static BootInfo> = None;

pub fn set_boot_info(info: &'static BootInfo) {
    unsafe { BOOT_INFO = Some(info) };
}

/// The read-eval-print loop. Never returns.
pub fn run() -> ! {
    let mut line = [0u8; MAX_LINE];
    let mut len = 0usize;

    banner();
    prompt();

    loop {
        match keyboard::read_key() {
            Some(Key::Char(c)) => {
                if len < MAX_LINE {
                    line[len] = c as u8;
                    len += 1;
                    print!("{c}");
                }
            }
            Some(Key::Backspace) => {
                if len > 0 {
                    len -= 1;
                    // Move back, overwrite with a space, move back again —
                    // a terminal has no concept of "delete".
                    print!("\x08 \x08");
                }
            }
            Some(Key::Enter) => {
                println!();
                let command = core::str::from_utf8(&line[..len]).unwrap_or("");
                execute(command.trim());
                len = 0;
                prompt();
            }
            None => {
                // Nothing typed. Park the CPU until the next interrupt rather
                // than spinning — this is the difference between a laptop with
                // a cool fan and one without.
                unsafe { asm!("hlt", options(nomem, nostack)) };
            }
        }
    }
}

fn banner() {
    println!();
    println!("5AM-OS shell. Everything below reads the live machine.");
    println!("Type `help`.");
    println!();
}

fn prompt() {
    print!("5am> ");
}

fn execute(command: &str) {
    let (verb, rest) = split(command);
    match verb {
        "" => {}
        "help" => help(),
        "regs" => regs(),
        "gdt" => dump_gdt(),
        "idt" => dump_idt(),
        "mem" => mem(),
        "uptime" => uptime(),
        "clear" => print!("\x1b[2J\x1b[H"),
        other => {
            println!("unknown command: {other}");
            println!("try `help`");
        }
    }
}

fn split(input: &str) -> (&str, &str) {
    match input.find(' ') {
        Some(index) => (&input[..index], input[index + 1..].trim()),
        None => (input, ""),
    }
}

fn help() {
    println!("commands:");
    println!("  help              this list");
    println!("  regs              live control registers");
    println!("  gdt               decode every GDT entry");
    println!("  idt               which interrupt vectors are wired up");
    println!("  mem               physical memory map from the firmware");
    println!("  uptime            timer ticks since boot");
    println!("  clear             clear the screen");
}

fn regs() {
    let (cr0, cr2, cr3, cr4, rsp, rflags): (u64, u64, u64, u64, u64, u64);
    unsafe {
        asm!(
            "mov {cr0}, cr0",
            "mov {cr2}, cr2",
            "mov {cr3}, cr3",
            "mov {cr4}, cr4",
            "mov {rsp}, rsp",
            "pushfq",
            "pop {rflags}",
            cr0 = out(reg) cr0,
            cr2 = out(reg) cr2,
            cr3 = out(reg) cr3,
            cr4 = out(reg) cr4,
            rsp = out(reg) rsp,
            rflags = out(reg) rflags,
            options(preserves_flags),
        );
    }
    println!("  CR0    {cr0:#018x}   PE={} PG={}", cr0 & 1, (cr0 >> 31) & 1);
    println!("  CR2    {cr2:#018x}   last faulting address");
    println!("  CR3    {cr3:#018x}   page table root");
    println!("  CR4    {cr4:#018x}   PAE={}", (cr4 >> 5) & 1);
    println!("  RSP    {rsp:#018x}");
    println!("  RFLAGS {rflags:#018x}   IF={}", (rflags >> 9) & 1);
    println!("         IF is the interrupt flag. It is 1, which is why the");
    println!("         keyboard you just typed on works.");
}

fn dump_gdt() {
    let entries = gdt::entries();
    println!("  #  raw                  meaning");
    for (index, entry) in entries.iter().enumerate() {
        let raw = *entry;
        let meaning = match index {
            0 => "null descriptor (required, must be zero)",
            1 => "kernel code, ring 0, long mode",
            2 => "kernel data, ring 0, writable",
            3 => "TSS descriptor, low half",
            4 => "TSS descriptor, high half (64-bit base)",
            _ => "unused",
        };
        println!("  {index}  {raw:#018x}   {meaning}");
    }
    println!();
    println!("  Entry 1 has bit 53 set: that single bit is what makes this a");
    println!("  64-bit code segment rather than a 32-bit one.");
}

fn dump_idt() {
    println!("  wired-up interrupt vectors:");
    let named: [(usize, &str); 8] = [
        (0, "divide by zero"),
        (3, "breakpoint (int3)"),
        (6, "invalid opcode"),
        (8, "double fault  [runs on IST stack]"),
        (13, "general protection fault"),
        (14, "page fault"),
        (32, "timer  (IRQ0, via PIC)"),
        (33, "keyboard (IRQ1, via PIC)"),
    ];
    for (vector, name) in named {
        let mark = if interrupts::is_present(vector) { "yes" } else { "NO " };
        println!("  {vector:>3}  present={mark}  {name}");
    }
    println!();
    println!("  The other 248 slots are empty. Hitting one is a fault we cannot");
    println!("  handle, which becomes a double fault, which we can.");
}

fn mem() {
    let info = unsafe { BOOT_INFO };
    let Some(info) = info else {
        println!("  no boot info");
        return;
    };

    let mut usable = 0u64;
    println!("  {:<20} {:<20} {:>10}  KIND", "START", "END", "SIZE");
    for region in info.memory_regions.iter() {
        if region.kind == MemoryRegionKind::Usable {
            usable += region.end - region.start;
        }
        println!(
            "  {:#018x}  {:#018x}  {:>7} KiB  {:?}",
            region.start,
            region.end,
            (region.end - region.start) / 1024,
            region.kind
        );
    }
    println!();
    println!("  {} MiB usable across {} regions.", usable / 1024 / 1024, info.memory_regions.len());
}

fn uptime() {
    let ticks = interrupts::ticks();
    // The PIT free-runs at ~18.2065 Hz by default. We have not reprogrammed it,
    // so this is the divisor the BIOS left behind.
    println!("  {ticks} ticks  (~{} seconds at the PIT's default 18.2 Hz)", ticks / 18);
}
