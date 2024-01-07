//! The kernel's own answer engine.
//!
//! **This is not a neural network, and it is important that the repository says
//! so.** It is keyword matching over a hand-written corpus, combined with
//! decoders that read the live machine. There is no model, no training, and no
//! generation — when it answers a question, every sentence was written by a
//! human and every number was read out of a register a moment ago.
//!
//! What it is good at is exactly what a language model of a size that fits in a
//! kernel would be bad at: being *correct*. `explain_fault` decodes the real
//! error-code bits and the real faulting address and tells you what actually
//! happened, with no chance of inventing a plausible-sounding wrong answer.
//!
//! It runs with no allocator, no network, and no host process. Pull the
//! ethernet cable, kill every process on your Mac, and `ask` still works.
//!
//! The transformer in `llm.rs` is the other half of the story: real generation,
//! genuinely intelligent-looking, and far less trustworthy. Keeping the two
//! clearly separated is the point.

use crate::interrupts;
use crate::println;
use core::arch::asm;

/// One thing the kernel knows how to explain.
struct Topic {
    /// Words that suggest this topic. Matched case-insensitively as substrings,
    /// so "paging" matches a question containing "pages".
    keywords: &'static [&'static str],
    explain: fn(),
}

/// Answer a question about this machine.
pub fn answer(question: &str) {
    let mut best: Option<(&Topic, u32)> = None;

    for topic in TOPICS {
        let score = score(topic, question);
        if score > 0 && best.map_or(true, |(_, b)| score > b) {
            best = Some((topic, score));
        }
    }

    println!();
    match best {
        Some((topic, _)) => (topic.explain)(),
        None => unknown(),
    }
    println!();
}

/// How well a question matches a topic.
///
/// Longer keywords count for more: a question mentioning "double fault" should
/// beat the topic that merely matched on "fault".
fn score(topic: &Topic, question: &str) -> u32 {
    let mut total = 0;
    for keyword in topic.keywords {
        if contains_lower(question, keyword) {
            total += keyword.len() as u32;
        }
    }
    total
}

/// Case-insensitive substring search.
///
/// No allocator means no `to_lowercase()`, so the comparison lowercases as it
/// goes. ASCII only, which is all a scancode table can produce anyway.
fn contains_lower(haystack: &str, needle: &str) -> bool {
    let (h, n) = (haystack.as_bytes(), needle.as_bytes());
    if n.is_empty() || n.len() > h.len() {
        return false;
    }
    for start in 0..=(h.len() - n.len()) {
        let matches = (0..n.len())
            .all(|i| h[start + i].to_ascii_lowercase() == n[i].to_ascii_lowercase());
        if matches {
            return true;
        }
    }
    false
}

fn unknown() {
    println!("I don't have anything written down about that.");
    println!();
    println!("This is a hand-written answer engine, not a model -- it only");
    println!("knows the topics someone taught it. Those are:");
    println!();
    println!("  paging   memory   stack    gdt      idt");
    println!("  faults   timer    keyboard serial   rings");
    println!("  boot     cpu      thinking");
    println!();
    println!("For open-ended questions, `llm <prompt>` runs the neural network");
    println!("instead -- it will always answer, and it is much easier to");
    println!("believe when it is wrong.");
}

// --- live-state helpers --------------------------------------------------

fn control_registers() -> (u64, u64, u64, u64) {
    let (cr0, cr2, cr3, cr4);
    unsafe {
        asm!(
            "mov {cr0}, cr0", "mov {cr2}, cr2", "mov {cr3}, cr3", "mov {cr4}, cr4",
            cr0 = out(reg) cr0, cr2 = out(reg) cr2,
            cr3 = out(reg) cr3, cr4 = out(reg) cr4,
            options(nomem, nostack, preserves_flags),
        );
    }
    (cr0, cr2, cr3, cr4)
}

fn stack_pointer() -> u64 {
    let rsp;
    unsafe { asm!("mov {}, rsp", out(reg) rsp, options(nomem, nostack, preserves_flags)) };
    rsp
}

// --- fault analysis ------------------------------------------------------

