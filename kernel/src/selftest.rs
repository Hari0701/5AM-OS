//! Tests that run inside the machine they are testing.
//!
//! This kernel had none for most of its life, and the cost is written all over
//! its history: an interrupt flag that was never restored, a loader that leaked
//! every page it mapped, a semaphore whose count the compiler read once and
//! cached forever. Every one of those shipped because the transcript *looked*
//! right. A transcript is not a test if you only wrote down what you were
//! hoping to see.
//!
//! ## Why they run in ring 0 and not on your laptop
//!
//! Most of what this kernel does cannot be tested anywhere else. `cargo test`
//! on the host can check that an ELF header parses, but it cannot map a page,
//! take a real page fault, switch a stack, or discover that the timer never
//! fires again. The interesting failures are all failures of the *machine*, and
//! the only honest place to look for them is on it.
//!
//! So: `selftest` in the shell. Each check prints its own line, the suite
//! prints a count, and a learner who has just reimplemented the frame allocator
//! finds out in two seconds whether it works.
//!
//! ## Writing a check
//!
//! Prove behaviour, not the absence of a crash. `heap: alloc did not panic` is
//! worthless; `heap: 400 interleaved cycles return every byte and leave one
//! hole` is the property that actually breaks when coalescing is wrong.

use crate::{memory, println};
use alloc::boxed::Box;
use alloc::vec;
use alloc::vec::Vec;

pub struct Report {
    pub passed: usize,
    pub failed: usize,
}

impl Report {
    fn new() -> Self {
        Self {
            passed: 0,
            failed: 0,
        }
    }

    /// Record one check. `detail` is printed either way, because a passing
    /// check that prints its measurement is documentation and a passing check
    /// that prints "ok" is noise.
    fn check(&mut self, name: &str, passed: bool, detail: core::fmt::Arguments) {
        if passed {
            self.passed += 1;
            println!("    pass  {name:<28} {detail}");
        } else {
            self.failed += 1;
            println!("    FAIL  {name:<28} {detail}");
        }
    }
}

macro_rules! check {
    ($report:expr, $name:expr, $condition:expr, $($detail:tt)*) => {
        $report.check($name, $condition, format_args!($($detail)*))
    };
}

/// Run one suite, or every suite.
pub fn run(which: &str) {
    let mut report = Report::new();

    let all = which.is_empty() || which == "all";
    if all || which == "heap" {
        println!("  heap");
        heap(&mut report);
    }
    if all || which == "memory" {
        println!("  memory");
        memory_suite(&mut report);
    }
    if all || which == "space" {
        println!("  address space");
        address_space(&mut report);
    }
    if all || which == "cow" {
        println!("  copy on write");
        cow(&mut report);
    }
    if all || which == "pipe" {
        println!("  pipes");
        pipes(&mut report);
    }
    if all || which == "swap" {
        println!("  swapping");
        swapping(&mut report);
    }
    if all || which == "sync" {
        println!("  sync");
        sync(&mut report);
    }
    if all || which == "sched" {
        println!("  sched");
        sched(&mut report);
    }
    if all || which == "claims" {
        println!("  claims this repository makes about itself");
        claims(&mut report);
    }
    if all || which == "replace" {
        println!("  page replacement policies");
        replacers(&mut report);
    }
    if all || which == "policy" {
        println!("  scheduling policies");
        sched_policy(&mut report);
    }
    if all || which == "priority" {
        println!("  priority");
        priority(&mut report);
    }
    if all || which == "elf" {
        println!("  elf");
        elf(&mut report);
    }
    if all || which == "fat" {
        println!("  fat");
        fat(&mut report);
    }

    println!();
    if report.failed == 0 {
        println!("  {} passed, 0 failed.", report.passed);
    } else {
        println!(
            "  {} passed, {} FAILED.",
            report.passed, report.failed
        );
    }
}

// --- suites --------------------------------------------------------------

fn heap(report: &mut Report) {
    let before = crate::heap::stats();

    // Allocation has to survive being used, not merely return a pointer.
    let mut values: Vec<u64> = Vec::new();
    for index in 0..64u64 {
        values.push(index * index);
    }
    check!(
        report,
        "vec grows and keeps values",
        values[63] == 63 * 63 && values.len() == 64,
        "last = {}",
        values[63]
    );

    // Alignment is the requirement a naive bump allocator quietly ignores until
    // something does an aligned SSE store into the memory it handed out.
    let aligned: Box<u64> = Box::new(0xDEAD_BEEF);
    let address = &*aligned as *const u64 as usize;
    check!(
        report,
        "u64 is 8-byte aligned",
        address % 8 == 0,
        "at {address:#x}"
    );

    drop(values);
    drop(aligned);

    let after_drop = crate::heap::stats();
    check!(
        report,
        "every byte comes back",
        after_drop.0 == before.0,
        "{} free before, {} after",
        before.0,
        after_drop.0
    );

    // The property that actually breaks: without coalescing, the hole count
    // climbs on every cycle until a large allocation cannot fit.
    for _ in 0..200 {
        let a: Vec<u8> = vec![0; 96];
        let b: Vec<u8> = vec![0; 300];
        drop(a);
        let c: Vec<u8> = vec![0; 64];
        drop(b);
        drop(c);
    }
    let after_cycles = crate::heap::stats();
    // Compare the hole count to what it was before the cycles, not to one.
    //
    // Asserting "exactly one hole" quietly assumed the heap was otherwise
    // empty, which stopped being true the moment the kernel kept any long-lived
    // allocation -- the copy-on-write share table, for instance. A live
    // allocation splits the free space in two and the check failed, reporting a
    // leak that was not there. The property that actually matters is that
    // coalescing puts the heap back exactly as it found it.
    check!(
        report,
        "200 cycles change nothing",
        after_cycles.1 == before.1 && after_cycles.0 == before.0,
        "{} holes and {} bytes free, started at {} and {}",
        after_cycles.1,
        after_cycles.0,
        before.1,
        before.0
    );
}

