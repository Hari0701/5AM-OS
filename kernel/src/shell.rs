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

    // Hand the machine to userspace before offering a kernel prompt at all.
    //
    // On a real system the first process *is* userspace: the kernel finishes
    // booting and runs one program, and everything else descends from it. This
    // is that, one step short -- if init cannot be started or exits, the kernel
    // shell below is the fallback, which a real machine would call a panic.
    crate::user::start_init();

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
                // If the scheduler watchdog took the wheel back, say so here
                // rather than where it happened. It happened inside the timer
                // interrupt, where printing means waiting on a spinlock the
                // interrupted code may be holding -- which on one core is not a
                // wait, it is the end. This line runs at all only because the
                // watchdog acted.
                crate::sched::report_starvation();
                len = 0;
                prompt();
            }
        }
    }
}

fn banner() {
    println!();
    println!("5AM-OS shell. Everything below reads the live machine.");
    println!();
    println!("  New here?  type `tour`   -- eight commands, in an order that builds.");
    println!("  Otherwise  type `help`   -- all thirty-five, grouped.");
    println!();
    println!("Type in this terminal, or in the VM window -- both work.");
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
        "tour" => tour(rest.trim()),
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
        "sched" => sched_command(rest.trim()),
        "paging" => paging_command(rest.trim()),
        "bench" => bench_command(rest.trim()),
        "timeline" => timeline_command(rest.trim()),
        "spawn" => spawn(rest),
        "workers" => workers(),
        "ticker" => ticker(),
        "selftest" => {
            println!();
            crate::selftest::run(rest.trim());
            println!();
        }
        "sleep" => sleep_command(rest),
        "user" => user_mode(),
        "ls" => list_files(),
        "cat" => cat(rest),
        "write" => write_file(rest),
        "rm" => remove_file(rest),
        "exec" => exec(rest),
        "translate" => translate(rest),
        "pagemap" => pagemap(),
        "smp" => smp_step(rest.trim()),
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

/// One stop on the guided tour.
struct Stop {
    command: &'static str,
    lines: &'static [&'static str],
}

/// The way in.
///
/// There are thirty-five commands in this shell and a newcomer typing `help`
/// gets all of them at once, which is the same as getting none. This is eight
/// of them in an order that builds, and it deliberately does not run anything
/// for you -- you type the command, because typing it is the part that sticks.
///
/// The last stop halts the machine on purpose. That is not a bug in the tour.
static TOUR: &[Stop] = &[
    Stop {
        command: "explain rings",
        lines: &[
            "Start with where you are. That command reads CS out of the CPU",
            "and tells you which privilege ring you are standing in.",
            "",
            "It will say ring 0. Every program you have ever written ran in",
            "ring 3, asking a kernel for permission. Right now there is",
            "nothing above you to ask.",
        ],
    },
    Stop {
        command: "regs",
        lines: &[
            "The control registers, live. CR3 is the physical address of the",
            "page table tree; CR0 bit 31 is whether paging is on at all.",
            "",
            "Reading these from a normal program is an instant fault. This is",
            "the first thing this machine can show you that your laptop",
            "cannot.",
        ],
    },
    Stop {
        command: "translate 0x400000",
        lines: &[
            "One address, walked by hand through all four page tables.",
            "",
            "0x400000 is where user programs load. From the kernel's own",
            "address space it is usually not mapped -- and `not mapped` is",
            "precisely what a page fault means. Try `translate 0x444444440000`",
            "afterwards, which is the heap, and compare.",
        ],
    },
    Stop {
        command: "workers",
        lines: &[
            "Three tasks share one counter, each holding a semaphore across a",
            "read, a sleep and a write.",
            "",
            "Watch the interleaving. Without the semaphore the read-modify-",
            "write is three steps with a preemption point between each, and",
            "the total comes out wrong in a way that depends on timing.",
            "Nine increments, nine results, or the lock is not working.",
        ],
    },
    Stop {
        command: "bench sched",
        lines: &[
            "This is the one to slow down for. Five scheduling policies, one",
            "workload, one table. Takes about half a minute.",
            "",
            "Read the `prio` row. It has the BEST interactive latency in the",
            "table and it starves a task anyway. Starvation is not a scheduler",
            "being bad at scheduling -- strict priority is excellent at serving",
            "what you declared important. It simply has no floor.",
            "",
            "Then `sched mlfq` and run it again. You just changed how the",
            "machine decides what runs next, while it was running.",
        ],
    },
    Stop {
        command: "bench paging",
        lines: &[
            "The same idea for memory, and one row worth the whole detour.",
            "",
            "`fifo` takes NINE faults with three frames and TEN with four. The",
            "machine was given more memory and did more work. Belady found",
            "that in 1969 and it is still the most surprising true thing in",
            "the subject -- and those are real pages, real accessed bits, real",
            "writes to the disk.",
        ],
    },
    Stop {
        command: "selftest",
        lines: &[
            "108 checks, run inside the machine, because that is the only",
            "honest place. Your laptop can verify an ELF header parses. It",
            "cannot take a page fault or switch a stack.",
            "",
            "`selftest claims` is the odd one: it tests what this repository",
            "SAYS about itself, because the README lied about six things for",
            "a month and no test could catch a sentence.",
        ],
    },
    Stop {
        command: "fault stack",
        lines: &[
            "Last one, and it ends the machine on purpose. Ctrl-A then X to",
            "quit afterwards, then ./run.sh again.",
            "",
            "It recurses until the kernel stack runs out. That faults. Faulting",
            "while handling a fault is a double fault -- and without a known-",
            "good stack to land on, that becomes a triple fault, which is not",
            "an exception at all. It is the CPU giving up and resetting the",
            "machine with nothing printed.",
            "",
            "You are about to watch this one survive it and tell you why.",
        ],
    },
];