/// Explain a fault the kernel just took, from the real hardware values.
///
/// Called from the fault handler itself: interrupts are off and the stack may
/// be damaged, so this allocates nothing and calls nothing that can fault.
pub fn explain_fault(kind: &str, rip: u64, error: u64, cr2: u64) {
    println!("[why ] Reading the fault, not guessing at it:");
    println!();

    match kind {
        "page_fault" => explain_page_fault(rip, error, cr2),
        "double_fault" => {
            println!("       A DOUBLE FAULT means a fault happened while the CPU was");
            println!("       already delivering a fault. The first one is the real bug;");
            println!("       this is only the consequence.");
            println!();
            println!("       The overwhelmingly common cause is stack exhaustion: the");
            println!("       kernel stack overflowed, so pushing the page-fault frame");
            println!("       faulted too. Look for unbounded recursion or a large stack");
            println!("       array in the path leading to {rip:#x}.");
        }
        "general_protection" => explain_general_protection(rip, error),
        _ => println!("       Fault type: {kind} at {rip:#x}, error code {error:#x}."),
    }
}

fn explain_general_protection(rip: u64, error: u64) {
    println!("       A general protection fault is the CPU's catch-all refusal:");
    println!("       an operation that is illegal for reasons other than a");
    println!("       missing page. Note there is no CR2 here -- unlike a page");
    println!("       fault, #GP does not record an address.");
    println!();

    if error == 0 {
        println!("       The error code is 0, which rules out the descriptor-table");
        println!("       causes (those put a selector index here). In 64-bit mode");
        println!("       the usual remaining cause is a NON-CANONICAL ADDRESS.");
        println!();
        println!("       A 64-bit pointer is not really 64-bit: bits 48-63 must all");
        println!("       copy bit 47. Anything else is rejected before the CPU even");
        println!("       consults the page tables -- which is why this arrives as");
        println!("       #GP rather than the page fault you might have expected.");
        println!();
        println!("       So the pointer is not merely wrong, it is malformed. Look");
        println!("       for a corrupted or partially-overwritten pointer, or an");
        println!("       integer that was cast to a pointer.");
    } else {
        let external = error & 1 != 0;
        let table = (error >> 1) & 0b11;
        let index = error >> 3;
        println!("       The error code names a descriptor: index {index}, in the");
        println!(
            "       {} table{}.",
            match table {
                0 => "GDT",
                1 | 3 => "IDT",
                _ => "LDT",
            },
            if external { ", from an external event" } else { "" },
        );
        println!();
        println!("       That means a segment or gate was loaded or used illegally");
        println!("       -- check the descriptor at that index.");
    }

    println!();
    println!("       The offending instruction is at {rip:#x}.");
}

fn explain_page_fault(rip: u64, error: u64, cr2: u64) {
    let present = error & 1 != 0;
    let write = (error >> 1) & 1 != 0;
    let user = (error >> 2) & 1 != 0;
    let reserved = (error >> 3) & 1 != 0;
    let instruction_fetch = (error >> 4) & 1 != 0;

    println!("       The CPU could not translate {cr2:#x}.");
    println!();
    println!(
        "       It was a {} {}, from ring {}.",
        if write { "write" } else { "read" },
        if instruction_fetch { "during an instruction fetch" } else { "of data" },
        if user { 3 } else { 0 },
    );

    if present {
        println!("       The page IS mapped -- this is a permission violation, not a");
        println!("       missing page. Something wrote to a read-only page, or ring 3");
        println!("       touched a kernel-only one.");
    } else {
        println!("       The page is not mapped at all. Nothing in the page table tree");
        println!("       rooted at CR3 covers that address.");
    }
    if reserved {
        println!("       A reserved bit was set in a page table entry -- that means the");
        println!("       table itself is malformed, not just the mapping.");
    }

    println!();
    println!("       Most likely cause, judging by the address:");
    for line in diagnose_address(cr2) {
        println!("       {line}");
    }

    println!();
    println!("       What a complete kernel would do here: decide whether {cr2:#x}");
    println!("       *should* be valid. If yes (a growable stack, a lazily-mapped");
    println!("       heap page, a swapped-out page) allocate a frame, map it, and");
    println!("       return -- the faulting instruction re-runs and succeeds. If no,");
    println!("       kill the offending process. 5AM-OS has neither an allocator nor");
    println!("       processes, so it can only halt.");
    println!();
    println!("       The faulting instruction is at {rip:#x}. Disassemble the kernel");
    println!("       and look there -- that is the actual bug.");
}