fn memory_suite(report: &mut Report) {
    let (free_before, _) = memory::allocator().stats();

    let Some(frame) = memory::allocator().allocate() else {
        check!(report, "allocate a frame", false, "allocator is empty");
        return;
    };

    // Somewhere in userspace territory that nothing else is using.
    const SCRATCH: u64 = 0x0000_3000_0000;

    let mapped = unsafe { memory::map_page(SCRATCH, frame, memory::FLAG_WRITABLE) };
    check!(
        report,
        "map a page",
        mapped.is_ok(),
        "{SCRATCH:#x} -> {frame:#x}"
    );
    if mapped.is_err() {
        unsafe { memory::allocator().deallocate(frame) };
        return;
    }

    // A mapping you cannot write to is not a mapping.
    const MARKER: u64 = 0x5A4D_0501_5A4D_0501;
    unsafe {
        core::ptr::write_volatile(SCRATCH as *mut u64, MARKER);
    }
    let read_back = unsafe { core::ptr::read_volatile(SCRATCH as *const u64) };
    check!(
        report,
        "the page is writable",
        read_back == MARKER,
        "read {read_back:#x}"
    );

    // The walk must agree with what we asked for. This is the check that fails
    // when the four table indexes are computed with the wrong shifts.
    match memory::translate(SCRATCH) {
        Some((physical, _)) => check!(
            report,
            "translate finds the frame",
            physical == frame,
            "walk says {physical:#x}, expected {frame:#x}"
        ),
        None => check!(report, "translate finds the frame", false, "not mapped"),
    }

    // And unmapping must actually unmap, or the next owner of this frame shares
    // it with whoever held it before.
    let returned = unsafe { memory::unmap_page(SCRATCH) };
    check!(
        report,
        "unmap returns the frame",
        returned == Some(frame),
        "{returned:?}"
    );
    check!(
        report,
        "translate now fails",
        memory::translate(SCRATCH).is_none(),
        "the address no longer resolves"
    );

    unsafe { memory::allocator().deallocate(frame) };
    let (free_after, _) = memory::allocator().stats();
    check!(
        report,
        "no frames leaked",
        free_after == free_before,
        "{free_before} before, {free_after} after"
    );
}

/// Does a private address space actually hide anything?
///
/// Map a page in a new space, then check the kernel's own space cannot see it,
/// and that the kernel's mappings are still reachable from inside the new one.
/// Those two together are the whole claim: private below, shared above.
fn address_space(report: &mut Report) {
    let (free_before, _) = memory::allocator().stats();

    let Some(space) = memory::AddressSpace::new() else {
        check!(report, "create an address space", false, "out of memory");
        return;
    };
    check!(
        report,
        "create an address space",
        space.root() != memory::active_root(),
        "root {:#x}, kernel {:#x}",
        space.root(),
        memory::active_root()
    );

    const PRIVATE: u64 = 0x0000_0040_0000;
    let Some(frame) = memory::allocator().allocate() else {
        check!(report, "allocate a frame", false, "out of memory");
        return;
    };
    let flags = memory::FLAG_USER | memory::FLAG_WRITABLE;
    let mapped = unsafe { space.map(PRIVATE, frame, flags) };
    check!(report, "map into the new space", mapped.is_ok(), "{mapped:?}");

    check!(
        report,
        "the new space sees it",
        space.translate(PRIVATE).map(|(p, _)| p) == Some(frame),
        "{:?}",
        space.translate(PRIVATE).map(|(p, _)| p)
    );

    // The point of the whole exercise.
    check!(
        report,
        "the kernel space does not",
        memory::translate(PRIVATE).is_none(),
        "the private mapping leaked into the active space"
    );

    // And the kernel must still be reachable from inside it, or the first
    // interrupt after a switch is a triple fault.
    let kernel_address = memory::active_root;
    let probe = kernel_address as usize as u64;
    check!(
        report,
        "the kernel is mapped there too",
        space.translate(probe).is_some(),
        "kernel code at {probe:#x} unreachable from the new space"
    );

    // A space refuses to map anything above the user boundary.
    check!(
        report,
        "refuses a kernel address",
        unsafe { space.map(memory::USER_SPACE_END, frame, flags) }.is_err(),
        "accepted a kernel-half mapping"
    );

    let freed = unsafe { space.destroy() };
    let (free_after, _) = memory::allocator().stats();
    check!(
        report,
        "destroy returns everything",
        free_after == free_before,
        "{freed} frames freed, {free_before} -> {free_after}"
    );
}