static mut TOUR_STEP: usize = 0;

fn tour(argument: &str) {
    let step = match argument.trim() {
        "" => unsafe { core::ptr::read_volatile(core::ptr::addr_of!(TOUR_STEP)) },
        "reset" | "restart" => {
            unsafe { core::ptr::write_volatile(core::ptr::addr_of_mut!(TOUR_STEP), 0) };
            0
        }
        text => match text.parse::<usize>() {
            Ok(n) if n >= 1 && n <= TOUR.len() => n - 1,
            _ => {
                println!("  usage: tour            the next stop");
                println!("         tour <1..{}>    jump to one", TOUR.len());
                println!("         tour reset     start over");
                return;
            }
        },
    };

    if step >= TOUR.len() {
        println!();
        println!("  That is the tour. What you have not seen yet is the part that");
        println!("  actually teaches: reading this kernel will not do it. Every hard");
        println!("  decision in here is already made, correctly, with a comment");
        println!("  saying why -- which is exactly the problem.");
        println!();
        println!("  So there are nine labs in exercises/. Each one deletes a working");
        println!("  function and asks for it back, and `selftest` tells you whether");
        println!("  you were right.");
        println!();
        println!("  Start with exercises/03-heap.md. Not lab 1 -- the heap is the");
        println!("  smallest thing here that is genuinely an operating system");
        println!("  problem, and it depends on nothing else.");
        println!();
        println!("  `tour reset` to go round again.");
        println!();
        return;
    }

    let stop = &TOUR[step];
    println!();
    println!("  [{} of {}]   type:  {}", step + 1, TOUR.len(), stop.command);
    println!();
    for line in stop.lines {
        if line.is_empty() {
            println!();
        } else {
            println!("  {line}");
        }
    }
    println!();
    if step + 1 < TOUR.len() {
        println!("  then `tour` for the next one.");
    } else {
        println!("  then `tour` for what to do next.");
    }
    println!();

    unsafe { core::ptr::write_volatile(core::ptr::addr_of_mut!(TOUR_STEP), step + 1) };
}