/// Classify a faulting address by where it landed.
///
/// Returns pre-wrapped lines: the console is 80 columns with no wrapping of
/// its own, so anything long has to arrive already broken up.
fn diagnose_address(address: u64) -> &'static [&'static str] {
    // Bits 48-63 must all match bit 47 to be a canonical address.
    let canonical = {
        let top = address >> 47;
        top == 0 || top == 0x1FFFF
    };

    if !canonical {
        // Rare here: the CPU rejects non-canonical addresses with #GP before
        // it ever walks the page tables, so this branch is mostly unreachable
        // via a page fault. Kept because CR2 can hold a stale value.
        return &[
            "not a canonical address -- bits 48-63 must all copy bit 47.",
            "Note this normally arrives as a #GP, not a page fault, so",
            "CR2 may simply be stale from an earlier fault.",
        ];
    }
    match address {
        0 => &[
            "a null pointer dereference. Something unwrapped a null.",
        ],
        1..=0xFFF => &[
            "inside the null page -- a null pointer plus a small offset,",
            "so almost certainly a field access through a null struct.",
        ],
        0x1000..=0xFFFF => &[
            "very low memory. Usually a small integer used as a pointer.",
        ],
        _ => &[
            "a plausible-looking address that simply is not mapped.",
            "Either the pointer is garbage, or the mapping was never made.",
        ],
    }
}

// --- the corpus ----------------------------------------------------------

