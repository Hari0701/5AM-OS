//! Preemptive multitasking: making one CPU look like several.
//!
//! Until now this kernel did exactly one thing at a time. Running the
//! transformer froze the shell for fifteen seconds — you could press keys, the
//! interrupt still fired and the bytes piled up in a buffer, but nothing
//! happened, because the shell *was* the thing running the transformer.
//!
//! ## What a task actually is
//!
//! Nothing but **register values plus a stack**. That is the whole idea. To
//! switch from one job to another you save the registers onto the current
//! stack, remember where that stack pointer was, load a different stack
//! pointer, and pop registers off *that* stack. Then you return — into a
//! completely different execution stream.
//!
//! The strange part is that the return does not come back to you. Something
//! else continues, and later something returns to you as though no time had
//! passed.
//!
//! ## Why the timer entry is written in assembly
//!
//! Every other interrupt handler here uses Rust's `extern "x86-interrupt"`,
//! which generates its own prologue and epilogue. That is fine when a handler
//! returns to the code it interrupted — and useless here, because we need to
//! return somewhere *else*, which means we need to know the exact layout of
//! what was pushed. So this one handler pushes the registers itself, in a known
//! order, hands the stack pointer to Rust, and uses whatever pointer Rust hands
//! back.
//!
//! A new task's stack is built to *look* like a task that was interrupted: a
//! fake interrupt frame with the entry point as its return address, and zeros
//! where the saved registers go. The first switch to it pops those zeros and
//! `iretq`s into the entry point, and the task never knows it was never
//! actually running.

use crate::interrupts::without_interrupts;
use crate::{gdt, println};
use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::arch::naked_asm;

pub const MAX_TASKS: usize = 8;
/// Lowest priority number that means anything. Bounded on purpose -- the aging
/// counter is not, and that asymmetry is what forbids starvation.
pub const MAX_PRIORITY: u8 = 15;
pub const DEFAULT_PRIORITY: u8 = 8;
/// The shell outranks everything: it is the only task a person is waiting on.
pub const SHELL_PRIORITY: u8 = 0;
const STACK_SIZE: usize = 64 * 1024;

#[derive(Clone, Copy, PartialEq)]
pub enum State {
    Free,
    Ready,
    /// Waiting for something, and not to be scheduled until it happens.
    ///
    /// The state this kernel went longest without, and the one that changes
    /// what a scheduler *is*. Round robin over none-but-Ready tasks is a
    /// timeshare; a scheduler that can be told "not this one, not yet" is what
    /// lets a machine wait for a keystroke without burning a core on it.
    Blocked(Reason),
    Finished,
}

/// Why a task is not runnable, and therefore what will make it runnable again.
///
/// Keeping the reason in the state rather than in a side table means waking is
/// a search for a matching reason, and it is impossible to have a task blocked
/// on nothing -- the bug where a task sleeps forever because whoever was
/// supposed to record the reason did not.
#[derive(Clone, Copy, PartialEq)]
pub enum Reason {
    /// Until `TICKS` reaches this value.
    Until(u64),
    /// Until somebody calls `wake_all` with the same address, which is just a
    /// number both sides agree on -- usually the address of the thing being
    /// waited for.
    Channel(u64),
}

pub struct Task {
    pub state: State,
    pub name: [u8; 24],
    pub name_len: usize,
    /// Where this task's saved registers are. Meaningless while it is running.
    pub stack_pointer: u64,
    /// Kept alive because the task is standing on it.
    _stack: Option<Box<[u8]>>,
    pub switches: u64,
    /// Lower runs first. 0 is the shell; background work sits well below it.
    pub priority: u8,
    /// Scheduling rounds this task has been passed over.
    ///
    /// Subtracted from `priority`, and that subtraction is the entire
    /// anti-starvation argument. Priorities are bounded (0..=MAX_PRIORITY) and
    /// this is not, so a task skipped MAX_PRIORITY+1 times outranks anything
    /// on the machine. Starvation is therefore not merely unlikely, it is
    /// impossible, with a stated bound.
    pub waited: u16,
    /// What the task returned, once it has finished.
    pub exit_code: u64,
    /// The level-4 table this task runs on, for tasks that have one of their
    /// own. Kernel tasks leave it None and run on whatever is active -- the
    /// kernel is mapped in every address space, so it does not matter which.
    pub address_space: Option<u64>,
    /// Who is entitled to this task's exit code.
    pub parent: Option<usize>,
    /// Top of this task's kernel stack, for `TSS.RSP0`.
    pub kernel_stack_top: u64,
    /// Work for the task to do, read by `task_entry`.
    pub work: Option<Work>,
    /// What this task's small integers mean.
    ///
    /// A file descriptor is not a file. It is an index into a per-process table
    /// of things that can be read or written, which is why 1 means "my standard
    /// output" and can be made to mean a pipe instead without the program
    /// noticing. That indirection is the whole reason a shell can redirect
    /// anything into anything.
    pub files: [Descriptor; MAX_FILES],
}

