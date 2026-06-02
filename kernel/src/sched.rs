//! The scheduling *policy*, separated from the scheduling *mechanism*.
//!
//! Everything about switching tasks lives in `task.rs`: saving a stack pointer,
//! waking sleepers, changing CR3, moving `TSS.RSP0`, delivering signals. None of
//! that depends on which task you pick. It is the same work whether the answer
//! comes from round robin, a priority queue or a lottery.
//!
//! The choosing is the part that is genuinely a design decision, and until now
//! it was eight lines in the middle of the timer interrupt. This module is those
//! eight lines given a name, a contract, and somewhere to keep their state — so
//! that a second answer can exist beside the first and be compared with it.
//!
//! ## The seam
//!
//! A policy answers one question: *given who is runnable, who runs next?* It is
//! deliberately not allowed to do anything else. [`RunQueue`] is read-only and
//! exposes six methods; a policy cannot mark a task Ready, cannot touch a stack
//! pointer, and cannot see a page table. If a brick could do those things the
//! contract would be as wide as the kernel, and swapping one for another would
//! stop being safe.
//!
//! ## Quanta
//!
//! The old code re-decided on every single tick, which is round robin's quantum
//! of one baked in as an assumption rather than chosen. A policy now says how
//! long its choice should stand ([`Policy::quantum`]) and the mechanism counts
//! the ticks down. `Aging` returns 1, so the machine behaves exactly as it did;
//! a multi-level queue is what makes the field earn its place.
//!
//! ## Where state lives
//!
//! With the policy. That is the whole point of the split, and it is why `waited`
//! is a field of [`Aging`] rather than of `Task` — no other policy has any use
//! for it, and a `Task` carrying every policy's bookkeeping would be a task
//! struct that has to change every time somebody writes a new brick.

use crate::task::{State, Task, MAX_TASKS};

/// What a policy is allowed to see: the runnable set, and nothing else.
///
/// Read-only on purpose. A policy that could mutate task state would be able to
/// break the machine in ways the conformance suite could not attribute to it,
/// and "you may look, and then answer" is a contract that fits on one screen.
pub struct RunQueue<'a> {
    tasks: &'a [Task; MAX_TASKS],
    current: usize,
    now: u64,
}

impl<'a> RunQueue<'a> {
    /// # Safety-in-the-non-Rust-sense
    /// Built by the mechanism, with interrupts already disabled. Nothing else
    /// should construct one.
    pub fn new(tasks: &'a [Task; MAX_TASKS], current: usize, now: u64) -> Self {
        Self {
            tasks,
            current,
            now,
        }
    }

    /// Who is running right now — the task about to be switched away from.
    pub fn current(&self) -> usize {
        self.current
    }

    /// Ticks since boot. A policy that wants to age, boost or decay needs a
    /// clock, and this is the only one it gets.
    pub fn now(&self) -> u64 {
        self.now
    }

    pub fn is_ready(&self, id: usize) -> bool {
        id < MAX_TASKS && self.tasks[id].state == State::Ready
    }

    /// The priority the task was *given*. What a policy makes of it is the
    /// policy's business: round robin ignores it entirely.
    pub fn priority(&self, id: usize) -> u8 {
        if id >= MAX_TASKS {
            return crate::task::MAX_PRIORITY;
        }
        self.tasks[id].priority
    }

    /// Does this task run in ring 3? A policy may reasonably treat a user
    /// process differently from a kernel worker.
    pub fn is_user(&self, id: usize) -> bool {
        id < MAX_TASKS && self.tasks[id].address_space.is_some()
    }

    /// Every runnable task, in id order.
    pub fn ready(&self) -> impl Iterator<Item = usize> + '_ {
        (0..MAX_TASKS).filter(|&id| self.is_ready(id))
    }
}

/// One way of deciding what runs next.
///
/// Four of the six methods have defaults, so the smallest useful brick is a
/// name and a `pick`. The notifications exist because some policies need to
/// know what *happened*, not just what is true now — a multi-level queue is
/// built entirely on the difference between a task that blocked early and one
/// that burned its whole slice, and that difference is invisible from a
/// snapshot of the runnable set.
pub trait Policy {
    /// How this policy is named by the shell.
    fn name(&self) -> &'static str;