/// Does fork share pages, and does the first write really un-share them?
fn cow(report: &mut Report) {
    let (free_before, _) = memory::allocator().stats();
    let Some(parent) = memory::AddressSpace::new() else {
        check!(report, "create a parent space", false, "out of memory");
        return;
    };

    const PAGE: u64 = 0x0000_0080_0000;
    let Some(frame) = memory::allocator().allocate() else {
        return;
    };
    let flags = memory::FLAG_USER | memory::FLAG_WRITABLE;
    if unsafe { parent.map(PAGE, frame, flags) }.is_err() {
        check!(report, "map a writable page", false, "map failed");
        return;
    }

    let Some(child) = (unsafe { parent.fork() }) else {
        check!(report, "fork the space", false, "out of memory");
        return;
    };

    // Nothing is copied yet: both must name the same physical frame.
    let parent_frame = parent.translate(PAGE).map(|(p, _)| p);
    let child_frame = child.translate(PAGE).map(|(p, _)| p);
    check!(
        report,
        "fork copies no data",
        parent_frame == Some(frame) && child_frame == Some(frame),
        "both at {frame:#x}"
    );

    check!(
        report,
        "the frame is marked shared",
        memory::is_shared(frame),
        "refcount did not rise"
    );

    // Writing has to go through the fault handler, which needs the space to be
    // active. Switch, write, switch back.
    let kernel_root = memory::active_root();
    unsafe {
        parent.activate();
        core::ptr::write_volatile(PAGE as *mut u64, 0xC0FFEE);
        memory::activate_root(kernel_root);
    }

    let after = parent.translate(PAGE).map(|(p, _)| p);
    check!(
        report,
        "writing gives a private copy",
        after.is_some() && after != Some(frame),
        "parent moved from {frame:#x} to {after:?}"
    );
    check!(
        report,
        "the other side keeps the original",
        child.translate(PAGE).map(|(p, _)| p) == Some(frame),
        "child still at {frame:#x}"
    );

    unsafe {
        child.destroy();
        parent.destroy();
    }
    let (free_after, _) = memory::allocator().stats();
    check!(
        report,
        "both spaces free cleanly",
        free_after == free_before,
        "{free_before} -> {free_after}"
    );
}

/// A pipe is a buffer; what makes it a pipe is what happens at the edges.
fn pipes(report: &mut Report) {
    let Some(id) = crate::pipe::create() else {
        check!(report, "create a pipe", false, "none free");
        return;
    };
    check!(report, "create a pipe", crate::pipe::in_use(id), "pipe {id}");

    let written = crate::pipe::write(id, b"hello");
    let mut buffer = [0u8; 16];
    let read = crate::pipe::read(id, &mut buffer);
    check!(
        report,
        "bytes come out in order",
        written == 5 && read == 5 && &buffer[..5] == b"hello",
        "wrote {written}, read {read}"
    );

    // Wrapping is the part a naive ring buffer gets wrong: fill it, drain it,
    // then do it again so head and tail are past the end of the array.
    let big = [b'x'; crate::pipe::CAPACITY];
    let filled = crate::pipe::write(id, &big);
    let mut drain = [0u8; crate::pipe::CAPACITY];
    let drained = crate::pipe::read(id, &mut drain);
    let again = crate::pipe::write(id, b"wrapped");
    let mut tail = [0u8; 8];
    let got = crate::pipe::read(id, &mut tail);
    check!(
        report,
        "the ring wraps",
        filled == crate::pipe::CAPACITY && drained == crate::pipe::CAPACITY && again == 7 && &tail[..7] == b"wrapped",
        "{filled} in, {drained} out, then {again} in and {got} out"
    );

    // End of file is a reference count, not a marker in the data.
    crate::pipe::close_writer(id);
    let mut empty = [0u8; 8];
    let eof = crate::pipe::read(id, &mut empty);
    check!(
        report,
        "no writers means end of file",
        eof == 0,
        "read returned {eof}"
    );

    crate::pipe::close_reader(id);
    check!(
        report,
        "both ends closed frees it",
        !crate::pipe::in_use(id),
        "still in use"
    );

    // And a write with nobody listening must not pretend to succeed.
    let Some(other) = crate::pipe::create() else {
        return;
    };
    crate::pipe::close_reader(other);
    let refused = crate::pipe::write(other, b"nobody there");
    check!(
        report,
        "no readers means a short write",
        refused == 0,
        "wrote {refused} bytes into a pipe with no reader"
    );
    crate::pipe::close_writer(other);
}

