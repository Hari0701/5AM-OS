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
    if all || which == "sync" {
        println!("  sync");
        sync(&mut report);
    }
    if all || which == "sched" {
        println!("  sched");
        sched(&mut report);
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
    check!(
        report,
        "200 cycles leave one hole",
        after_cycles.1 == 1 && after_cycles.0 == before.0,
        "{} holes, {} bytes free",
        after_cycles.1,
        after_cycles.0
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