pub const MAX_FILES: usize = 8;

#[derive(Clone, Copy, PartialEq)]
pub enum Descriptor {
    Free,
    /// The serial console, which is what fds 0, 1 and 2 start as.
    Console,
    PipeRead(usize),
    PipeWrite(usize),
}

/// What a spawned task is for.
///
/// A real kernel would not have this: a task would run a program, and the
/// program would come from a file. This kernel can do that in ring 3 -- see
/// `exec` -- but a *kernel* task still has to be one of the things the kernel
/// knows how to do, because there is no way to hand it arbitrary code.
pub enum Work {
    /// Run the transformer on a prompt.
    Generate(String),
    /// A worker for the `workers` demo: take the shared semaphore, count, and
    /// give it back, sleeping in between so the interleaving is visible.
    Worker(usize),
    /// Print the tick count every `gap` ticks, `times` times. Exists to make
    /// preemption observable: if these lines appear *during* a ring 3 program
    /// that never yields, userspace is genuinely preemptible.
    Ticker { times: u64, gap: u64 },
    /// Spin without ever blocking, until `ticks` have passed. Used to prove
    /// that a task which never yields cannot starve a lower-priority one.
    Hog(u64),
    /// Count as fast as it is given the CPU, until told to stop.
    Spinner,
}

impl Task {
    const fn empty() -> Self {
        Self {
            state: State::Free,
            name: [0; 24],
            name_len: 0,
            stack_pointer: 0,
            _stack: None,
            switches: 0,
            priority: DEFAULT_PRIORITY,
            waited: 0,
            exit_code: 0,
            address_space: None,
            parent: None,
            kernel_stack_top: 0,
            work: None,
            files: [Descriptor::Free; MAX_FILES],
        }
    }

    pub fn name(&self) -> &str {
        core::str::from_utf8(&self.name[..self.name_len]).unwrap_or("?")
    }
}

static mut TASKS: [Task; MAX_TASKS] = [const { Task::empty() }; MAX_TASKS];
static mut CURRENT: usize = 0;
static mut ENABLED: bool = false;

fn tasks() -> &'static mut [Task; MAX_TASKS] {
    unsafe { &mut *core::ptr::addr_of_mut!(TASKS) }
}

pub fn current_id() -> usize {
    unsafe { core::ptr::read_volatile(core::ptr::addr_of!(CURRENT)) }
}

/// Register task 0 as the code that is already running: the shell.
///
/// It needs no stack allocated — it is standing on one. Its saved stack pointer
/// gets filled in the first time it is switched away from.
pub fn init() {
    crate::memory::remember_kernel_root();
    let tasks = tasks();
    tasks[0].state = State::Ready;
    let name = b"shell";
    tasks[0].name[..name.len()].copy_from_slice(name);
    tasks[0].name_len = name.len();
    tasks[0].priority = SHELL_PRIORITY;
    unsafe {
        CURRENT = 0;
        ENABLED = true;
    }
}

/// Create a task that will run `prompt` through the transformer.
pub fn spawn(name: &str, work: Work) -> Result<usize, &'static str> {
    spawn_with_priority(name, work, DEFAULT_PRIORITY)
}

pub fn spawn_with_priority(
    name: &str,
    work: Work,
    priority: u8,
) -> Result<usize, &'static str> {
    without_interrupts(|| {
        let tasks = tasks();
        let id = (1..MAX_TASKS)
            .find(|&i| tasks[i].state == State::Free)
            .ok_or("no free task slots")?;

        // Every task needs its own stack, and this is the first thing in the
        // kernel that could not have existed before the allocator did.
        let stack: Box<[u8]> = vec![0u8; STACK_SIZE].into_boxed_slice();
        let top = stack.as_ptr() as u64 + STACK_SIZE as u64;
        let top = top & !0xF;

        // Build a stack that looks like a task which was interrupted.
        let stack_pointer = unsafe { build_frame(top, task_entry as u64) };

        let task = &mut tasks[id];
        task.state = State::Ready;
        task.stack_pointer = stack_pointer;
        task.kernel_stack_top = top;
        task.address_space = None;
        task.parent = None;
        task._stack = Some(stack);
        task.switches = 0;
        task.work = Some(work);
        task.priority = priority.min(MAX_PRIORITY);
        let bytes = name.as_bytes();
        let len = bytes.len().min(task.name.len());
        task.name[..len].copy_from_slice(&bytes[..len]);
        task.name_len = len;

        Ok(id)
    })
}