/// A page written to disk, its frame taken away, and the same bytes back.
///
/// The whole claim of swapping in one check: the contents survive somewhere
/// that is not memory, and the address keeps working.
fn swapping(report: &mut Report) {
    if !crate::ata::present() {
        println!("    skip  no disk attached");
        return;
    }

    let (free_before, _) = memory::allocator().stats();
    let Some(space) = memory::AddressSpace::new() else {
        check!(report, "create a space", false, "out of memory");
        return;
    };

    const BASE: u64 = 0x0000_0060_0000;
    const PAGES: u64 = 4;
    let flags = memory::FLAG_USER | memory::FLAG_WRITABLE;
    for index in 0..PAGES {
        let Some(frame) = memory::allocator().allocate() else {
            return;
        };
        if unsafe { space.map(BASE + index * 4096, frame, flags) }.is_err() {
            return;
        }
    }

    // Fill each page with a pattern that says which page it is, so a mix-up
    // between slots shows up as wrong data rather than as nothing at all.
    let kernel_root = memory::active_root();
    unsafe { space.activate() };
    for index in 0..PAGES {
        let address = BASE + index * 4096;
        for offset in (0..4096u64).step_by(512) {
            unsafe { core::ptr::write_volatile((address + offset) as *mut u64, 0xA000 + index) };
        }
    }

    let evicted = unsafe { memory::evict_one(space.root()) };
    check!(
        report,
        "evicting returns a frame",
        evicted.is_some(),
        "{evicted:?}"
    );
    if let Some(frame) = evicted {
        unsafe { memory::allocator().deallocate(frame) };
    }

    // Exactly one of the four should now be out on disk.
    let mut swapped = 0;
    let mut swapped_address = 0;
    for index in 0..PAGES {
        let address = BASE + index * 4096;
        if let Some(entry) = memory::leaf_entry(space.root(), address) {
            if memory::is_swapped_entry(entry) {
                swapped += 1;
                swapped_address = address;
            }
        }
    }
    check!(
        report,
        "one page is out on disk",
        swapped == 1,
        "{swapped} of {PAGES} swapped"
    );

    let (slots, evictions, _) = crate::swap::stats();
    check!(
        report,
        "a slot is in use",
        slots >= 1 && evictions >= 1,
        "{slots} slots, {evictions} evictions"
    );

    // Touching it faults, and the fault handler brings it back. If the bytes
    // are right, they went to a disk and came back.
    let expected = 0xA000 + (swapped_address - BASE) / 4096;
    let read_back = unsafe { core::ptr::read_volatile(swapped_address as *const u64) };
    let tail = unsafe { core::ptr::read_volatile((swapped_address + 3584) as *const u64) };
    check!(
        report,
        "the same bytes come back",
        read_back == expected && tail == expected,
        "read {read_back:#x} and {tail:#x}, expected {expected:#x}"
    );

    check!(
        report,
        "and it is present again",
        memory::translate(swapped_address).is_some(),
        "mapped again at {:?}",
        memory::translate(swapped_address).map(|(p, _)| p)
    );

    unsafe {
        memory::activate_root(kernel_root);
        space.destroy();
    }
    let (free_after, _) = memory::allocator().stats();
    check!(
        report,
        "no frames lost",
        free_after == free_before,
        "{free_before} -> {free_after}"
    );
}

fn sync(report: &mut Report) {
    use crate::sync::Semaphore;

    let semaphore = Semaphore::new(2);
    check!(
        report,
        "starts with its count",
        semaphore.count() == 2,
        "count = {}",
        semaphore.count()
    );

    check!(
        report,
        "try_wait takes one",
        semaphore.try_wait() && semaphore.count() == 1,
        "count = {}",
        semaphore.count()
    );

    semaphore.try_wait();
    check!(
        report,
        "try_wait fails at zero",
        !semaphore.try_wait(),
        "count = {}",
        semaphore.count()
    );

    semaphore.signal();
    check!(
        report,
        "signal gives one back",
        semaphore.count() == 1,
        "count = {}",
        semaphore.count()
    );
}

fn sched(report: &mut Report) {
    let start = crate::interrupts::ticks();
    crate::task::sleep(5);
    let slept = crate::interrupts::ticks() - start;
    check!(
        report,
        "sleep waits at least as long",
        slept >= 5,
        "asked 5, slept {slept}"
    );

    // The real one: three tasks, one semaphore, a shared counter. This is the
    // test that would have caught the compiler caching the semaphore's count --
    // it deadlocked with the resource free and two tasks asleep.
    crate::task::reset_counter();
    let mut ids = [0usize; 3];
    let mut spawned = true;
    for (index, slot) in ids.iter_mut().enumerate() {
        match crate::task::spawn("test", crate::task::Work::Worker(index)) {
            Ok(id) => *slot = id,
            Err(_) => spawned = false,
        }
    }
    if !spawned {
        check!(report, "spawn three workers", false, "no free task slots");
        return;
    }
    for id in ids {
        crate::task::wait_for(id);
    }
    let total = crate::task::counter_value();
    check!(
        report,
        "nine increments, none lost",
        total == 9,
        "counter = {total}"
    );
    crate::task::reap_finished();
}

