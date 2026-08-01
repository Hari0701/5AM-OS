//! Running the same workload under every scheduling policy, and printing what
//! happened.
//!
//! This is the reason the policies were made swappable. "Round robin is fair
//! but bad for interactive work" and "priority starves the bottom" are things
//! every operating systems course says and nobody remembers, because they
//! arrive as claims. Here they arrive as a table the machine you are sitting in
//! front of produced two seconds ago, and the difference between those two
//! kinds of knowledge is the whole point of this kernel.
//!
//! ## Why every number is a count
//!
//! Not one column is a time. This kernel is normally run under QEMU's TCG,
//! emulating x86 on a machine that is not x86, where wall-clock figures measure
//! the emulator far more than the operating system. A tick count means the same
//! thing there as it does on real hardware.
//!
//! It is also the more honest unit for what is being compared. A scheduler does
//! not make work faster; it decides who waits. Slices, waits and switches are
//! that decision, stated directly.
//!
//! ## Why the workload has four shapes
//!
//! This is the part benchmarks usually get wrong. Run four identical CPU-bound
//! tasks and every policy in this kernel produces almost the same table, and
//! you learn nothing. Policies differ only when the workload contains the thing
//! they disagree about:
//!
//!   * two **hogs** that never block, so there is real contention;
//!   * one **interactive** task that sleeps, wakes and wants the CPU briefly
//!     but soon -- this is what separates a good policy from a fair one;
//!   * one **background** task at the lowest priority, which is the one that
//!     gets starved and therefore the one that tells you whether a policy has a
//!     floor.
//!
//! Take away the interactive task and `rr` looks optimal. Take away the
//! background task and `prio` looks free.
//!
//! ## Why the tasks carry deadlines rather than durations
//!
//! Because a starved task must still end. `Work::Burn` stops at an absolute
//! tick fixed when it was spawned, so a policy that never runs it at all is
//! measured rather than hung -- and the watchdog is switched off for the
//! duration, because a run that got rescued halfway through is not a
//! measurement of anything.

use crate::memory;
use crate::sched;
use crate::shell::FloatText;
use crate::task::{self, Work};
use crate::{interrupts, println};

// --- page replacement ------------------------------------------------------

/// Where the benchmark's pages live. Any unused user address will do; this one
/// is far from the program at 0x400000 and the stack below 0x110000.
const PAGE_BASE: u64 = 0x2000_0000;

/// Bélády's string, from the 1969 paper. Twelve references over five pages, and
/// the smallest known demonstration that FIFO gets *worse* with more memory.
const BELADY: [u8; 12] = [1, 2, 3, 4, 1, 2, 5, 1, 2, 3, 4, 5];

/// Two hot pages and a parade of cold ones — the shape almost all real programs
/// have. This is where a policy that reads the accessed bit should beat one
/// that does not.
const LOCALITY: [u8; 18] = [1, 2, 1, 2, 3, 1, 2, 4, 1, 2, 5, 1, 2, 6, 1, 2, 7, 1];