/// Lay out a fake interrupt frame plus zeroed registers.
///
/// The order here must be the exact reverse of what `timer_entry` pops.
///
/// ## The alignment trap
///
/// `top` is 16-byte aligned, and handing that straight to the task is wrong in
/// a way that takes a fault to discover. The ABI does not say "aligned at entry"
/// — it says the stack was 16-aligned *before the `call`*, and the call pushed
/// eight bytes of return address. So a function's first instruction sees
/// `rsp % 16 == 8`, and the compiler emits `movaps` against that assumption.
/// Start a task on a 16-aligned stack and every spill is off by eight, which the
/// CPU reports as a #GP with error code 0 — not the alignment complaint you
/// would hope for.
///
/// # Safety
/// `top` must be the 16-aligned top of a stack big enough for the frame.
unsafe fn build_frame(top: u64, entry: u64) -> u64 {
    // Where the task's own RSP starts: one slot down, standing where a return
    // address would be if anyone had called it. Nothing reads that slot —
    // `task_entry` never returns — it exists purely to fix the alignment.
    let task_rsp = top - 8;
    let mut sp = top;
    let mut push = |value: u64| {
        sp -= 8;
        unsafe { *(sp as *mut u64) = value };
    };

    // What the CPU pushes on an interrupt, in the order it pushes it.
    push(0);                       // SS: 0 is legal for a ring-0 iretq
    push(task_rsp);                // RSP the task resumes with
    push(0x202);                   // RFLAGS: interrupts enabled, reserved bit
    push(gdt::KERNEL_CODE as u64); // CS
    push(entry);                   // RIP — where the task begins

    // The fifteen registers timer_entry pushes, all zero to start with.
    for _ in 0..15 {
        push(0);
    }
    sp
}

/// Where every spawned task begins.
extern "C" fn task_entry() -> ! {
    let id = current_id();
    let work = without_interrupts(|| tasks()[id].work.take());

    match work {
        Some(Work::Generate(prompt)) => crate::llm::generate(&prompt, 96),
        Some(Work::Worker(index)) => worker(index),
        Some(Work::Ticker { times, gap }) => {
            for round in 0..times {
                sleep(gap);
                println!(
                    "  [ticker] still running at tick {} ({}/{})",
                    crate::interrupts::ticks(),
                    round + 1,
                    times
                );
            }
        }
        Some(Work::Hog(ticks)) => {
            // Deliberately never blocks and never sleeps. A plain priority
            // scheduler hands this task the CPU forever.
            let deadline = crate::interrupts::ticks() + ticks;
            while crate::interrupts::ticks() < deadline {
                core::hint::spin_loop();
            }
        }
        Some(Work::Spinner) => {
            while !STOP_SPINNING.load(core::sync::atomic::Ordering::Acquire) {
                SPINS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            }
        }
        None => {}
    }

    without_interrupts(|| {
        tasks()[id].exit_code = 0;
        tasks()[id].state = State::Finished;
    });
    // Anyone blocked in wait_for() is waiting on this task's slot address.
    wake_all(id as u64);
    println!();
    println!("[task] {id} finished. Press enter for a prompt.");

    // The scheduler will not pick a Finished task again, so this only spins
    // until the next tick.
    loop {
        unsafe { core::arch::asm!("hlt", options(nomem, nostack)) };
    }
}