/// Do the things this repository *says about itself* still hold?
///
/// Every other suite here tests the kernel. This one tests the prose, and it
/// exists because the prose has been wrong three times.
///
/// The README's limitations list asserted for weeks that there were no signals,
/// no argv, one address space and no preemptible userspace, long after all four
/// had shipped. The bridge's system prompt — whose entire job is telling a model
/// what machine it is attached to — said the kernel had no allocator, no paging
/// code of its own and no filesystem. `ask memory` said "there is no allocator,
/// none" while `heap.rs` sat in the same directory.
///
/// None of those were caught by a test, because none of them were testable.
/// They were sentences, and sentences do not fail.
///
/// So each check below is a claim made somewhere in the documentation, restated
/// as something the machine can be asked. If a feature is ever removed the check
/// fails and the sentence gets revisited; if a sentence is ever written that
/// contradicts one of these, the contradiction is one boot away from being
/// obvious.
///
/// Note what this cannot do. It cannot catch a claim about something the kernel
/// does *not* have — "no network stack" is unfalsifiable from in here — and it
/// cannot catch prose that is merely misleading. It catches the specific class
/// that actually bit: a capability that arrived and a sentence that never
/// noticed.
fn claims(report: &mut Report) {
    // "The heap allocator: the line where Vec and String start existing."
    // Said not to exist by `ask memory` and by the bridge prompt.
    let mut grown: Vec<u64> = Vec::new();
    for value in 0..500u64 {
        grown.push(value * 3);
    }
    check!(
        report,
        "there is an allocator",
        grown.len() == 500 && grown[499] == 1497,
        "a Vec grew to {} and kept its values",
        grown.len()
    );
    drop(grown);

    // "Page tables: four table reads to resolve one address." Said by `ask
    // paging` to be the bootloader's work that we merely inherited.
    let (free_before, _) = memory::allocator().stats();
    const CLAIM_PAGE: u64 = 0x0000_3100_0000;
    let built = memory::allocator().allocate().is_some_and(|frame| {
        let mapped = unsafe { memory::map_page(CLAIM_PAGE, frame, memory::FLAG_WRITABLE) };
        let resolves = memory::translate(CLAIM_PAGE).map(|(physical, _)| physical) == Some(frame);
        unsafe { memory::unmap_page(CLAIM_PAGE) };
        unsafe { memory::allocator().deallocate(frame) };
        mapped.is_ok() && resolves
    });
    let (free_after, _) = memory::allocator().stats();
    check!(
        report,
        "the kernel maps its own pages",
        built && free_after == free_before,
        "mapped, walked and unmapped {CLAIM_PAGE:#x} with no frames lost"
    );

    // "A process is a task with its own level-4 table." The README claimed for
    // weeks that every task shared one CR3.
    let private = match memory::AddressSpace::new() {
        Some(space) => {
            let root = space.root();
            let distinct = root != memory::kernel_root() && root != 0;
            unsafe { space.destroy() };
            distinct
        }
        None => false,
    };
    check!(
        report,
        "a process gets its own space",
        private,
        "a fresh level-4 table is not the kernel's {:#x}",
        memory::kernel_root()
    );

    // "Preemptible ring 3" and "argv + init", both denied by the README long
    // after they worked. The frame a user task is resumed through carries the
    // proof of each: the interrupt flag, and the two argument registers.
    let mut scratch = vec![0u64; 48];
    let top = (scratch.as_mut_ptr() as u64 + 48 * 8) & !0xF;
    let frame = unsafe { crate::task::build_user_frame(top, 0x40_0000, 0x11_0000, 3, 0xBEEF) };
    let word = |index: usize| unsafe { *((frame as *const u64).add(index)) };
    check!(
        report,
        "ring 3 runs interruptible",
        word(17) & (1 << 9) != 0 && word(16) & 3 == 3,
        "RFLAGS {:#x} has IF set, CS {:#x} is ring 3",
        word(17),
        word(16)
    );
    check!(
        report,
        "argv reaches the program",
        word(8) == 3 && word(9) == 0xBEEF,
        "argc in rdi = {}, argv in rsi = {:#x}",
        word(8),
        word(9)
    );
    drop(scratch);

    // "Pipes + file descriptors: end-of-file is a reference count." The README
    // claimed there were no file descriptors at all.
    let descriptors = match crate::pipe::create() {
        Some(pipe) => {
            let read = crate::task::add_descriptor(0, crate::task::Descriptor::PipeRead(pipe));
            let write = crate::task::add_descriptor(0, crate::task::Descriptor::PipeWrite(pipe));
            let ok = match (read, write) {
                (Some(r), Some(w)) => {
                    r != w
                        && crate::task::descriptor(0, r) == crate::task::Descriptor::PipeRead(pipe)
                        && crate::task::descriptor(0, w)
                            == crate::task::Descriptor::PipeWrite(pipe)
                }
                _ => false,
            };
            if let Some(r) = read {
                crate::task::close_descriptor(0, r);
            }
            if let Some(w) = write {
                crate::task::close_descriptor(0, w);
            }
            ok
        }
        None => false,
    };
    check!(
        report,
        "descriptors are per-task integers",
        descriptors,
        "two small numbers named the two ends of one pipe"
    );

    // "Signals: the kernel calling a function the program never called," and
    // SIGKILL is the one that cannot be declined. The README said neither
    // existed.
    let caught = crate::task::set_signal_handler(0, crate::signal::SIGINT, 0x1000, 0x2000);
    let refused = !crate::task::set_signal_handler(0, crate::signal::SIGKILL, 0x1000, 0x2000);
    crate::task::clear_signal_handlers(0);
    check!(
        report,
        "signals exist, SIGKILL excepted",
        caught && refused,
        "a SIGINT handler was accepted and a SIGKILL handler was not"
    );

    // "FAT16: a file is a linked list living in a table." The bridge prompt
    // said there was no filesystem.
    let files = crate::fat::mount()
        .and_then(|volume| volume.find("hello.elf"))
        .is_ok();
    check!(
        report,
        "there is a filesystem",
        files,
        "hello.elf was found on a real disk"
    );

    // The newest claim, and the one the front page now leads with: two
    // subsystems are slots rather than decisions.
    check!(
        report,
        "the scheduler is a slot",
        crate::sched::COUNT > 1,
        "{} policies registered, `{}` installed",
        crate::sched::COUNT,
        crate::sched::active_name()
    );
    check!(
        report,
        "page replacement is a slot",
        crate::replace::COUNT > 1,
        "{} policies registered, `{}` installed",
        crate::replace::COUNT,
        crate::replace::active_name()
    );
}