    /// One line, shown by `sched`. A brick that cannot say what it does in a
    /// sentence is usually a brick that does two things.
    fn describe(&self) -> &'static str {
        ""
    }

    /// Choose. `None` means nothing is runnable, and the mechanism will keep
    /// the current task where it is.
    ///
    /// Returning a task that is not Ready is a bug, and the conformance suite
    /// exists to say so out loud rather than let it present as a hang.
    fn pick(&mut self, queue: &RunQueue) -> Option<usize>;

    /// How many ticks this choice should stand for. Clamped to at least 1 by
    /// the mechanism: a quantum of zero is a machine that only schedules.
    fn quantum(&mut self, _id: usize, _queue: &RunQueue) -> u32 {
        1
    }

    /// A task slot has just been filled. Any per-task bookkeeping this policy
    /// keeps is stale and must be cleared: slot 3 today is not slot 3 from ten
    /// seconds ago.
    fn on_ready(&mut self, _id: usize) {}

    /// The running task is being asked to give the CPU back.
    ///
    /// `used_full_quantum` is the load-bearing argument. A task that ran out
    /// its slice is CPU-bound; one that blocked before the slice ended is
    /// interactive. Inferring that rather than being told it is the entire
    /// mechanism behind a multi-level feedback queue.
    fn on_yield(&mut self, _id: usize, _used_full_quantum: bool) {}

    /// The slot is free again.
    ///
    /// Note what is *not* here yet: a notification when a blocked task becomes
    /// runnable again. Nothing needs it so far -- `on_ready` means "a slot was
    /// filled", and arrival order is set at creation. A multi-level queue will
    /// need it, because a task that wakes from a blocking read is exactly the
    /// task it wants to promote. It is a separate hook when it arrives, so that
    /// adding it cannot silently change what `Aging` does.
    fn on_exit(&mut self, _id: usize) {}

    /// Forget everything. Called when this policy is installed, before the
    /// mechanism replays the currently runnable set into it.
    fn reset(&mut self) {}
}

// --- priority with aging -------------------------------------------------

/// The policy this kernel has always had, now as a brick.
///
/// Lower priority number runs first, minus how many rounds the task has been
/// passed over. Priorities are bounded (`0..=MAX_PRIORITY`) and `waited` is not,
/// so a task skipped `MAX_PRIORITY + 1` times outranks anything on the machine.
/// Starvation is therefore not merely unlikely, it is impossible, with a stated
/// bound — which is a much better thing to be able to say than "it seems fine".
pub struct Aging {
    /// Scheduling rounds each task has been passed over. This used to be a
    /// field on `Task`, where it did not belong: it is not a property of a
    /// task, it is this policy's opinion about one.
    waited: [u16; MAX_TASKS],
}

impl Aging {
    pub const fn new() -> Self {
        Self {
            waited: [0; MAX_TASKS],
        }
    }
}

impl Default for Aging {
    fn default() -> Self {
        Self::new()
    }
}

impl Policy for Aging {
    fn name(&self) -> &'static str {
        "aging"
    }

    fn describe(&self) -> &'static str {
        "priority minus how long you have waited. starvation is impossible"
    }

    fn pick(&mut self, queue: &RunQueue) -> Option<usize> {
        let current = queue.current();

        // Scan starting just *after* the current task, and take a candidate
        // only on a strictly better score. Together those two details are what
        // make equal-priority tasks rotate instead of the lowest id winning
        // every time.
        let mut chosen = None;
        let mut best = i32::MAX;
        for offset in 1..=MAX_TASKS {
            let candidate = (current + offset) % MAX_TASKS;
            if !queue.is_ready(candidate) {
                continue;
            }
            let effective = queue.priority(candidate) as i32 - self.waited[candidate] as i32;
            if effective < best {
                best = effective;
                chosen = Some(candidate);
            }
        }

        // Age everyone who was runnable and did not get it. Unbounded growth
        // here is deliberate -- see `waited`.
        let winner = chosen.unwrap_or(current);
        for id in 0..MAX_TASKS {
            if queue.is_ready(id) && id != winner {
                self.waited[id] = self.waited[id].saturating_add(1);
            }
        }
        self.waited[winner] = 0;

        chosen
    }

    fn on_ready(&mut self, id: usize) {
        if id < MAX_TASKS {
            self.waited[id] = 0;
        }
    }

    fn on_exit(&mut self, id: usize) {
        if id < MAX_TASKS {
            self.waited[id] = 0;
        }
    }

    fn reset(&mut self) {
        self.waited = [0; MAX_TASKS];
    }
}

// --- the registry --------------------------------------------------------

static mut AGING: Aging = Aging::new();