/// Pick the next runnable task and hand back its stack pointer.
///
/// Called from `timer_entry` with the interrupted task's stack pointer. Whatever
/// this returns becomes the stack the CPU resumes on — which is the entire
/// mechanism of multitasking, in one return value.
#[unsafe(no_mangle)]
extern "C" fn schedule(stack_pointer: u64) -> u64 {
    let enabled = unsafe { core::ptr::read_volatile(core::ptr::addr_of!(ENABLED)) };
    if !enabled {
        return stack_pointer;
    }

    let tasks = tasks();
    let current = current_id();
    tasks[current].stack_pointer = stack_pointer;

    // Anything sleeping until a deadline that has now passed becomes runnable
    // again. Doing this here, in the timer, is the entire implementation of
    // "sleep": there is no separate timer subsystem, only a scheduler that
    // checks the clock before it chooses.
    let now = crate::interrupts::ticks();
    for task in tasks.iter_mut() {
        if let State::Blocked(Reason::Until(deadline)) = task.state {
            if now >= deadline {
                task.state = State::Ready;
            }
        }
    }

    // Choose by effective priority, which is the stated priority minus how
    // long the task has been passed over.
    //
    // A plain priority scheduler starves its lowest priority task forever, and
    // the version of this that was here before did exactly that. What was
    // actually wrong then was `wait_for` polling -- the waiter came back Ready
    // on every tick, outranked everything, and prevented the very task it was
    // waiting for. Priorities took the blame. Now that waiting genuinely
    // blocks, this can be reinstated with a bound rather than a hope.
    let mut next = current;
    let mut best = i32::MAX;
    for offset in 1..=MAX_TASKS {
        let candidate = (current + offset) % MAX_TASKS;
        if tasks[candidate].state != State::Ready {
            continue;
        }
        let effective = tasks[candidate].priority as i32 - tasks[candidate].waited as i32;
        // Strictly less, and the scan starts just after the current task, so
        // equal-priority tasks rotate rather than the lowest id always winning.
        if effective < best {
            best = effective;
            next = candidate;
        }
    }

    // Age everyone who was runnable and did not get it. Unbounded growth here
    // is deliberate: see `waited`.
    for (id, task) in tasks.iter_mut().enumerate() {
        if task.state == State::Ready && id != next {
            task.waited = task.waited.saturating_add(1);
        }
    }
    tasks[next].waited = 0;

    if next != current {
        unsafe { CURRENT = next };
        tasks[next].switches += 1;

        // Two things have to follow the task, and forgetting either is a bug
        // that shows up somewhere else entirely.
        //
        // The address space, so a user task resumes seeing its own memory. A
        // kernel task has none and keeps whatever is active, which is safe
        // precisely because the kernel half is identical in all of them.
        // A task with no address space of its own runs in the kernel's, not in
        // whatever the last user task left behind. Inheriting works -- the
        // kernel is mapped in every space -- right up until the shell tries to
        // destroy the space it is currently standing in.
        let want = tasks[next]
            .address_space
            .unwrap_or_else(crate::memory::kernel_root);
        if want != 0 && want != crate::memory::active_root() {
            unsafe { crate::memory::activate_root(want) };
        }

        // And the kernel stack the CPU will use the next time this task enters
        // ring 0. Share one between two ring 3 tasks and the second to be
        // interrupted overwrites the first one's trap frame -- no fault, just
        // the wrong program resuming.
        if tasks[next].kernel_stack_top != 0 {
            crate::gdt::set_kernel_stack(tasks[next].kernel_stack_top);
        }
    }
    tasks[next].stack_pointer
}

/// The timer interrupt, with the register handling written out by hand.
///
/// # Safety
/// Installed directly in the IDT as vector 32. Never called from Rust.
#[unsafe(naked)]
pub unsafe extern "C" fn timer_entry() {
    naked_asm!(
        // Save every general-purpose register. The order matters only in that
        // build_frame must mirror it exactly.
        "push rax", "push rcx", "push rdx", "push rbx",
        "push rbp", "push rsi", "push rdi",
        "push r8",  "push r9",  "push r10", "push r11",
        "push r12", "push r13", "push r14", "push r15",

        // Tell the PIC we are done before switching away. Do it after and the
        // controller waits forever for an EOI from a task that is no longer
        // running, and no further timer interrupts ever arrive.
        "mov al, 0x20",
        "out 0x20, al",

        // Count the tick.
        "inc qword ptr [rip + {ticks}]",

        // schedule(current_rsp) -> rsp to resume on.
        "mov rdi, rsp",
        "call {schedule}",
        "mov rsp, rax",

        // Restore, in exact reverse.
        "pop r15", "pop r14", "pop r13", "pop r12",
        "pop r11", "pop r10", "pop r9",  "pop r8",
        "pop rdi", "pop rsi", "pop rbp",
        "pop rbx", "pop rdx", "pop rcx", "pop rax",
        "iretq",
        ticks = sym crate::interrupts::TICKS,
        schedule = sym schedule,
    )
}

/// Report every task, for the shell.
pub fn report() {
    let tasks = tasks();
    let current = current_id();
    if crate::llm::busy() {
        println!("  the model is claimed by one task (only one may run it)");
    }
    println!("  id  name      state     prio switches");
    for (id, task) in tasks.iter().enumerate() {
        let state = match task.state {
            State::Free => continue,
            State::Ready if id == current => "running",
            State::Ready => "ready",
            State::Blocked(Reason::Until(_)) => "sleeping",
            State::Blocked(Reason::Channel(_)) => "waiting",
            State::Finished => "finished",
        };
        println!(
            "  {id:<3} {:<9} {state:<9} {:<4} {}",
            task.name(),
            task.priority,
            task.switches
        );
    }
}

/// Give up the rest of this time slice.
///
/// Not required for preemption — the timer takes the CPU away regardless — but
/// it makes waiting politely cheap instead of burning a slice spinning.
pub fn yield_now() {
    unsafe { core::arch::asm!("hlt", options(nomem, nostack)) };
}