/// Is every registered page replacement policy *correct*?
///
/// The same split as `sched_policy`, one slot over. Correctness here has one
/// right answer and safety rests on it: name a page that is shared with another
/// address space and the mechanism would write it to disk and hand its frame
/// away while a second process is still reading through it. Whether a policy is
/// any *good* is `bench paging`, and that has no right answer at all.
///
/// Interrogated against fabricated entries that belong to no page table, so the
/// awkward cases are cheap: a set where everything is already swapped, or where
/// exactly one page of six may be taken.
fn replacers(report: &mut Report) {
    for index in 0..crate::replace::COUNT {
        let name = crate::replace::name_at(index);
        crate::replace::test_reset(index);

        // 1. Nothing at all. A policy that indexes into an empty set is a
        //    panic in the page fault path, which is the worst place for one.
        let empty: [(u64, u64, *mut u64); 0] = [];
        let picked = crate::replace::test_choose(index, &memory::test_page_set(&empty));
        check!(
            report,
            "survives an empty set",
            picked.is_none(),
            "{name}: chose {picked:?} from nothing"
        );

        // 2. Candidates exist, but every one of them is already out on disk.
        //    There is no frame left to reclaim in any of them.
        let mut gone = [memory::test_entry(false, false, false, true); 4];
        let pages = fabricate(&mut gone);
        let picked = crate::replace::test_choose(index, &memory::test_page_set(&pages));
        check!(
            report,
            "refuses when none are eligible",
            picked.is_none(),
            "{name}: chose {picked:?} with every candidate swapped out"
        );

        // 3. One taker among five that cannot be taken. Every policy has an
        //    order it prefers, and all of them must still arrive at the only
        //    available answer.
        let mut one = [
            memory::test_entry(false, false, false, true),
            memory::test_entry(false, false, false, true),
            memory::test_entry(true, true, true, false),
            memory::test_entry(false, false, false, true),
            memory::test_entry(false, false, false, true),
        ];
        let pages = fabricate(&mut one);
        let picked = crate::replace::test_choose(index, &memory::test_page_set(&pages));
        check!(
            report,
            "finds the only candidate",
            picked == Some(2),
            "{name}: chose {picked:?}, and index 2 was the only one eligible"
        );

        // 4. A crowd, repeatedly. A policy carries state and can be right once
        //    and wrong afterwards, so ask many times.
        const ROUNDS: usize = 200;
        let mut crowd = [
            memory::test_entry(true, true, false, false),
            memory::test_entry(true, false, false, false),
            memory::test_entry(false, false, false, true),
            memory::test_entry(true, true, true, false),
            memory::test_entry(true, false, true, false),
        ];
        let pages = fabricate(&mut crowd);
        let set = memory::test_page_set(&pages);
        crate::replace::test_reset(index);

        let mut illegal = 0usize;
        let mut gave_up = 0usize;
        for _ in 0..ROUNDS {
            match crate::replace::test_choose(index, &set) {
                Some(i) if i < set.len() && set.eligible(i) => {}
                Some(_) => illegal += 1,
                None => gave_up += 1,
            }
        }
        check!(
            report,
            "only ever names a taker",
            illegal == 0,
            "{name}: {ROUNDS} choices, {illegal} of them ineligible"
        );
        check!(
            report,
            "always finds one",
            gave_up == 0,
            "{name}: {ROUNDS} choices, {gave_up} gave up with four eligible"
        );

        crate::replace::test_reset(index);
    }
    // Probing reset the installed policy along with the rest.
    crate::replace::install_by_name(crate::replace::active_name());
}

/// Turn fabricated entries into the (address, entry, pointer) triples a
/// `PageSet` is built over. The addresses name nothing; only `invlpg` ever
/// touches them, and flushing a translation that does not exist is a no-op.
fn fabricate(entries: &mut [u64]) -> Vec<(u64, u64, *mut u64)> {
    entries
        .iter_mut()
        .enumerate()
        .map(|(index, entry)| {
            let address = 0x5000_0000 + index as u64 * memory::PAGE_SIZE as u64;
            (address, *entry, entry as *mut u64)
        })
        .collect()
}