/// Run one reference string under one policy with a fixed number of frames,
/// and count the faults.
///
/// This is the textbook simulation, except nothing about it is simulated. The
/// pages are real pages in a real address space, the accessed bits are set by
/// the CPU translating through them, the evictions really write 4 KiB to the
/// IDE disk, and the policy under test is the same code that runs when the
/// machine genuinely runs out of memory.
///
/// The frame limit is enforced by evicting on purpose rather than by exhausting
/// the allocator — there are 125,000 free frames and running the machine out of
/// them to test three-frame behaviour would be a long way round. That is also
/// what a real kernel does: this is *local* replacement against a fixed
/// per-process allocation, which is one of the two ways the choice is framed.
fn run_string(policy: usize, string: &[u8], frames: usize) -> Option<(u64, u64)> {
    crate::replace::install(policy);

    let space = memory::AddressSpace::new()?;
    let root = space.root();
    core::mem::forget(space);
    let kernel = memory::active_root();

    let mut faults = 0u64;
    let mut resident = 0usize;
    // Times the policy was asked for a victim and declined while at the limit.
    //
    // This has to be counted or the table lies. A policy that refuses to evict
    // leaves the resident set above the cap, so every later reference finds its
    // page still there and the fault count comes out *lower* -- a broken brick
    // posting the best score in the comparison. Found by deliberately breaking
    // the clock two ways and watching both come back with six faults where the
    // working one has nine.
    let mut refused = 0u64;

    interrupts::without_interrupts(|| {
        // The scheduler installs a task's address space on every switch, and
        // the shell has none -- so a timer tick in the middle of this would put
        // the kernel's root back and the next touch would fault on an address
        // that is only mapped over here.
        unsafe { memory::activate_root(root) };

        for &reference in string {
            let page = PAGE_BASE + reference as u64 * memory::PAGE_SIZE as u64;

            if memory::translate(page).is_some() {
                touch(page, reference);
                continue;
            }

            faults += 1;

            // At the limit: somebody has to go, and which one is the question
            // this whole module exists to ask.
            if resident >= frames {
                match unsafe { memory::evict_one(root) } {
                    Some(frame) => {
                        unsafe { memory::allocator().deallocate(frame) };
                        resident -= 1;
                    }
                    None => refused += 1,
                }
            }

            let out_on_disk = memory::leaf_entry(root, page).is_some_and(memory::is_swapped_entry);
            let arrived = if out_on_disk {
                unsafe { memory::swap_in(page) }
            } else {
                match memory::allocator().allocate() {
                    Some(frame) => unsafe {
                        memory::map_page(
                            page,
                            frame,
                            memory::FLAG_USER | memory::FLAG_WRITABLE,
                        )
                        .is_ok()
                    },
                    None => false,
                }
            };

            if arrived {
                resident += 1;
                touch(page, reference);
            }
        }

        // Bring everything home before tearing the space down. `destroy` walks
        // present entries; a page still out on disk is not one, so its frame is
        // already gone but its swap slot would be leaked and its entry left
        // pointing at a slot that gets reused.
        for &reference in string {
            let page = PAGE_BASE + reference as u64 * memory::PAGE_SIZE as u64;
            if memory::leaf_entry(root, page).is_some_and(memory::is_swapped_entry) {
                unsafe { memory::swap_in(page) };
            }
        }

        unsafe { memory::activate_root(kernel) };
    });

    unsafe { memory::AddressSpace::adopt(root).destroy() };
    Some((faults, refused))
}

/// Reference a page so the CPU records it.
///
/// Reads set the accessed bit; only writes set the dirty bit. Odd-numbered
/// pages are written so that `nru` has something to classify -- with every page
/// clean its four classes collapse to two, and the policy is not being shown
/// doing what it does.
fn touch(page: u64, reference: u8) {
    unsafe {
        let byte = core::ptr::read_volatile(page as *const u8);
        if reference % 2 == 1 {
            core::ptr::write_volatile(page as *mut u8, byte.wrapping_add(1));
        }
    }
}

/// Compare every page replacement policy on the same reference strings.
pub fn paging() {
    if task::busy() {
        println!("  something is still running. The measurement needs the");
        println!("  machine to itself.");
        return;
    }

    let restore = crate::replace::active_name();
    let (free_before, _) = memory::allocator().stats();

    println!();
    println!("  Real pages, real accessed bits, real 4 KiB writes to the disk.");
    println!("  The frame limit is enforced by evicting on purpose, which is");
    println!("  what a fixed per-process allocation means.");
    println!();
    println!("  Belady's string:  1 2 3 4 1 2 5 1 2 3 4 5");
    println!();
    println!("    policy    3 frames  4 frames");
    for index in 0..crate::replace::COUNT {
        let three = run_string(index, &BELADY, 3);
        let four = run_string(index, &BELADY, 4);
        let (Some((three, refused3)), Some((four, refused4))) = (three, four) else {
            println!("    {:<9} out of memory", crate::replace::name_at(index));
            continue;
        };
        let note = if refused3 + refused4 > 0 {
            "  <- REFUSED to evict; the cap was not held, ignore these"
        } else if four > three {
            "  <- more memory, MORE faults"
        } else {
            ""
        };
        println!(
            "    {:<9} {three:>8}  {four:>8}{note}",
            crate::replace::name_at(index)
        );
    }

    println!();
    println!("  Locality:  1 2 1 2 3 1 2 4 1 2 5 1 2 6 1 2 7 1   (1 and 2 are hot)");
    println!();
    println!("    policy    3 frames  4 frames");
    for index in 0..crate::replace::COUNT {
        let three = run_string(index, &LOCALITY, 3);
        let four = run_string(index, &LOCALITY, 4);
        if let (Some((three, refused3)), Some((four, refused4))) = (three, four) {
            let note = if refused3 + refused4 > 0 {
                "  <- REFUSED to evict; the cap was not held, ignore these"
            } else if four > three {
                "  <- more memory, MORE faults"
            } else {
                ""
            };
            println!(
                "    {:<9} {three:>8}  {four:>8}{note}",
                crate::replace::name_at(index)
            );
        }
    }

    crate::replace::install_by_name(restore);
    let (free_after, _) = memory::allocator().stats();

    println!();
    println!("  Every number is a page fault: how many of the references above");
    println!("  found their page gone. Lower is better, and the second column");
    println!("  is supposed to be lower than the first.");
    println!();
    if free_after == free_before {
        println!("  {free_before} frames before, {free_after} after. Nothing leaked.");
    } else {
        println!("  LEAK: {free_before} frames before, {free_after} after.");
    }
    println!("  `{restore}` is installed again.");
    println!();
}