fn help() {
    println!("  new here?  type `tour`  -- eight commands, in an order that builds.");
    println!();
    println!("ASK IT THINGS");
    println!("  explain <topic>   read the live machine and explain a subsystem:");
    println!("                    boot gdt idt interrupts paging rings serial");
    println!("                    keyboard");
    println!("  ask <question>    answered inside this machine -- keyword matching");
    println!("                    plus hardware decoders. no network, no model.");
    println!("  bridge <question> send it out over COM2 to a host process instead");
    println!("                    (optional, needs bridge.py)");
    println!();
    println!("LOOK AT THE MACHINE");
    println!("  regs              live control registers");
    println!("  gdt / idt         decode the descriptor tables the CPU is using");
    println!("  mem               physical memory map from the firmware");
    println!("  pagemap           which 512 GiB slots of the address space exist");
    println!("  translate <addr>  walk the page tables for one address");
    println!("  heap              the allocator, proved with a live Vec");
    println!("  tasks             what is running, and how often it switched");
    println!("  uptime / fpu / screen");
    println!();
    println!("CHANGE HOW IT DECIDES");
    println!("  sched [policy]    what runs next: rr fifo prio aging mlfq");
    println!("  paging [policy]   what leaves memory: clock fifo nru random");
    println!();
    println!("MEASURE IT");
    println!("  bench sched [n]   one workload under every scheduling policy");
    println!("  bench paging      every replacement policy, and Belady's anomaly");
    println!("  timeline [n]      who ran, and who was runnable and passed over");
    println!("  selftest [suite]  108 checks, run inside the machine");
    println!("                    heap memory space cow swap pipe sync sched");
    println!("                    policy replace priority elf fat claims");
    println!();
    println!("MAKE IT DO SOMETHING");
    println!("  workers           three tasks share one semaphore, visibly");
    println!("  ticker            a kernel task that prints while other things run");
    println!("  user              drop to ring 3 and come back through a syscall");
    println!("  exec <file>       load an ELF off the disk and run it in ring 3");
    println!("  sleep <ticks>     block this shell on the clock, not a spin");
    println!("  smp <step>        wake a second processor: apic | install | wake");
    println!();
    println!("THE DISK");
    println!("  ls / cat <file> / write <file> <..> / rm <file>");
    println!();
    println!("BREAK IT ON PURPOSE");
    println!("  fault <kind>      int3 | div0 | page | null | wild | stack");
    println!();
    println!("THE NEURAL NETWORK  (needs ./run.sh --ai)");
    println!("  model             what is loaded, if anything");
    println!("  llm <prompt>      run it. It writes stories. It does NOT know");
    println!("                    anything about this kernel.");
    println!("  spawn <prompt>    same, on its own task, so the shell stays yours");
    println!();
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

/// A background task that reports it is still alive.
///
/// Start it, then immediately `exec spin.elf` -- a ring 3 program that loops
/// without ever making a syscall. If the ticker keeps printing while the
/// spinner runs, the timer is taking the CPU away from ring 3.
fn ticker() {
    match crate::task::spawn("ticker", crate::task::Work::Ticker { times: 30, gap: 9 }) {
        Ok(id) => {
            println!("  [task {id} ticking -- now run `exec spin.elf`]");
        }
        Err(error) => println!("  could not spawn: {error}"),
    }
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

/// Draw what the scheduler has been doing.
fn timeline_command(argument: &str) {
    let span: u64 = match argument {
        "" => 60,
        text => match text.parse() {
            Ok(value) if (8..=1024).contains(&value) => value,
            _ => {
                println!("  usage: timeline [ticks]   (8..1024, default 60)");
                return;
            }
        },
    };
    let now = interrupts::ticks();
    println!();
    crate::bench::timeline(now.saturating_sub(span), now);
    println!();
}

/// Measure something, rather than assert it.
fn bench_command(argument: &str) {
    let (what, rest) = split(argument);
    match what {
        "sched" => crate::bench::sched(rest),
        "paging" => crate::bench::paging(),
        "" => {
            println!("  bench sched [ticks]   every scheduling policy, one workload,");
            println!("                        one table. takes about half a minute.");
            println!("  bench paging          every page replacement policy against");
            println!("                        two reference strings, on real pages.");
        }
        other => println!("  nothing to benchmark called `{other}`. Try `bench`."),
    }
}

/// Show or change the page replacement policy.
///
/// The second slot in this kernel, and it exists to show the first was not a
/// one-off. Same shape: a narrow contract, several implementations, swappable
/// under a running machine, measured rather than argued about.
fn paging_command(argument: &str) {
    if argument.is_empty() {
        println!("  page replacement policies -- `*` is installed:");
        println!();
        let active = crate::replace::active_name();
        for index in 0..crate::replace::COUNT {
            let name = crate::replace::name_at(index);
            let mark = if name == active { '*' } else { ' ' };
            println!("   {mark} {name:<7} {}", crate::replace::describe_at(index));
        }
        let (slots, evictions, faults) = crate::swap::stats();
        println!();
        println!("  {slots} swap slots in use, {evictions} pages out, {faults} brought back.");
        println!("  `paging <name>` swaps the policy. `bench paging` compares them.");
        return;
    }

    if argument == crate::replace::active_name() {
        println!("  `{argument}` is already installed.");
        return;
    }
    if !crate::replace::install_by_name(argument) {
        println!("  no policy called `{argument}`. Try `paging` for the list.");
        return;
    }
    println!("  installed `{argument}`: {}", crate::replace::active_description());
    println!("  it decides the next time a frame is wanted and none is free.");
}

/// Show or change the scheduling policy.
///
/// The point of this command is not that the machine can be tuned. It is that
/// "which scheduler is better" stops being something you are told and becomes
/// something you can do to the machine you are sitting in front of, in one
/// word, and then watch.
fn sched_command(argument: &str) {
    if argument.is_empty() {
        println!("  scheduling policies -- `*` is installed:");
        println!();
        let active = crate::sched::active_name();
        for index in 0..crate::sched::COUNT {
            let name = crate::sched::name_at(index);
            let mark = if name == active { '*' } else { ' ' };
            println!("   {mark} {name:<6}  {}", crate::sched::describe_at(index));
        }
        let (task, waited) = crate::sched::worst_wait();
        println!();
        println!("  the mechanism, not the policy, keeps this count:");
        if waited == 0 {
            println!("    no runnable task is currently being passed over.");
        } else {
            println!("    task {task} has been runnable and unpicked for {waited} ticks.");
        }
        println!("  `sched <name>` swaps the policy under the running machine.");
        return;
    }

    if argument == crate::sched::active_name() {
        println!("  `{argument}` is already installed.");
        return;
    }

    // Everything the new policy needs to be told about the world it is
    // inheriting. A fresh brick knows nothing, so installing one is a handover
    // rather than an assignment -- see `sched::install`.
    let installed = crate::sched::install_by_name(
        argument,
        crate::task::snapshot(),
        crate::task::current_id(),
        interrupts::ticks(),
    );

    if !installed {
        println!("  no policy called `{argument}`. Try `sched` for the list.");
        return;
    }

    println!("  installed `{argument}`: {}", crate::sched::active_description());
    println!("  the runnable set was replayed into it; the next tick is its decision.");
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

/// `write notes.txt hello there` -- and it survives a reboot.
fn write_file(rest: &str) {
    let rest = rest.trim();
    let Some((name, text)) = rest.split_once(' ') else {
        println!("  usage: write <file> <text>");
        return;
    };
    let Some(volume) = volume() else { return };

    match volume.create(name, text.as_bytes()) {
        Ok(()) => {
            println!("  wrote {} bytes to {name}", text.len());
            println!("  reboot and `cat {name}` -- it is on the disk, not in memory.");
        }
        Err(error) => println!("  {name}: {error}"),
    }
}

fn remove_file(name: &str) {
    let name = name.trim();
    if name.is_empty() {
        println!("  usage: rm <file>");
        return;
    }
    let Some(volume) = volume() else { return };
    match volume.remove(name) {
        Ok(()) => println!("  removed {name} (the data is still there; the slot is not)"),
        Err(error) => println!("  {name}: {error}"),
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

/// The whole address space, one line per occupied 512 GiB slot.
///
/// Every virtual address on this machine falls into one of 512 top-level slots.
/// Almost all of them are empty -- an address space is mostly a hole, and the
/// holes are what make a 64-bit address space affordable.
/// Start a second processor one step at a time, so a failure names one thing.
///
/// Doing this from the shell rather than at boot is the whole point: a fault
/// here prints, and a fault during boot resets the machine and tells you
/// nothing. The first attempt at this produced nine boot banners and no
/// information at all.
fn smp_step(step: &str) {
    println!();
    match step {
        "apic" => match crate::smp::probe_apic() {
            Ok(id) => {
                println!("  the local APIC answers. This processor is number {id}.");
                println!("  Every core reads a different value from the same address,");
                println!("  which is what makes it a *local* APIC.");
            }
            Err(error) => {
                println!("  {error}");
                println!();
                println!("  The bootloader maps physical *memory*. The APIC is");
                println!("  memory-mapped I/O above RAM, so it may simply not be in");
                println!("  that mapping -- which would fault on the first read.");
            }
        },
        "install" => match unsafe { crate::smp::install_trampoline() } {
            Ok(length) => {
                println!("  trampoline installed at 0x8000, {length} bytes.");
                println!("  identity mapped, so it survives paging being switched on");
                println!("  halfway through it.");
                println!("  progress byte is now {}", crate::smp::progress());
            }
            Err(error) => println!("  could not install it: {error}"),
        },
        "wake" => {
            println!("  sending INIT then STARTUP to processor 1 ...");
            unsafe { crate::smp::wake_one(1) };
            let progress = crate::smp::progress();
            println!();
            println!("  it got to stage {progress}:");
            println!("    0  never executed a single instruction");
            println!("    1  running, 16-bit real mode");
            println!("    2  protected mode");
            println!("    3  long mode, paging on, identity map survived");
            println!("    4  running Rust on its own stack");
            println!("    5  loaded its own GDT and IDT");
            println!("    6  past the point that used to triple fault");
            if progress >= 4 {
                println!();
                println!("  {} processors awake.", crate::smp::started());
            }
        }
        _ => println!("  usage: smp apic | smp install | smp wake"),
    }
    println!();
}

fn pagemap() {
    println!();
    println!("  The level-4 table: 512 slots, each covering 512 GiB.");
    println!();
    println!("  slot  covers                 entry");
    let mut present = 0;
    for (index, entry, base) in crate::memory::top_level_map() {
        if entry & 1 == 0 {
            continue;
        }
        present += 1;
        let what = match index {
            0 => "user programs live here",
            _ if base >= 0x1000_0000_0000 && base < 0x1100_0000_0000 => "the kernel image",
            _ => "kernel data",
        };
        println!("  {index:<5} {base:#018x}   {entry:#018x}  {what}");
    }
    println!();
    println!("  {present} of 512 slots in use. The rest is not merely unused,");
    println!("  it is unmapped -- no table exists for it at all, which is how a");
    println!("  128 TiB address space costs a few kilobytes.");
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