/// Is every registered scheduling policy *correct*?
///
/// Note what this suite does not ask. It says nothing about whether a policy is
/// fair, whether it starves anybody, or whether it is any good — `fifo` passes
/// every check here and will still hang your machine, and it is supposed to.
/// That is the split the whole slot rests on:
///
///   * **conformance** is safety. A policy that picks a task which is not
///     runnable will resume a stack that belongs to nobody, and no amount of
///     cleverness elsewhere survives that. There is exactly one right answer
///     and it is checked here.
///   * **quality** is `bench sched`. There is no right answer, only trade-offs
///     you can measure and then argue about.
///
/// Every registered brick is examined, including one you have just written, and
/// it is examined against a synthetic task table rather than the live machine —
/// so a broken policy is a failed line rather than a console that stops
/// answering.
fn sched_policy(report: &mut Report) {
    use crate::task::State;

    for index in 0..crate::sched::COUNT {
        let name = crate::sched::name_at(index);
        // Start from nothing known, so one policy's history cannot make
        // another's answer look right.
        crate::sched::test_reset(index);

        // 1. Nobody is runnable. There is no honest answer but `None`, and a
        //    policy that invents one hands the scheduler a task to resume that
        //    is blocked, finished, or has never existed.
        let empty = crate::task::test_table(&[
            (State::Blocked(crate::task::Reason::Channel(1)), 0),
            (State::Finished, 8),
        ]);
        let picked = crate::sched::test_pick(index, &empty, 0, 100);
        check!(
            report,
            "picks nobody when nobody can",
            picked.is_none(),
            "{name}: chose {picked:?} from a table with no runnable task"
        );

        // 2. Exactly one runnable task, and it is not the current one. Any
        //    policy that has a rotation, a queue or a priority order must still
        //    arrive at the only available answer.
        let one = crate::task::test_table(&[
            (State::Blocked(crate::task::Reason::Channel(1)), 0),
            (State::Free, 0),
            (State::Ready, 9),
        ]);
        crate::sched::test_ready(index, 2);
        let picked = crate::sched::test_pick(index, &one, 0, 200);
        check!(
            report,
            "finds the only candidate",
            picked == Some(2),
            "{name}: chose {picked:?}, and task 2 was the only Ready one"
        );

        // 3. A crowd, repeatedly. Every answer must be a runnable task, and
        //    there must always be one -- checked over many rounds because a
        //    policy carries state and can be right once and wrong afterwards.
        let crowd = crate::task::test_table(&[
            (State::Ready, 0),
            (State::Ready, 8),
            (State::Blocked(crate::task::Reason::Until(9999)), 8),
            (State::Ready, crate::task::MAX_PRIORITY),
            (State::Finished, 4),
        ]);
        crate::sched::test_reset(index);
        for id in [0usize, 1, 3] {
            crate::sched::test_ready(index, id);
        }

        const ROUNDS: u64 = 200;
        let mut illegal = 0usize;
        let mut gave_up = 0usize;
        let mut current = 0usize;
        for round in 0..ROUNDS {
            match crate::sched::test_pick(index, &crowd, current, 300 + round) {
                Some(id) if id < crate::task::MAX_TASKS && crowd[id].state == State::Ready => {
                    current = id
                }
                Some(_) => illegal += 1,
                None => gave_up += 1,
            }
        }
        check!(
            report,
            "only ever picks runnable",
            illegal == 0,
            "{name}: {ROUNDS} picks, {illegal} of them not runnable"
        );
        check!(
            report,
            "always picks somebody",
            gave_up == 0,
            "{name}: {ROUNDS} picks, {gave_up} gave up with 3 tasks Ready"
        );

        // 4. A quantum of zero is a scheduler that only ever schedules. The
        //    mechanism clamps this, but a policy that returns it is telling us
        //    something is wrong with its arithmetic.
        let quantum = crate::sched::test_quantum(index, current, &crowd, current, 500);
        check!(
            report,
            "asks for at least one tick",
            quantum >= 1,
            "{name}: wanted a quantum of {quantum}"
        );

        crate::sched::test_reset(index);
    }

    // Probing reset the installed policy too. Hand it the real runnable set
    // back before the machine takes another timer interrupt.
    crate::sched::resync();
}