/// So the shell can tell whether anything is still working.
pub fn busy() -> bool {
    tasks()
        .iter()
        .enumerate()
        .any(|(id, t)| id != 0 && !matches!(t.state, State::Free | State::Finished))
}

pub fn reap_finished() {
    without_interrupts(|| {
        for task in tasks().iter_mut().skip(1) {
            if task.state == State::Finished {
                *task = Task::empty();
            }
        }
    })
}

/// Keeps `Vec` in scope for the stack allocation above.
const _: fn() -> Vec<u8> = || vec![0u8; 0];

// --- blocking and waking -------------------------------------------------

/// Give up the CPU until `wake_all(channel)` is called.
///
/// A channel is just a number both sides agree on -- conventionally the address
/// of whatever is being waited for, which makes collisions impossible without
/// any registry.
///
/// ## The lost wakeup
///
/// The check and the block must not be separable. Test a condition, get
/// interrupted, have the waker run and signal *before* you actually block, and
/// you go to sleep waiting for an event that already happened. The machine
/// stops with no error, and the bug appears once a week under load.
///
/// So the caller tests the condition and calls this with interrupts already
/// disabled; marking blocked and yielding happens without a window in between.
/// Mark this task blocked. Does not yield, and does not touch interrupts.
///
/// # Safety-in-the-non-Rust-sense
/// The caller must already have interrupts disabled, and must have tested its
/// condition inside that same disabled region. Anything else reintroduces the
/// lost wakeup this exists to prevent.
pub fn park(channel: u64) {
    let id = current_id();
    tasks()[id].state = State::Blocked(Reason::Channel(channel));
}

/// Test a condition and block on `channel` if it is not yet true, atomically.
///
/// The two halves cannot be separated. Test the condition, get preempted, have
/// the waker run and signal *before* you actually mark yourself blocked, and
/// you sleep waiting for an event that already happened. Nothing reports an
/// error; the task simply never runs again.
///
/// I wrote that warning on this function's first version and then implemented
/// exactly the bug it describes -- the test was inside a critical section and
/// the block was outside it. Three worker tasks deadlocked on the first run.
pub fn block_until<R>(channel: u64, mut ready: impl FnMut() -> Option<R>) -> R {
    loop {
        let were_enabled = crate::interrupts::are_enabled();
        crate::interrupts::disable();

        // Stop the compiler hoisting the condition's loads out of this loop.
        //
        // Whatever `ready` inspects is changed by another task, and on one core
        // the compiler can see no writer at all -- so it is entitled to read
        // once and reuse the answer forever. That is not a theoretical hazard:
        // it deadlocked three tasks against a semaphore whose count was
        // visibly 1. Disabling interrupts excludes other *tasks*; it says
        // nothing to the *compiler*.
        core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::SeqCst);

        if let Some(value) = ready() {
            if were_enabled {
                crate::interrupts::enable();
            }
            return value;
        }

        // Still inside the disabled region: no waker can run between the test
        // above and this line.
        park(channel);

        if were_enabled {
            crate::interrupts::enable();
        }
        // Hand the CPU away immediately rather than waiting for the timer.
        yield_now();
    }
}

/// Make every task waiting on `channel` runnable again.
///
/// Wakes all rather than one. Waking a single waiter needs a policy for which,
/// and every policy is wrong for somebody; letting them all re-test their
/// condition is the honest version, and the cost only matters at a scale this
/// kernel does not reach.
pub fn wake_all(channel: u64) -> usize {
    let mut woken = 0;
    for task in tasks().iter_mut() {
        if task.state == State::Blocked(Reason::Channel(channel)) {
            task.state = State::Ready;
            woken += 1;
        }
    }
    woken
}

/// Sleep for a number of timer ticks.
///
/// The deadline is absolute, not a countdown: a countdown decremented per tick
/// drifts whenever a tick is missed, and ticks are missed whenever an interrupt
/// arrives while interrupts are disabled.
pub fn sleep(ticks: u64) {
    let deadline = crate::interrupts::ticks() + ticks;
    without_interrupts(|| {
        let id = current_id();
        tasks()[id].state = State::Blocked(Reason::Until(deadline));
    });
    while crate::interrupts::ticks() < deadline {
        yield_now();
    }
}

/// The exit code of a finished task, if it has one.
pub fn exit_code(id: usize) -> Option<u64> {
    without_interrupts(|| {
        let task = &tasks()[id];
        match task.state {
            State::Finished => Some(task.exit_code),
            _ => None,
        }
    })
}