/// How many bricks are registered.
pub const COUNT: usize = 1;

/// Which policy is installed, as an index into [`policy_at`].
static mut ACTIVE: usize = 0;

/// Ticks left in the current quantum. Zero forces a fresh decision.
static mut REMAINING: u32 = 0;

/// Reach one policy.
///
/// A `match` rather than an array of trait objects, so exactly one `&mut` to
/// any given policy exists at a time. Adding a brick is a `static mut`, an arm
/// here, and one number in `COUNT` -- which is as close to "drop in a file" as
/// a kernel with no allocator at boot can get.
fn policy_at(index: usize) -> &'static mut dyn Policy {
    unsafe {
        match index {
            _ => &mut *core::ptr::addr_of_mut!(AGING),
        }
    }
}

/// A one-line description of a registered policy.
pub fn describe_at(index: usize) -> &'static str {
    policy_at(index.min(COUNT - 1)).describe()
}

fn active_index() -> usize {
    unsafe { core::ptr::read_volatile(core::ptr::addr_of!(ACTIVE)) }
}

/// The name of the installed policy, for the shell.
pub fn active_name() -> &'static str {
    policy_at(active_index()).name()
}

/// What the installed policy says it does.
pub fn active_description() -> &'static str {
    policy_at(active_index()).describe()
}

/// The name of a registered policy by index.
pub fn name_at(index: usize) -> &'static str {
    policy_at(index.min(COUNT - 1)).name()
}

/// Choose the next task to run.
///
/// The mechanism's single call into policy. Everything around it -- saving the
/// stack pointer, waking sleepers, switching address spaces, delivering signals
/// -- is unchanged and unaware that policies exist.
///
/// # Safety-in-the-non-Rust-sense
/// Called from the timer interrupt with interrupts already disabled.
pub fn choose(tasks: &[Task; MAX_TASKS], current: usize, now: u64) -> usize {
    let queue = RunQueue::new(tasks, current, now);

    // Burn a tick of the current quantum first, so a policy that asked for
    // several ticks actually gets them.
    let remaining = unsafe { core::ptr::read_volatile(core::ptr::addr_of!(REMAINING)) };
    let remaining = remaining.saturating_sub(1);
    unsafe { core::ptr::write_volatile(core::ptr::addr_of_mut!(REMAINING), remaining) };

    // Time left, and the task can still use it. Nothing to decide.
    //
    // The runnability test is not optional: a task that blocked or exited
    // during its own slice must not keep the CPU just because its quantum has
    // not expired.
    if remaining > 0 && queue.is_ready(current) {
        return current;
    }

    let policy = policy_at(active_index());
    policy.on_yield(current, remaining == 0);

    match policy.pick(&queue) {
        Some(id) => {
            let quantum = policy.quantum(id, &queue).max(1);
            unsafe { core::ptr::write_volatile(core::ptr::addr_of_mut!(REMAINING), quantum) };
            id
        }
        // Nobody is runnable. Stay where we are and ask again next tick.
        None => {
            unsafe { core::ptr::write_volatile(core::ptr::addr_of_mut!(REMAINING), 0) };
            current
        }
    }
}

pub fn task_created(id: usize) {
    policy_at(active_index()).on_ready(id);
}

/// Tell the installed policy that a task slot has been freed.
pub fn task_exited(id: usize) {
    policy_at(active_index()).on_exit(id);
}

/// Install a different policy, migrating the runnable set into it.
///
/// A fresh policy knows nothing, so this is a state handover rather than an
/// assignment: reset it, replay every currently runnable task, and clear the
/// quantum so the next tick asks the new brick rather than honouring the old
/// one's answer.
///
/// # Safety-in-the-non-Rust-sense
/// Must not be called from the timer interrupt -- the policy it is replacing is
/// the one on the stack. Call it from an ordinary task.
pub fn install(index: usize, tasks: &[Task; MAX_TASKS], current: usize, now: u64) -> bool {
    if index >= COUNT {
        return false;
    }
    crate::interrupts::without_interrupts(|| {
        let queue = RunQueue::new(tasks, current, now);
        let policy = policy_at(index);
        policy.reset();
        for id in queue.ready() {
            policy.on_ready(id);
        }
        unsafe {
            core::ptr::write_volatile(core::ptr::addr_of_mut!(ACTIVE), index);
            core::ptr::write_volatile(core::ptr::addr_of_mut!(REMAINING), 0);
        }
    });
    true
}
