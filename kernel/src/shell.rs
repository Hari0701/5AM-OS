//! The shell — where 5AM-OS stops being a log and starts being a system.
//!
//! Every command here reads the *live* machine. `explain gdt` does not print a
//! description of what a GDT is; it asks the CPU where its GDT is, walks it,
//! and decodes the bytes that are actually there. If you change the kernel, the
//! explanation changes with it, because there is nothing to keep in sync.

use crate::keyboard::{wait_key, Key};
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
        // Blocks. The task leaves the scheduler's rotation entirely until a
        // key arrives, instead of being handed slices to rediscover that the
        // buffer is empty. With a generation running, that is the difference
        // between the model getting most of the CPU and half of it.
        match wait_key() {
            Key::Char(c) => {
                if len < MAX_LINE {
                    line[len] = c as u8;
                    len += 1;
                    print!("{c}");
                }
            }
            Key::Backspace => {
                if len > 0 {
                    len -= 1;
                    // Move back, overwrite with a space, move back again —
                    // a terminal has no concept of "delete".
                    print!("\x08 \x08");
                }
            }
            Key::Enter => {
                println!();
                let command = core::str::from_utf8(&line[..len]).unwrap_or("");
                crate::task::reap_finished();
                execute(command.trim());
                len = 0;
                prompt();
            }
        }
    }
}

fn banner() {
    println!();
    println!("5AM-OS shell. Everything below reads the live machine.");
    println!("Type `help`, or `explain <topic>` to learn what is under you.");
    println!("Type here in this terminal, or in the VM window -- both work.");
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
        "explain" => explain(rest),
        "regs" => regs(),
        "gdt" => dump_gdt(),
        "idt" => dump_idt(),
        "mem" => mem(),
        "uptime" => uptime(),
        "fpu" => fpu_check(),
        "screen" => screen(),
        "heap" => heap_status(),
        "tasks" => crate::task::report(),
        "spawn" => spawn(rest),
        "workers" => workers(),
        "selftest" => {
            println!();
            crate::selftest::run(rest.trim());
            println!();
        }
        "sleep" => sleep_command(rest),
        "user" => user_mode(),
        "ls" => list_files(),
        "cat" => cat(rest),
        "exec" => exec(rest),
        "translate" => translate(rest),
        "llm" => llm(rest),
        "model" => crate::llm::describe(),
        "fault" => fault(rest),
        "ask" => ask(rest),
        "bridge" => bridge(rest),
        "clear" => {
            // Two different displays, two different ways to clear. The escape
            // sequence means something to a terminal on the other end of the
            // serial port; the framebuffer has never heard of ANSI and would
            // just draw the characters.
            print!("\x1b[2J\x1b[H");
            crate::framebuffer::clear();
        }
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
    println!("  explain <topic>   read the machine and explain a subsystem");
    println!("                    topics: boot gdt idt interrupts paging");
    println!("                            rings serial keyboard");
    println!("  regs              live control registers");
    println!("  gdt               decode every GDT entry");
    println!("  idt               which interrupt vectors are wired up");
    println!("  mem               physical memory map from the firmware");
    println!("  uptime            timer ticks since boot");
    println!("  fpu               is floating point on, and does it work?");
    println!("  screen            the framebuffer this text is drawn on");
    println!("  heap              the allocator, proved with a live Vec");
    println!("  tasks             what is running, and how often it switched");
    println!("  spawn <prompt>    run the transformer in the background and");
    println!("                    keep using the shell while it thinks");
    println!("  user              drop to ring 3 and come back through a");
    println!("                    syscall -- the privilege boundary, live");
    println!("  selftest [suite]  run the kernel's tests against itself");
    println!("                    suites: heap memory sync sched priority elf fat");
    println!("  workers           three tasks share one semaphore, visibly");
    println!("  sleep <ticks>     block this shell on the clock, not a spin");
    println!("  ls                list the files on the FAT16 disk");
    println!("  cat <file>        print a file from that disk");
    println!("  exec <file>       load an ELF off the disk and run it in ring 3");
    println!("  translate <addr>  walk the page tables for an address");
    println!("  model             what neural network is loaded, if any");
    println!("  llm <prompt>      run that network. It writes stories; it does");
    println!("                    NOT know anything about this kernel.");
    println!("  fault <kind>      deliberately break something:");
    println!("                    int3 | div0 | page | null | wild | stack");
    println!("  ask <question>    ask the kernel about itself. Answered inside");
    println!("                    this machine -- no network, no host process.");
    println!("  bridge <question> send the question out over COM2 to a host");
    println!("                    process instead (optional, needs bridge.py)");
    println!("  clear             clear the screen");
}