/// Block until task `id` has finished, then take its exit code.
///
/// The `wait` half of a process model. What is missing to make it the real
/// thing is a parent-child relationship: any task may wait for any other, and
/// nothing reaps a task nobody waited for -- which is precisely what a zombie
/// is.
pub fn wait_for(id: usize) -> Option<u64> {
    if id == 0 || id >= MAX_TASKS {
        return None;
    }
    // Blocks on the task's completion rather than polling for it. The first
    // version slept a tick and re-checked, which looks equivalent and is not:
    // the waiter is Ready every time it wakes, and the shell outranks the
    // workers, so it won the CPU on every single tick and the tasks it was
    // waiting for never ran. A poll loop turns "wait for you" into "prevent
    // you", and the higher the waiter's priority the worse it gets.
    block_until(id as u64, || match tasks()[id].state {
        State::Finished => Some(Some(tasks()[id].exit_code)),
        State::Free => Some(None),
        _ => None,
    })
}

// --- the worker demo -----------------------------------------------------

/// One permit. With a count of one, a semaphore is a mutex that sleeps instead
/// of spinning -- which is what you want around anything slow, and what this
/// kernel could not express until tasks could block.
static WORK_LOCK: crate::sync::Semaphore = crate::sync::Semaphore::new(1);

/// Shared state the workers fight over, so that "serialised" is something you
/// can see rather than something the comment claims.
static mut COUNTER: u64 = 0;

fn worker(index: usize) {
    for round in 0..3 {
        // Sleeping *outside* the critical section on purpose. Holding a lock
        // while sleeping is the classic way to turn a fast system into a slow
        // one -- everybody else waits for a task that is doing nothing.
        sleep(3 + index as u64);

        WORK_LOCK.wait();
        // Only one task is ever inside here. Without the semaphore, the
        // read-modify-write below is three separate steps with a preemption
        // point between each, and the total comes out wrong in a way that
        // depends on timing.
        let value = unsafe { core::ptr::read_volatile(core::ptr::addr_of!(COUNTER)) };
        sleep(2); // hold it long enough that an unprotected version would lose
        unsafe { core::ptr::write_volatile(core::ptr::addr_of_mut!(COUNTER), value + 1) };
        println!("  worker {index} round {round}: counter is now {}", value + 1);
        WORK_LOCK.signal();
    }
}

// --- the starvation experiment -------------------------------------------

pub static SPINS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static STOP_SPINNING: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

pub fn counter_value() -> u64 {
    unsafe { core::ptr::read_volatile(core::ptr::addr_of!(COUNTER)) }
}

pub fn reset_counter() {
    unsafe { core::ptr::write_volatile(core::ptr::addr_of_mut!(COUNTER), 0) };
}

// --- user tasks ----------------------------------------------------------

/// Lay out a frame in the exact shape `timer_entry` pops, so the scheduler can
/// resume anything with one code path.
///
/// This is the change that unified the two schedulers. There used to be two
/// save formats -- the timer's fifteen registers plus a trap frame, and the
/// syscall path's smaller one -- which meant two ways to resume and two places
/// that had to agree. Now `syscall_entry` saves in the same shape, and a task
/// is a task whether it was interrupted or made a call.
///
/// # Safety
/// `top` must be the 16-aligned top of a stack with room for the frame.
pub unsafe fn build_user_frame(top: u64, entry: u64, user_stack: u64) -> u64 {
    let mut sp = top;
    let mut push = |value: u64| {
        sp -= 8;
        unsafe { *(sp as *mut u64) = value };
    };

    push(gdt::USER_DATA as u64); // SS
    push(user_stack);            // RSP -- the user's own stack
    push(0x202);                 // RFLAGS: interrupts ON, so ring 3 is preemptible
    push(gdt::USER_CODE as u64); // CS -- RPL 3 is what performs the transition
    push(entry);                 // RIP

    // Fifteen registers, all zero. A fresh program has no history.
    for _ in 0..15 {
        push(0);
    }
    sp
}