static TOPICS: &[Topic] = &[
    Topic {
        keywords: &["paging", "page table", "cr3", "virtual memory", "translate", "mmu", "tlb"],
        explain: || {
            let (_, _, cr3, cr4) = control_registers();
            println!("PAGING -- why your addresses are fiction");
            println!();
            println!("  CR3 = {cr3:#018x}");
            println!("  That is a PHYSICAL address, and it is the root of a four-level");
            println!("  tree. Every memory access you make is translated through it.");
            println!();
            println!("  A 64-bit virtual address is not one number. It is five:");
            println!("    bits 47-39  index into the level-4 table (at CR3)");
            println!("    bits 38-30  index into the level-3 table");
            println!("    bits 29-21  index into the level-2 table");
            println!("    bits 20-12  index into the level-1 table");
            println!("    bits 11-0   offset inside the 4KiB frame");
            println!();
            println!("  So one memory read is really five: four table walks plus the");
            println!("  actual access. That is why the TLB exists -- it caches the");
            println!("  result so the walk happens once, not every time.");
            println!();
            println!("  PAE (CR4 bit 5) = {}. 64-bit paging cannot work without it.",
                (cr4 >> 5) & 1);
            println!();
            println!("  5AM-OS does not build these tables -- the bootloader did, and");
            println!("  we inherited them. Writing our own is the next real milestone.");
        },
    },
    Topic {
        keywords: &["memory", "ram", "allocator", "heap", "malloc", "physical"],
        explain: || {
            println!("MEMORY -- what this kernel does and does not have");
            println!();
            println!("  There is no allocator. None. That is why you will not find a");
            println!("  String, a Vec, or a Box anywhere in this codebase -- every");
            println!("  buffer is a fixed-size array decided at compile time.");
            println!();
            println!("  Run `mem` to see the firmware's map. The thing to notice is");
            println!("  that physical RAM is not one clean block: it is a dozen");
            println!("  fragments separated by firmware code, ACPI tables and");
            println!("  memory-mapped devices.");
            println!();
            println!("  That fragmentation is the entire reason paging exists. Page");
            println!("  tables let a kernel hand out one tidy contiguous virtual space");
            println!("  built out of whatever physical scraps are actually available.");
            println!();
            println!("  Writing an allocator means: track which physical frames are");
            println!("  free (a bitmap or a free list), map them into virtual space on");
            println!("  demand, and implement Rust's GlobalAlloc on top. That single");
            println!("  step is what unlocks String, Vec, and everything built on them.");
        },
    },
    Topic {
        keywords: &["stack", "rsp", "overflow", "recursion", "ist"],
        explain: || {
            let rsp = stack_pointer();
            println!("THE STACK");
            println!();
            println!("  RSP = {rsp:#018x}   (right now, inside this command)");
            println!();
            println!("  It grows DOWNWARD. Every call pushes a return address and the");
            println!("  callee's locals; every return pops them. Nothing checks the");
            println!("  bottom -- run out of stack and you simply write into whatever");
            println!("  is mapped below it.");
            println!();
            println!("  That is why stack overflow is so dangerous in a kernel. The");
            println!("  CPU tries to push a fault frame onto the broken stack, fails,");
            println!("  escalates to a double fault, tries to push again onto the same");
            println!("  broken stack, fails again, and triple faults -- which is not an");
            println!("  exception at all. It is the CPU resetting the machine.");
            println!();
            println!("  5AM-OS survives that because the double fault handler is given");
            println!("  its own known-good stack through the IST (see gdt.rs). Try it:");
            println!("  `fault stack`.");
        },
    },
    Topic {
        keywords: &["gdt", "segment", "descriptor", "tss", "selector"],
        explain: || {
            let (base, limit) = crate::gdt::current();
            println!("GDT -- the CPU's list of segments");
            println!();
            println!("  Live: base {base:#018x}, limit {limit} ({} entries)", (limit + 1) / 8);
            println!();
            println!("  In 64-bit mode segmentation is mostly a fossil: base and limit");
            println!("  are ignored and every segment covers all of memory. Two things");
            println!("  still matter.");
            println!();
            println!("  The privilege level -- ring 0 versus ring 3.");
            println!();
            println!("  And the TSS, which is where the CPU looks to find a known-good");
            println!("  stack when the current one cannot be used. That is the only");
            println!("  reason a stack overflow here prints a message instead of");
            println!("  rebooting the machine. Run `gdt` to decode the real bytes.");
        },
    },
    Topic {
        keywords: &["idt", "interrupt", "irq", "handler", "vector", "pic", "exception"],
        explain: || {
            let (base, limit) = interrupts::current();
            println!("INTERRUPTS");
            println!();
            println!("  IDT live at {base:#018x}, limit {limit}.");
            println!("  Timer ticks so far: {}", interrupts::ticks());
            println!();
            println!("  That number is climbing without any code of yours running. A");
            println!("  handler fires ~18.2 times a second entirely behind your back.");
            println!();
            println!("  Vectors 0-31 belong to Intel -- the CPU raises them AT you when");
            println!("  you make a mistake. 32 and up are yours. The 8259 PIC defaults");
            println!("  to 8-15, which collides with Intel's range, so remapping it to");
            println!("  32-47 is not an optimisation -- an unremapped timer tick would");
            println!("  arrive as a double fault.");
            println!();
            println!("  Without interrupts a kernel must poll, and polling means");
            println!("  burning a whole core to notice a keypress.");
        },
    },
    Topic {
        keywords: &["fault", "crash", "panic", "trap", "double fault", "triple fault", "wrong"],
        explain: || {
            println!("FAULTS");
            println!();
            println!("  A fault is the CPU refusing and telling you why. The kinds this");
            println!("  kernel catches:");
            println!();
            println!("    0   divide by zero");
            println!("    3   breakpoint (int3 -- what a debugger patches in)");
            println!("    6   invalid opcode");
            println!("    8   double fault    [runs on its own IST stack]");
            println!("    13  general protection fault");
            println!("    14  page fault");
            println!();
            println!("  Anything not in that list has no handler, which makes it a");
            println!("  double fault, which we do handle.");
            println!();
            println!("  When one fires, this engine decodes the real error-code bits");
            println!("  and the real faulting address rather than guessing. Break the");
            println!("  machine on purpose and watch: `fault page`.");
        },
    },
    Topic {
        keywords: &["timer", "pit", "tick", "clock", "time", "uptime"],
        explain: || {
            let ticks = interrupts::ticks();
            println!("THE TIMER");
            println!();
            println!("  {ticks} ticks so far -- about {} seconds.", ticks / 18);
            println!();
            println!("  The PIT free-runs at ~18.2 Hz because that is the divisor the");
            println!("  BIOS left in it and nobody has reprogrammed it. That number is");
            println!("  a 1981 accident: 1.193182 MHz divided by 65536.");
            println!();
            println!("  This is the kernel's only sense of time. It is far too coarse");
            println!("  to schedule with -- a real kernel reprograms the PIT to 1000 Hz,");
            println!("  or better, uses the local APIC timer and the TSC.");
        },
    },
    Topic {
        keywords: &["keyboard", "scancode", "key", "typing", "ps/2", "input"],
        explain: || {
            println!("THE KEYBOARD");
            println!();
            println!("  It does not send letters. It sends one number when a key goes");
            println!("  down and a different one when it comes up. 'A' is not something");
            println!("  the hardware knows -- this kernel decides that scancode 0x1E");
            println!("  plus shift means 'A'. That is why layouts are software.");
            println!();
            println!("  Two bugs cost days here, and both were silent:");
            println!();
            println!("  Unmasking IRQ1 in the PIC is not enough. The i8042 controller");
            println!("  has its own configuration byte, and bit 0 of it decides whether");
            println!("  the thing raises an interrupt at all. It was clear. Keys went");
            println!("  in, nothing came out, no error anywhere.");
            println!();
            println!("  Then IRQ1 fired with correct scancodes and the shell still saw");
            println!("  nothing -- the compiler had cached the queue index, because it");
            println!("  cannot see that an interrupt writes to it. That is what");
            println!("  read_volatile is for.");
        },
    },
    Topic {
        keywords: &["serial", "uart", "console", "com1", "com2", "16550", "output"],
        explain: || {
            println!("SERIAL");
            println!();
            println!("  A 16550 UART at port 0x3F8. Writing one byte there puts a");
            println!("  character on the wire -- that is the whole driver.");
            println!();
            println!("  x86 has a second address space for devices, reachable only by");
            println!("  the `in` and `out` instructions. That is why serial works with");
            println!("  almost no setup while the screen needs a framebuffer, a font,");
            println!("  and a glyph rasteriser before it can say one word.");
            println!();
            println!("  It is also the only way this kernel can reach the outside");
            println!("  world at all: no network stack, no disk. COM2 is wired to the");
            println!("  optional host bridge for exactly that reason.");
        },
    },
    Topic {
        keywords: &["ring", "privilege", "kernel mode", "user mode", "ring 0", "ring 3", "permission"],
        explain: || {
            let cs: u16;
            unsafe { asm!("mov {0:x}, cs", out(reg) cs, options(nomem, nostack)) };
            println!("PRIVILEGE RINGS");
            println!();
            println!("  CS = {cs:#06x}, so you are in ring {}.", cs & 0b11);
            println!();
            println!("  Ring 0 can execute any instruction, read any register, touch");
            println!("  any memory. Ring 3 cannot do I/O, cannot load descriptor");
            println!("  tables, cannot disable interrupts.");
            println!();
            println!("  Every program you have ever run was in ring 3, asking a ring 0");
            println!("  kernel for permission. Right now you are the thing that grants");
            println!("  permission -- there is nothing above you to say no.");
            println!();
            println!("  x86 has four rings. Essentially nobody uses 1 and 2.");
        },
    },
    Topic {
        keywords: &["boot", "startup", "real mode", "long mode", "bootloader", "power on"],
        explain: || {
            println!("BOOT");
            println!();
            println!("  The CPU powers on in 16-bit real mode -- the same mode an 8086");
            println!("  booted in, in 1978. 1MB addressable, no memory protection.");
            println!();
            println!("  Getting to 64-bit long mode means, in this exact order: build a");
            println!("  GDT, enable protected mode, build page tables, enable PAE, set");
            println!("  the long mode bit, enable paging, far jump. Any mistake is a");
            println!("  triple fault with no message.");
            println!();
            println!("  That walk is currently done for us by the bootloader crate. It");
            println!("  is the single most interesting part of boot and it is borrowed,");
            println!("  which is why replacing it is on the list.");
        },
    },
    Topic {
        keywords: &["cpu", "register", "cr0", "cr4", "control register", "state", "rflags"],
        explain: || {
            let (cr0, cr2, cr3, cr4) = control_registers();
            let rsp = stack_pointer();
            println!("THE MACHINE, RIGHT NOW");
            println!();
            println!("  CR0 = {cr0:#018x}   PE={} PG={}", cr0 & 1, (cr0 >> 31) & 1);
            println!("        bit 0 is protected mode, bit 31 is paging. Those two bits");
            println!("        are the difference between a 1978 machine and this one.");
            println!("  CR2 = {cr2:#018x}   last address that failed to translate");
            println!("  CR3 = {cr3:#018x}   root of the page table tree");
            println!("  CR4 = {cr4:#018x}   PAE={}", (cr4 >> 5) & 1);
            println!("  RSP = {rsp:#018x}");
            println!();
            println!("  These are readable only in ring 0. In any program you have");
            println!("  written before, reading CR3 would have been an instant fault.");
        },
    },
    Topic {
        keywords: &["thinking", "ai", "model", "neural", "llm", "intelligent", "how do you work"],
        explain: || {
            println!("HOW THIS ANSWER WAS PRODUCED");
            println!();
            println!("  Honestly: by keyword matching. You typed a question, this");
            println!("  kernel scored it against a list of topics a human wrote, picked");
            println!("  the best match, and ran a function that reads live registers.");
            println!();
            println!("  There is no model here and no generation. Every sentence you");
            println!("  are reading was written by a person; every number was read out");
            println!("  of the hardware a moment ago. That is why it can be trusted on");
            println!("  a fault report -- it cannot invent a plausible wrong answer,");
            println!("  because it cannot invent anything.");
            println!();
            println!("  `llm <prompt>` is the opposite trade: a real transformer, real");
            println!("  generation, running in ring 0 with no OS beneath it -- and no");
            println!("  idea what a page fault is. Both are honest about what they are.");
        },
    },
];
