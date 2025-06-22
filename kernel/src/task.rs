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
    /// What the task returned, once it has finished.
    pub exit_code: u64,
    /// Work for the task to do, read by `task_entry`.
    pub work: Option<Work>,
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
            exit_code: 0,
            work: None,
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
    let tasks = tasks();
    tasks[0].state = State::Ready;
    let name = b"shell";
    tasks[0].name[..name.len()].copy_from_slice(name);
    tasks[0].name_len = name.len();
    unsafe {
        CURRENT = 0;
        ENABLED = true;
    }
}

/// Create a task that will run `prompt` through the transformer.
pub fn spawn(name: &str, work: Work) -> Result<usize, &'static str> {
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
        task._stack = Some(stack);
        task.switches = 0;
        task.work = Some(work);
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

    // Round robin over the runnable tasks. Blocked ones are simply not
    // candidates, which is the whole difference a wait state makes: the CPU
    // stops being offered to code that has nothing to do with it.
    //
    // Deliberately plain. A priority scheme with aging was here briefly and is
    // gone: it starved two of three workers in the `workers` demo, and a
    // scheduler whose fairness I cannot demonstrate is worse than an obvious
    // one that I can. Priorities belong on top of a scheduler that is known
    // correct, not mixed into the change that introduces blocking.
    let mut next = current;
    for offset in 1..=MAX_TASKS {
        let candidate = (current + offset) % MAX_TASKS;
        if tasks[candidate].state == State::Ready {
            next = candidate;
            break;
        }
    }

    if next != current {
        unsafe { CURRENT = next };
        tasks[next].switches += 1;
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
    println!("  id  name      state     switches");
    for (id, task) in tasks.iter().enumerate() {
        let state = match task.state {
            State::Free => continue,
            State::Ready if id == current => "running",
            State::Ready => "ready",
            State::Blocked(Reason::Until(_)) => "sleeping",
            State::Blocked(Reason::Channel(_)) => "waiting",
            State::Finished => "finished",
        };
        println!("  {id:<3} {:<9} {state:<9} {}", task.name(), task.switches);
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

pub fn counter_value() -> u64 {
    unsafe { core::ptr::read_volatile(core::ptr::addr_of!(COUNTER)) }
}

pub fn reset_counter() {
    unsafe { core::ptr::write_volatile(core::ptr::addr_of_mut!(COUNTER), 0) };
}