/// Create a task that runs a program in ring 3.
///
/// It differs from a kernel task in exactly two fields -- an address space and
/// a trap frame with ring 3 selectors -- and in nothing else. The scheduler
/// does not know or care which kind it is picking.
pub fn spawn_user(
    name: &str,
    entry: u64,
    user_stack: u64,
    root: u64,
    parent: Option<usize>,
) -> Result<usize, &'static str> {
    without_interrupts(|| {
        let tasks = tasks();
        let id = (1..MAX_TASKS)
            .find(|&i| tasks[i].state == State::Free)
            .ok_or("no free task slots")?;

        let stack: Box<[u8]> = vec![0u8; STACK_SIZE].into_boxed_slice();
        let top = (stack.as_ptr() as u64 + STACK_SIZE as u64) & !0xF;
        let stack_pointer = unsafe { build_user_frame(top, entry, user_stack) };

        let task = &mut tasks[id];
        task.state = State::Ready;
        task.stack_pointer = stack_pointer;
        task.kernel_stack_top = top;
        task.address_space = Some(root);
        task.parent = parent;
        task.switches = 0;
        task.priority = DEFAULT_PRIORITY;
        task.waited = 0;
        task.exit_code = 0;
        task.work = None;
        // Hold the allocation. Dropping it here frees the very stack the task
        // is about to run on -- which faults only once the heap hands those
        // bytes to somebody else, so the first symptom is a crash in whatever
        // allocated next.
        task._stack = Some(stack);
        task.files = [Descriptor::Free; MAX_FILES];
        // 0, 1 and 2 are the console, by convention older than this kernel by
        // several decades. Nothing enforces it -- they are just the first three
        // slots, and a program that trusts them is trusting whoever set them.
        task.files[0] = Descriptor::Console;
        task.files[1] = Descriptor::Console;
        task.files[2] = Descriptor::Console;
        let bytes = name.as_bytes();
        let len = bytes.len().min(task.name.len());
        task.name[..len].copy_from_slice(&bytes[..len]);
        task.name_len = len;

        Ok(id)
    })
}

/// Copy a running task's trap frame onto a new task's stack: `fork`.
///
/// The child is the parent, exactly, with a different address space and a zero
/// in RAX. Because both are now saved in the same format, that is a memcpy and
/// one store.
pub fn fork_from(parent: usize, root: u64, frame: *const u64) -> Result<usize, &'static str> {
    without_interrupts(|| {
        let tasks = tasks();
        let id = (1..MAX_TASKS)
            .find(|&i| tasks[i].state == State::Free)
            .ok_or("no free task slots")?;

        let stack: Box<[u8]> = vec![0u8; STACK_SIZE].into_boxed_slice();
        let top = (stack.as_ptr() as u64 + STACK_SIZE as u64) & !0xF;

        // Twenty qwords: fifteen registers, then RIP, CS, RFLAGS, RSP, SS.
        const WORDS: usize = 20;
        let sp = top - (WORDS * 8) as u64;
        for index in 0..WORDS {
            unsafe { *((sp as *mut u64).add(index)) = *frame.add(index) };
        }
        // RAX was pushed first, so it sits at the top of the register block.
        // Zero is what makes this the child.
        unsafe { *((sp as *mut u64).add(14)) = 0 };

        let task = &mut tasks[id];
        task.state = State::Ready;
        task.stack_pointer = sp;
        task.kernel_stack_top = top;
        task.address_space = Some(root);
        task.parent = Some(parent);
        task.switches = 0;
        task.priority = tasks_priority(parent);
        task.waited = 0;
        task.exit_code = 0;
        task.work = None;
        // Descriptors are inherited, and every pipe end gains an owner. That
        // count is what end-of-file is made of: a reader learns there will be
        // no more data only when the last writer is gone, so a child that
        // inherits a write end and never closes it hangs the reader forever.
        task.files = tasks_files(parent);
        for descriptor in task.files.iter() {
            match descriptor {
                Descriptor::PipeRead(id) => crate::pipe::add_reader(*id),
                Descriptor::PipeWrite(id) => crate::pipe::add_writer(*id),
                _ => {}
            }
        }
        task._stack = Some(stack);
        let name = b"child";
        task.name[..name.len()].copy_from_slice(name);
        task.name_len = name.len();

        Ok(id)
    })
}

fn tasks_priority(id: usize) -> u8 {
    tasks()[id].priority
}

fn tasks_files(id: usize) -> [Descriptor; MAX_FILES] {
    tasks()[id].files
}

/// What a descriptor currently refers to.
pub fn descriptor(id: usize, fd: usize) -> Descriptor {
    if fd >= MAX_FILES {
        return Descriptor::Free;
    }
    tasks()[id].files[fd]
}

/// Put something in the lowest free slot, which is what makes the classic
/// "close it, then open the replacement" redirection trick work.
pub fn add_descriptor(id: usize, descriptor: Descriptor) -> Option<usize> {
    without_interrupts(|| {
        let files = &mut tasks()[id].files;
        let slot = files.iter().position(|d| *d == Descriptor::Free)?;
        files[slot] = descriptor;
        Some(slot)
    })
}