/// How wide a timeline is drawn. Plus the label, this fits an 80-column
/// terminal, which is the console this kernel actually has.
const WIDTH: usize = 60;

/// Draw who ran, and who wanted to, over a window of ticks.
///
/// Three characters, and the middle one is the point:
///
/// ```text
///   #   ran on this tick
///   -   was runnable and was not chosen
///   .   was not runnable -- blocked, sleeping, or gone
/// ```
///
/// A table can tell you a task waited 65 ticks. A row of sixty-five dashes
/// tells you the same thing in a way you cannot read past.
pub fn timeline(from: u64, to: u64) {
    let (last, depth) = sched::trace_window();
    if depth == 0 {
        println!("  nothing recorded yet.");
        return;
    }

    // Clamp to what the ring buffer still holds.
    let oldest = last.saturating_sub(depth.saturating_sub(1));
    let from = from.max(oldest);
    let to = to.min(last);
    if to < from {
        println!("  that window has already scrolled out of the trace.");
        return;
    }

    let span = to - from + 1;
    // One column per tick if it fits; otherwise each column stands for several,
    // and a task counts as having run in that column if it ran in any of them.
    let per_column = span.div_ceil(WIDTH as u64).max(1);
    let columns = (span.div_ceil(per_column) as usize).min(WIDTH);

    // Which tasks appear at all in this window? Drawing empty rows for the five
    // free slots is noise.
    let mut seen = 0u8;
    for tick in from..=to {
        if let Some((ran, ready)) = sched::trace_at(tick) {
            seen |= ready;
            if ran < 8 {
                seen |= 1 << ran;
            }
        }
    }

    println!(
        "  ticks {from}..{to}, {} per column.  # ran   - wanted it   . blocked",
        per_column
    );

    for id in 0..task::MAX_TASKS {
        if seen & (1 << id) == 0 {
            continue;
        }
        let mut line = [b'.'; WIDTH];
        for (column, slot) in line.iter_mut().enumerate().take(columns) {
            let start = from + column as u64 * per_column;
            let end = (start + per_column).min(to + 1);
            let mut ran = false;
            let mut wanted = false;
            for tick in start..end {
                if let Some((who, ready)) = sched::trace_at(tick) {
                    if who == id {
                        ran = true;
                    }
                    if ready & (1 << id) != 0 {
                        wanted = true;
                    }
                }
            }
            *slot = if ran {
                b'#'
            } else if wanted {
                b'-'
            } else {
                b'.'
            };
        }
        let drawn = core::str::from_utf8(&line[..columns]).unwrap_or("");
        println!("  {id} {:<7}{drawn}", task::name_of(id));
    }
}

/// One row of the table.
struct Row {
    policy: &'static str,
    switches: u64,
    fairness: f32,
    worst: u32,
    interactive_wait: u32,
    interactive_first: Option<u64>,
    starved: usize,
    background_slices: u64,
}

/// How long each policy is measured for, in timer ticks.
///
/// The PIT runs at its default ~18.2 Hz here, so 90 ticks is about five
/// seconds per policy. Long enough for aging to complete several rounds and for
/// starvation to be unambiguous; short enough that the whole comparison fits in
/// under half a minute.
const DEFAULT_TICKS: u64 = 90;

/// A task counts as starved if it waited this long without being picked.
const STARVED_AT: u32 = 40;

/// Run the comparison.
pub fn sched(argument: &str) {
    let budget = match argument.trim() {
        "" => DEFAULT_TICKS,
        text => match text.parse::<u64>() {
            Ok(value) if (20..=400).contains(&value) => value,
            _ => {
                println!("  usage: bench sched [ticks]   (20..400, default {DEFAULT_TICKS})");
                return;
            }
        },
    };

    if task::busy() {
        println!("  something is still running. Let it finish first -- the");
        println!("  measurement needs the machine to itself.");
        return;
    }

    let restore = sched::active_name();

    println!();
    println!("  Four tasks, {budget} ticks, once per policy:");
    println!("    hog-a, hog-b   never block. contention.");
    println!("    inter          sleeps and wakes. wants the CPU briefly, and soon.");
    println!("    bg             lowest priority. the one that gets starved.");
    println!();
    println!("  Every column is a count, not a time -- see the module comment.");
    println!();

    // A rescued run is not a measurement. Every task below ends on its own
    // deadline regardless, so nothing here can hang the machine for longer than
    // the budget.
    sched::set_watchdog(false);

    let mut rows: [Option<Row>; sched::COUNT] = [const { None }; sched::COUNT];
    for index in 0..sched::COUNT {
        let name = sched::name_at(index);
        println!("    running {name} ...");
        rows[index] = run_one(index, name, budget);
    }

    sched::set_watchdog(true);

    // Put back whatever was installed before, so a benchmark does not silently
    // leave the machine on the last policy it tried.
    sched::install_by_name(
        restore,
        task::snapshot(),
        task::current_id(),
        interrupts::ticks(),
    );

    print_table(&rows, budget, restore);
}