/// Can a task that never yields starve one below it?
///
/// The hog runs at the top priority and never blocks, sleeps or yields. The
/// spinner runs at the bottom and only counts. Under a plain priority
/// scheduler the spinner never runs at all and this returns zero; under aging
/// it gets the CPU every MAX_PRIORITY+1 rounds or better.
///
/// Bounded on purpose -- the hog exits on a deadline whatever happens, so a
/// broken scheduler fails this check rather than hanging the machine.
fn priority(report: &mut Report) {
    use core::sync::atomic::Ordering;

    crate::task::SPINS.store(0, Ordering::Release);
    crate::task::STOP_SPINNING.store(false, Ordering::Release);

    let spinner = crate::task::spawn_with_priority(
        "spin",
        crate::task::Work::Spinner,
        crate::task::MAX_PRIORITY,
    );
    let hog = crate::task::spawn_with_priority("hog", crate::task::Work::Hog(20), 0);

    let (Ok(spinner), Ok(hog)) = (spinner, hog) else {
        check!(report, "spawn hog and spinner", false, "no free task slots");
        crate::task::STOP_SPINNING.store(true, Ordering::Release);
        return;
    };

    crate::task::wait_for(hog);
    let spins = crate::task::SPINS.load(Ordering::Acquire);
    crate::task::STOP_SPINNING.store(true, Ordering::Release);
    crate::task::wait_for(spinner);

    check!(
        report,
        "aging beats a spinning hog",
        spins > 0,
        "lowest-priority task ran {spins} times while priority 0 spun"
    );
    crate::task::reap_finished();
}

fn elf(report: &mut Report) {
    let program = crate::user::embedded_program();

    let (free_before, _) = memory::allocator().stats();
    match unsafe { crate::elf::load(program, false) } {
        Ok(loaded) => {
            check!(
                report,
                "loads the embedded program",
                loaded.entry != 0 && loaded.segments >= 1,
                "entry {:#x}, {} segments",
                loaded.entry,
                loaded.segments
            );
            let freed = unsafe { memory::release(&loaded.pages) };
            let (free_after, _) = memory::allocator().stats();
            check!(
                report,
                "release returns every page",
                free_after == free_before,
                "{freed} freed, {free_before} -> {free_after}"
            );
        }
        Err(error) => check!(report, "loads the embedded program", false, "{error}"),
    }

    // A loader that guesses is a loader that runs somebody else's bytes. These
    // must be refused, not tolerated.
    let mut broken = Vec::from(program);
    broken[0] = 0;
    check!(
        report,
        "rejects a bad magic",
        unsafe { crate::elf::load(&broken, false) }.is_err(),
        "refused"
    );

    check!(
        report,
        "rejects a truncated file",
        unsafe { crate::elf::load(&program[..32], false) }.is_err(),
        "refused 32 bytes"
    );
}

fn fat(report: &mut Report) {
    let volume = match crate::fat::mount() {
        Ok(volume) => volume,
        Err(error) => {
            println!("    skip  no disk attached ({error})");
            return;
        }
    };

    match volume.find("hello.elf") {
        Ok(entry) => {
            check!(
                report,
                "finds hello.elf",
                entry.size > 0,
                "{} bytes",
                entry.size
            );
            match volume.read_file(&entry) {
                Ok(data) => {
                    check!(
                        report,
                        "reads the whole file",
                        data.len() == entry.size as usize,
                        "{} of {} bytes",
                        data.len(),
                        entry.size
                    );
                    // Following the cluster chain correctly means the bytes are
                    // the ones the linker produced, not just the right count.
                    check!(
                        report,
                        "the bytes are an ELF",
                        data.len() > 4 && &data[0..4] == b"\x7fELF",
                        "starts {:02x?}",
                        &data[..4.min(data.len())]
                    );
                }
                Err(error) => check!(report, "reads the whole file", false, "{error}"),
            }
        }
        Err(error) => check!(report, "finds hello.elf", false, "{error}"),
    }

    check!(
        report,
        "a missing file is an error",
        volume.find("nope.txt").is_err(),
        "lookup of a nonexistent name refused"
    );

    // --- writing ---------------------------------------------------------
    //
    // Deliberately larger than one cluster, so this exercises a real chain
    // rather than a single-cluster special case that would pass with the
    // linking code missing entirely.
    let mut payload = Vec::new();
    for index in 0..5000u32 {
        payload.push((index % 251) as u8);
    }

    match volume.create("test.dat", &payload) {
        Ok(()) => {
            let read_back = volume
                .find("test.dat")
                .and_then(|entry| volume.read_file(&entry));
            match read_back {
                Ok(data) => {
                    check!(
                        report,
                        "written file reads back",
                        data == payload,
                        "{} bytes of {}",
                        data.len(),
                        payload.len()
                    );
                }
                Err(error) => check!(report, "written file reads back", false, "{error}"),
            }
        }
        Err(error) => check!(report, "create a file", false, "{error}"),
    }

    // Replacing must not leak the old chain, or a volume loses space every
    // time a file is edited.
    let free_before = free_clusters(&volume);
    let _ = volume.create("test.dat", &payload);
    let free_after = free_clusters(&volume);
    check!(
        report,
        "replacing reuses the space",
        free_before == free_after,
        "{free_before} free clusters before, {free_after} after"
    );

    match volume.remove("test.dat") {
        Ok(()) => {
            check!(
                report,
                "removed file is gone",
                volume.find("test.dat").is_err(),
                "no longer in the directory"
            );
            check!(
                report,
                "delete returns the clusters",
                free_clusters(&volume) > free_after,
                "{} free now",
                free_clusters(&volume)
            );
        }
        Err(error) => check!(report, "remove a file", false, "{error}"),
    }
}

/// How much room is left, counted the slow honest way.
fn free_clusters(volume: &crate::fat::Volume) -> u32 {
    volume.count_free().unwrap_or(0)
}