/// Drop a descriptor, telling the pipe it has one fewer end.
pub fn close_descriptor(id: usize, fd: usize) -> bool {
    let taken = without_interrupts(|| {
        if fd >= MAX_FILES {
            return Descriptor::Free;
        }
        let files = &mut tasks()[id].files;
        core::mem::replace(&mut files[fd], Descriptor::Free)
    });
    match taken {
        Descriptor::PipeRead(pipe) => {
            crate::pipe::close_reader(pipe);
            true
        }
        Descriptor::PipeWrite(pipe) => {
            crate::pipe::close_writer(pipe);
            true
        }
        Descriptor::Console => true,
        Descriptor::Free => false,
    }
}

/// Point `new` at whatever `old` refers to, closing whatever `new` was.
///
/// The pipe gains an owner, because two descriptors now name the same end and
/// end-of-file is counted, not observed.
pub fn duplicate_descriptor(id: usize, old: usize, new: usize) -> bool {
    if old >= MAX_FILES || new >= MAX_FILES {
        return false;
    }
    let source = descriptor(id, old);
    if source == Descriptor::Free {
        return false;
    }
    if old == new {
        return true;
    }
    close_descriptor(id, new);
    match source {
        Descriptor::PipeRead(pipe) => crate::pipe::add_reader(pipe),
        Descriptor::PipeWrite(pipe) => crate::pipe::add_writer(pipe),
        _ => {}
    }
    without_interrupts(|| tasks()[id].files[new] = source);
    true
}

/// Close everything a finished task still held open.
///
/// Without this a program that exits without closing its pipe ends leaves them
/// counted forever, and whoever is reading waits for a writer that is not only
/// gone but was never going to write again.
pub fn close_all_descriptors(id: usize) {
    for fd in 0..MAX_FILES {
        close_descriptor(id, fd);
    }
}

/// Swap the address space of the running task, for `exec`.
pub fn set_address_space(id: usize, root: u64) {
    without_interrupts(|| tasks()[id].address_space = Some(root))
}

pub fn address_space_of(id: usize) -> Option<u64> {
    tasks()[id].address_space
}

/// Finish the current task with a code, and wake whoever is waiting.
pub fn finish(id: usize, code: u64) {
    close_all_descriptors(id);
    without_interrupts(|| {
        tasks()[id].exit_code = code;
        tasks()[id].state = State::Finished;
    });
    wake_all(id as u64);
}

/// Block until any child of `parent` finishes, and take its exit code.
///
/// This is `wait`, and it needed no new machinery at all: a child is a task, so
/// waiting for one is the blocked state and the wait channel that already
/// existed. Unifying the schedulers deleted the bespoke version.
pub fn wait_any_child(parent: usize) -> Option<u64> {
    // Anything to wait for?
    let any = tasks()
        .iter()
        .enumerate()
        .any(|(id, t)| t.parent == Some(parent) && t.state != State::Free && id != parent);
    if !any {
        return None;
    }

    loop {
        // Collect a finished child if there is one. Reaping here is what stops
        // it being a zombie -- a task that has stopped running and cannot be
        // forgotten because nobody has read its answer.
        let collected = without_interrupts(|| {
            let tasks = tasks();
            for id in 0..MAX_TASKS {
                if tasks[id].parent != Some(parent) || tasks[id].state != State::Finished {
                    continue;
                }
                let code = tasks[id].exit_code;
                // Take the address space *before* clearing the slot, or it is
                // simply lost: unreachable and still allocated, with nothing
                // left in the table pointing at it.
                if let Some(root) = tasks[id].address_space.take() {
                    orphans().push(root);
                }
                tasks[id] = Task::empty();
                return Some(code);
            }
            None
        });
        if let Some(code) = collected {
            return Some(code);
        }

        // Still running. Block on the child's own channel rather than polling:
        // a poll loop here would make the waiter runnable on every tick and
        // starve the very task it is waiting for.
        let child = tasks()
            .iter()
            .position(|t| t.parent == Some(parent) && t.state != State::Free)?;
        block_until(child as u64, || {
            let state = tasks()[child].state;
            if state == State::Finished || state == State::Free {
                Some(())
            } else {
                None
            }
        });
    }
}

/// Address spaces whose task is gone, waiting to be reclaimed.
static mut ORPHANS: Option<Vec<u64>> = None;

fn orphans() -> &'static mut Vec<u64> {
    unsafe {
        let slot = &mut *core::ptr::addr_of_mut!(ORPHANS);
        slot.get_or_insert_with(Vec::new)
    }
}

/// Every address space left over by a finished task. Taken out of the table as
/// they are collected, so nothing can be freed twice.
pub fn orphan_address_spaces() -> Vec<u64> {
    without_interrupts(|| {
        for task in tasks().iter_mut() {
            if matches!(task.state, State::Finished | State::Free) {
                if let Some(root) = task.address_space.take() {
                    orphans().push(root);
                }
            }
        }
        core::mem::take(orphans())
    })
}