/// Measure one policy.
fn run_one(index: usize, name: &'static str, budget: u64) -> Option<Row> {
    sched::install(
        index,
        task::snapshot(),
        task::current_id(),
        interrupts::ticks(),
    );

    // Every deadline is fixed here, before anything is spawned, so all four
    // tasks are measured over exactly the same window whatever the policy does
    // to them.
    let start = interrupts::ticks();
    let until = start + budget;
    sched::reset_stats(start);

    let hog_a = task::spawn_with_priority("hog-a", Work::Burn { until }, 8).ok()?;
    let hog_b = task::spawn_with_priority("hog-b", Work::Burn { until }, 8).ok()?;
    let inter =
        task::spawn_with_priority("inter", Work::Interactive { until, gap: 4 }, 8).ok()?;
    let background = task::spawn_with_priority(
        "bg",
        Work::Burn { until },
        task::MAX_PRIORITY,
    )
    .ok()?;

    let workload = [hog_a, hog_b, inter, background];

    // The shell blocks here, so it is not part of the measurement -- which is
    // correct: it is the thing asking the question, not part of the workload.
    for id in workload {
        task::wait_for(id);
    }

    let row = Row {
        policy: name,
        switches: sched::switches(),
        fairness: fairness(&workload),
        worst: workload
            .iter()
            .map(|&id| sched::worst_wait_of(id))
            .max()
            .unwrap_or(0),
        interactive_wait: sched::worst_wait_of(inter),
        interactive_first: sched::first_run(inter),
        starved: workload
            .iter()
            .filter(|&&id| sched::worst_wait_of(id) >= STARVED_AT)
            .count(),
        background_slices: sched::slices(background),
    };

    // Draw it before reaping: the slots still hold the names, and a row labelled
    // `-` teaches nobody anything.
    println!();
    timeline(start, interrupts::ticks());
    println!();

    task::reap_finished();
    Some(row)
}

/// Jain's fairness index over the slices each task received.
///
/// `(sum x)^2 / (n * sum x^2)`, which is 1.0 when everybody got exactly the
/// same and 1/n when one task got everything. It is the standard measure and it
/// has the property that matters here: it is a single number, so two policies
/// can be put beside each other without an argument about which task counts.
///
/// Note what it does *not* capture. A perfectly fair policy can still be a bad
/// one -- see the interactive column, which is where `rr` loses.
fn fairness(tasks: &[usize]) -> f32 {
    let mut sum = 0.0f32;
    let mut sum_squares = 0.0f32;
    for &id in tasks {
        let slices = sched::slices(id) as f32;
        sum += slices;
        sum_squares += slices * slices;
    }
    if sum_squares == 0.0 {
        return 0.0;
    }
    (sum * sum) / (tasks.len() as f32 * sum_squares)
}

fn print_table(rows: &[Option<Row>], budget: u64, restored: &'static str) {
    println!();
    println!("  policy  switches  fairness  worst wait  inter wait  first CPU  starved  bg slices");
    for row in rows.iter().flatten() {
        let first = match row.interactive_first {
            Some(ticks) => ticks as i64,
            None => -1,
        };
        println!(
            "  {:<7} {:>8}  {:>8}  {:>10}  {:>10}  {:>9}  {:>7}  {:>9}",
            row.policy,
            row.switches,
            FloatText(row.fairness),
            row.worst,
            row.interactive_wait,
            first,
            row.starved,
            row.background_slices,
        );
    }

    println!();
    println!("  fairness    Jain's index over slices received. 1.00 = everybody equal.");
    println!("  worst wait  longest any task was runnable and not picked, in ticks.");
    println!("  inter wait  the same, for the interactive task. this is latency.");
    println!("  first CPU   ticks before the interactive task ran at all. -1 = never.");
    println!("  starved     tasks that waited {STARVED_AT}+ ticks. the watchdog was off.");
    println!("  bg slices   ticks the lowest-priority task got out of {budget}.");
    println!();
    println!("  `{restored}` is installed again. `sched <name>` to keep one.");
    println!();
}