fn explain(topic: &str) {
    match topic {
        "boot" => {
            println!("BOOT");
            println!("  The CPU starts in 16-bit real mode for compatibility with");
            println!("  1978. Getting to 64-bit long mode means: build a GDT,");
            println!("  enable protected mode, build page tables, enable PAE,");
            println!("  set the long mode bit, enable paging, far jump. In that");
            println!("  order. Any mistake is a triple fault with no message.");
            println!();
            println!("  Right now that work is done for us by the `bootloader`");
            println!("  crate. Replacing it with our own stage is a milestone.");
        }
        "gdt" => {
            let (base, limit) = gdt::current();
            println!("GDT — Global Descriptor Table");
            println!("  The CPU's list of memory segments.");
            println!();
            println!("  Live: base {base:#018x}, limit {limit} ({} entries)", (limit + 1) / 8);
            println!();
            println!("  In 64-bit mode segment base and limit are ignored: every");
            println!("  segment is all of memory. What still matters is the");
            println!("  privilege level, and the TSS entry — which is how the");
            println!("  CPU finds a known-good stack when the current one is");
            println!("  unusable. Run `gdt` to decode the actual entries.");
        }
        "idt" => {
            let (base, limit) = interrupts::current();
            println!("IDT — Interrupt Descriptor Table");
            println!("  256 slots. Each one says: when interrupt N happens, jump");
            println!("  here, on this stack, at this privilege level.");
            println!();
            println!("  Live: base {base:#018x}, limit {limit}");
            println!();
            println!("  Vectors 0-31 are Intel's: the CPU raises them at you.");
            println!("  32+ are yours. We pointed 32 and 33 at the timer and the");
            println!("  keyboard. Run `idt` to see which are wired up.");
        }
        "interrupts" => {
            println!("INTERRUPTS");
            println!("  The mechanism that lets hardware get your attention");
            println!("  without you asking. Without them a kernel must poll, and");
            println!("  polling means burning a core to notice a keypress.");
            println!();
            println!("  When one fires the CPU pushes RIP, CS, RFLAGS, RSP, SS,");
            println!("  looks up the IDT, and jumps. Your handler must preserve");
            println!("  every register and return with `iretq` — which is what");
            println!("  Rust's `extern \"x86-interrupt\"` does for us.");
            println!();
            println!("  Ticks so far: {}", interrupts::ticks());
            println!("  That number is incremented by a handler running roughly");
            println!("  18.2 times a second, entirely behind your back.");
        }
        "paging" => {
            let cr3: u64;
            unsafe { asm!("mov {}, cr3", out(reg) cr3, options(nomem, nostack)) };
            println!("PAGING");
            println!("  Every address your code uses is a lie the CPU maintains.");
            println!();
            println!("  CR3 = {cr3:#018x}");
            println!("  ^ physical address of the level-4 page table. A virtual");
            println!("    address is split into four 9-bit indexes plus a 12-bit");
            println!("    offset; the CPU walks four tables to find the physical");
            println!("    frame. That is four extra memory reads per access,");
            println!("    which is why the TLB cache exists.");
            println!();
            println!("  This is what makes each program think it owns the");
            println!("  machine, and what stops it reaching into another's memory.");
        }
        "rings" => {
            let cs: u16;
            unsafe { asm!("mov {0:x}, cs", out(reg) cs, options(nomem, nostack)) };
            println!("PRIVILEGE RINGS");
            println!("  x86 has four; everyone uses two.");
            println!();
            println!("  Current CS = {cs:#06x} -> ring {}", cs & 0b11);
            println!();
            println!("  Ring 0 can execute any instruction, read any register,");
            println!("  touch any memory. Ring 3 cannot do I/O, cannot load");
            println!("  descriptor tables, cannot disable interrupts. Every");
            println!("  program you have ever run was in ring 3, asking a ring 0");
            println!("  kernel for permission. You are currently the thing that");
            println!("  grants permission.");
        }
        "serial" => {
            println!("SERIAL");
            println!("  A 16550 UART at I/O port 0x3F8. Writing one byte there");
            println!("  puts a character on the wire.");
            println!();
            println!("  x86 has a second address space for devices, reached only");
            println!("  with `in` and `out`. That is why serial works with zero");
            println!("  setup while the screen needs a framebuffer and a font.");
            println!("  It is also why every serious kernel keeps a serial");
            println!("  console: it still works when everything else is broken.");
        }
        "keyboard" => {
            println!("KEYBOARD");
            println!("  The keyboard sends scancodes, not characters. One number");
            println!("  when a key goes down, another when it comes up.");
            println!();
            println!("  'A' is not a thing the hardware knows. The kernel decides");
            println!("  that scancode 0x1E plus shift means 'A' — which is why");
            println!("  keyboard layouts are software.");
            println!();
            println!("  The interrupt handler does almost nothing: read port 0x60,");
            println!("  push the byte into a ring buffer, return. This shell pops");
            println!("  from that buffer with interrupts disabled, because the");
            println!("  handler can fire in the middle of our read.");
        }
        "" => println!("usage: explain <topic>   (see `help` for the list)"),
        other => println!("no topic called `{other}` — see `help`"),
    }
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

/// Three tasks, one semaphore, one counter.
///
/// The point is the *absence* of interleaving inside the critical section.
/// Every worker sleeps before it tries, so they genuinely contend; the counter
/// still reaches exactly nine, because only one is ever inside.
fn workers() {
    crate::task::reset_counter();
    println!();
    println!("  Three workers, each incrementing a shared counter three times.");
    println!("  Each holds a semaphore across a read, a sleep, and a write --");
    println!("  without it, the read-modify-write would lose updates.");
    println!();

    let mut ids = [0usize; 3];
    for (index, slot) in ids.iter_mut().enumerate() {
        match crate::task::spawn("worker", crate::task::Work::Worker(index)) {
            Ok(id) => *slot = id,
            Err(error) => {
                println!("  could not spawn worker {index}: {error}");
                return;
            }
        }
    }

    // Block until every worker is done. The shell is asleep for all of this,
    // not spinning -- which is the only reason the workers get the CPU.
    for id in ids {
        crate::task::wait_for(id);
    }

    let total = crate::task::counter_value();
    println!();
    println!("  final counter: {total} (expected 9)");
    if total == 9 {
        println!("  No updates lost. Nine increments, nine results.");
    } else {
        println!("  Updates were lost -- the critical section is not exclusive.");
    }
    crate::task::reap_finished();
    println!();
}

fn sleep_command(argument: &str) {
    let ticks: u64 = argument.trim().parse().unwrap_or(18);
    let before = crate::interrupts::ticks();
    println!("  sleeping {ticks} ticks ...");
    crate::task::sleep(ticks);
    println!(
        "  awake after {} ticks. The shell was Blocked, not spinning:",
        crate::interrupts::ticks() - before
    );
    println!("  the scheduler was not offering it the CPU at all.");
}

/// Everything the disk commands need: a mounted volume, or a reason why not.
fn volume() -> Option<crate::fat::Volume> {
    match crate::fat::mount() {
        Ok(volume) => Some(volume),
        Err(error) => {
            println!("  cannot read the disk: {error}");
            None
        }
    }
}

fn list_files() {
    let Some(volume) = volume() else { return };
    let entries = match volume.list() {
        Ok(entries) => entries,
        Err(error) => {
            println!("  cannot read the root directory: {error}");
            return;
        }
    };

    if entries.is_empty() {
        println!("  the disk is empty");
        return;
    }

    println!(
        "  FAT16, {} clusters of {} bytes",
        volume.clusters,
        volume.sectors_per_cluster * volume.bytes_per_sector
    );
    println!("  {:<14}{:>9}  {}", "name", "size", "first cluster");
    for entry in &entries {
        println!("  {:<14}{:>9}  {}", entry.name, entry.size, entry.first_cluster);
    }
}

fn cat(name: &str) {
    if name.is_empty() {
        println!("  usage: cat <file>");
        return;
    }
    let Some(volume) = volume() else { return };
    let entry = match volume.find(name) {
        Ok(entry) => entry,
        Err(error) => {
            println!("  {name}: {error}");
            return;
        }
    };
    let data = match volume.read_file(&entry) {
        Ok(data) => data,
        Err(error) => {
            println!("  {name}: {error}");
            return;
        }
    };

    match core::str::from_utf8(&data) {
        Ok(text) => print!("{text}"),
        Err(_) => println!("  {name} is not text ({} bytes)", data.len()),
    }
}

/// Load an ELF off the disk and run it.
///
/// This is the command the last three subsystems were building towards. The
/// kernel has never seen this file, was not compiled against it, and learns its
/// entry point by reading it.
fn exec(name: &str) {
    if name.is_empty() {
        println!("  usage: exec <file>");
        return;
    }
    let Some(volume) = volume() else { return };
    let entry = match volume.find(name) {
        Ok(entry) => entry,
        Err(error) => {
            println!("  {name}: {error}");
            return;
        }
    };
    let data = match volume.read_file(&entry) {
        Ok(data) => data,
        Err(error) => {
            println!("  {name}: {error}");
            return;
        }
    };

    println!();
    println!("  Read {} bytes of {name} off the disk.", data.len());
    crate::user::run_bytes(&data);
    println!();
}

/// Run the ring 3 demonstration.
///
/// Kept behind a command rather than run at boot because it is the one thing
/// here you want to watch deliberately.
fn user_mode() {
    println!();
    println!("  Everything this kernel has done so far ran in ring 0: full");
    println!("  privileges, kernel address space, no supervision. What follows");
    println!("  does not.");
    println!();
    crate::user::run();
    println!();
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

/// Run the transformer as a separate task.
fn spawn(prompt: &str) {
    use alloc::string::ToString;
    if prompt.is_empty() {
        println!("  usage: spawn <prompt>");
        println!();
        println!("  Same as `llm`, except the shell stays usable while it runs.");
        println!("  Try it and then type `tasks` while the story is generating.");
        return;
    }
    match crate::task::spawn("llm", crate::task::Work::Generate(prompt.to_string())) {
        Ok(id) => {
            println!("  [task {id} started -- the prompt is still yours]");
        }
        Err(reason) => println!("  could not spawn: {reason}"),
    }
}

/// Show the allocator working, rather than asserting that it does.
fn heap_status() {
    use alloc::string::String;
    use alloc::vec::Vec;
    use core::fmt::Write;

    let (free_frames, total_frames) = crate::memory::allocator().stats();
    println!("  physical frames : {free_frames} free of {total_frames}");
    println!("                    ({} MiB still unallocated)",
        free_frames * crate::memory::PAGE_SIZE / 1024 / 1024);

    let (free, holes, live) = crate::heap::stats();
    println!("  heap            : {free} bytes free in {holes} hole(s), {live} live allocation(s)");
    println!("                    mapped at {:#x}, {} KiB",
        crate::heap::HEAP_START, crate::heap::HEAP_SIZE / 1024);
    println!();

    // The proof. None of this could have been written anywhere in this kernel
    // before the allocator existed -- there was no way to have a growable
    // anything.
    let mut values: Vec<u32> = Vec::new();
    for i in 1..=8 {
        values.push(i * i);
    }
    let mut text = String::new();
    let _ = write!(text, "{:?}", values);

    println!("  A Vec, grown at runtime : {text}");
    println!("  Its heap address        : {:p}", values.as_ptr());

    let (after, _, live_after) = crate::heap::stats();
    println!("  Heap free while held    : {after} bytes, {live_after} live");

    drop(values);
    drop(text);
    let (recovered, holes_end, live_end) = crate::heap::stats();
    println!("  After dropping them     : {recovered} bytes, {live_end} live");
    println!();

    // The real test of an allocator is not that it allocates -- it is that the
    // free list survives churn. Without coalescing, this loop leaves hundreds
    // of unmergeable holes and the hole count below climbs with every run.
    let before = crate::heap::stats();
    let mut kept: Vec<Vec<u8>> = Vec::new();
    for round in 0..400 {
        let size = 16 + (round * 37) % 512;
        let block = alloc::vec![round as u8; size];
        // Keep every third block, drop the rest -- interleaving is what
        // actually fragments a heap; allocating then freeing in order does not.
        if round % 3 == 0 {
            kept.push(block);
        }
    }
    let mid = crate::heap::stats();
    drop(kept);
    let after = crate::heap::stats();

    println!("  400 alloc/free cycles, interleaved:");
    println!("    before : {} bytes free, {} hole(s)", before.0, before.1);
    println!("    during : {} bytes free, {} hole(s)", mid.0, mid.1);
    println!("    after  : {} bytes free, {} hole(s)", after.0, after.1);
    println!();
    if after.0 == before.0 && after.1 <= before.1 {
        println!("  Every byte came back and the holes merged back into one.");
        println!("  That is coalescing working: without it the hole count would");
        println!("  climb on every run until a large allocation could not fit.");
    } else {
        println!("  LEAK or FRAGMENTATION: the heap did not return to its");
        println!("  starting state. {} bytes and {} hole(s) unaccounted for.",
            before.0 as i64 - after.0 as i64, after.1 as i64 - before.1 as i64);
    }
    let _ = (holes_end, live_end);
}

/// Walk the page tables for an address, showing every level.
fn translate(argument: &str) {
    let address = match parse_hex(argument) {
        Some(value) => value,
        None => {
            println!("  usage: translate <hex address>");
            println!();
            println!("  Try `translate {:#x}` for the heap, or the RSP value", crate::heap::HEAP_START);
            println!("  from `regs` to see where your own stack lives.");
            return;
        }
    };

    println!("  virtual {address:#018x}");
    println!();
    match crate::memory::translate(address) {
        None => {
            println!("  Not mapped. Nothing in the tree rooted at CR3 covers it,");
            println!("  which is exactly what a page fault would tell you.");
        }
        Some((physical, entries)) => {
            let names = ["level 4", "level 3", "level 2", "level 1"];
            for (name, entry) in names.iter().zip(entries.iter()) {
                if *entry == 0 {
                    continue;
                }
                println!(
                    "    {name}  entry {entry:#018x}  -> frame {:#x}{}",
                    entry & 0x000f_ffff_ffff_f000,
                    if entry & (1 << 1) != 0 { " writable" } else { " read-only" },
                );
            }
            println!();
            println!("  physical {physical:#018x}");
            println!();
            println!("  Four table reads to resolve one address. That is what");
            println!("  the TLB caches, and why a TLB miss is expensive.");
        }
    }
}

fn parse_hex(text: &str) -> Option<u64> {
    let text = text.trim().trim_start_matches("0x");
    if text.is_empty() {
        return None;
    }
    u64::from_str_radix(text, 16).ok()
}

/// Describe the display we are drawing on.
fn screen() {
    match crate::framebuffer::info() {
        None => {
            println!("  No framebuffer. This output is going to the serial port");
            println!("  only -- which is why serial stays the primary console.");
        }
        Some((width, height, columns, rows)) => {
            println!("  {width}x{height} pixels, {columns}x{rows} characters.");
            println!();
            println!("  There is no text mode here. The hardware knows only");
            println!("  pixels; every character on this screen was drawn one");
            println!("  pixel at a time from a 16-byte bitmap in font.rs, and");
            println!("  the screen scrolls by copying itself upward.");
        }
    }
}

/// Run the in-kernel transformer.
fn llm(prompt: &str) {
    if prompt.is_empty() {
        println!("  usage: llm <prompt>");
        println!();
        println!("  Runs a 15M-parameter Llama-2 transformer inside this kernel,");
        println!("  in ring 0, with no OS beneath it and nothing linked in.");
        println!();
        println!("  It was trained on children's stories, so give it one to");
        println!("  continue -- `llm Once upon a time`. It does not know what an");
        println!("  operating system is; use `ask` for questions about this");
        println!("  machine. Run `model` for what is actually loaded.");
        return;
    }
    crate::llm::generate(prompt, 96);
}

/// Prove the FPU is on by doing arithmetic the CPU could not do at reset.
fn fpu_check() {
    let (cr0, cr4) = crate::fpu::state();
    println!("  CR0 = {cr0:#018x}   EM(bit 2)={} MP(bit 1)={}", (cr0 >> 2) & 1, (cr0 >> 1) & 1);
    println!("  CR4 = {cr4:#018x}   OSFXSR(bit 9)={} OSXMMEXCPT(bit 10)={}",
        (cr4 >> 9) & 1, (cr4 >> 10) & 1);
    println!();
    if !crate::fpu::enabled() {
        println!("  Floating point is OFF. Any float instruction would fault.");
        return;
    }

    // Actual hardware arithmetic. If SSE were still emulated, reaching this
    // line at all would have raised #UD.
    let a = 3.5_f32;
    let b = 1.25_f32;
    let sum = a + b;
    let product = a * b;
    let quotient = a / b;
    let root = crate::llm::sqrt(a * a);

    println!("  3.5 + 1.25 = {}", FloatText(sum));
    println!("  3.5 * 1.25 = {}", FloatText(product));
    println!("  3.5 / 1.25 = {}", FloatText(quotient));
    println!("  sqrt(12.25) = {}", FloatText(root));
    println!();
    println!("  Those were real SSE instructions on real XMM registers. At");
    println!("  reset this CPU would have faulted on every one of them.");
}

/// Print an f32 without libm or an allocator.
///
/// core::fmt can format floats, but it pulls in a large formatting path; this
/// is a fixed 4-decimal renderer, which is all the kernel needs.
pub struct FloatText(pub f32);

impl core::fmt::Display for FloatText {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let value = self.0;
        if value.is_nan() {
            return write!(f, "NaN");
        }
        let negative = value < 0.0;
        let magnitude = if negative { -value } else { value };
        let whole = magnitude as u64;
        let frac = ((magnitude - whole as f32) * 10_000.0 + 0.5) as u64;
        let (whole, frac) = if frac >= 10_000 { (whole + 1, 0) } else { (whole, frac) };
        write!(f, "{}{}.{:04}", if negative { "-" } else { "" }, whole, frac)
    }
}

fn uptime() {
    let ticks = interrupts::ticks();
    // The PIT free-runs at ~18.2065 Hz by default. We have not reprogrammed it,
    // so this is the divisor the BIOS left behind.
    println!("  {ticks} ticks  (~{} seconds at the PIT's default 18.2 Hz)", ticks / 18);
}

/// Answer a question using only what is inside this machine.
fn ask(question: &str) {
    if question.is_empty() {
        println!("  usage: ask <question>");
        println!();
        println!("  Answered entirely inside 5AM-OS: no network, no host process,");
        println!("  nothing to install. It reads the live registers and explains");
        println!("  what it finds. See `ask how do you work` for what it actually");
        println!("  is -- it is not a model, and it says so.");
        return;
    }
    crate::oracle::answer(question);
}

/// Send a question out over COM2 to a host process.
fn bridge(question: &str) {
    if question.is_empty() {
        println!("  usage: bridge <question>");
        println!();
        println!("  Sends the question out of the second serial port, with this");
        println!("  machine's register state attached, to bridge/bridge.py on the");
        println!("  host. Entirely optional -- `ask` needs nothing attached.");
        return;
    }
    crate::ai::ask(question);
}

/// Deliberately break the machine, to prove the handlers are real.
fn fault(kind: &str) {
    match kind {
        "int3" => {
            println!("  executing int3 ...");
            unsafe { asm!("int3", options(nomem, nostack)) };
            println!("  ...and we came back. A handled exception is survivable.");
        }
        "div0" => {
            println!("  dividing by zero ...");
            unsafe {
                asm!(
                    "xor rdx, rdx",
                    "xor rcx, rcx",
                    "mov rax, 1",
                    "div rcx",
                    out("rax") _, out("rdx") _, out("rcx") _,
                    options(nomem, nostack),
                );
            }
        }
        "null" => {
            println!("  dereferencing a null pointer ...");
            unsafe {
                let bad = core::ptr::null_mut::<u64>();
                core::ptr::write_volatile(bad, 42);
            }
        }
        "wild" => {
            println!("  dereferencing a non-canonical address ...");
            unsafe {
                let bad = 0xdead_beef_dead_beef_u64 as *mut u64;
                core::ptr::write_volatile(bad, 42);
            }
        }
        "page" => {
            println!("  dereferencing 0xdeadbeef ...");
            unsafe {
                let bad = 0xdead_beef as *mut u64;
                core::ptr::write_volatile(bad, 42);
            }
        }
        "stack" => {
            println!("  recursing until the stack runs out ...");
            println!("  (this is the one that would reboot a machine without an");
            println!("   IST stack for the double fault handler)");
            blow_the_stack(0);
        }
        _ => println!("  usage: fault int3|div0|page|null|wild|stack"),
    }
}

/// Infinite recursion, to prove the double fault handler is real.
///
/// Getting this to actually overflow takes some care. The obvious version —
/// recurse, then return — is a *tail call*, and LLVM rewrites tail calls into
/// jumps. The stack then never grows and the machine happily loops forever.
///
/// Touching a local *after* the recursive call is what prevents that: the frame
/// has to survive the call, so a real frame must be pushed every time.
#[allow(unconditional_recursion)]
fn blow_the_stack(depth: u64) {
    let marker = depth;
    blow_the_stack(depth + 1);
    unsafe { core::ptr::read_volatile(&marker) };
}
