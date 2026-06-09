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

// --- round robin ---------------------------------------------------------

/// The simplest thing that is not obviously wrong: everybody in turn.
///
/// No state, no priorities, no bookkeeping — the scan starts just after the
/// current task and takes the first runnable one it finds, so the current task
/// is considered last and therefore goes to the back of the queue.
///
/// It is perfectly fair and that is precisely its problem. A task that blocks
/// for a keystroke after two microseconds is treated identically to one that
/// has been burning the CPU for a minute, so interactive work waits behind
/// batch work for no reason anybody chose. Every policy below is an argument
/// about how to tell those two apart.
pub struct Rr;

impl Rr {
    pub const fn new() -> Self {
        Self
    }
}

impl Default for Rr {
    fn default() -> Self {
        Self::new()
    }
}

impl Policy for Rr {
    fn name(&self) -> &'static str {
        "rr"
    }

    fn describe(&self) -> &'static str {
        "everybody in turn, one tick each. fair, and blind"
    }

    fn pick(&mut self, queue: &RunQueue) -> Option<usize> {
        let current = queue.current();
        for offset in 1..=MAX_TASKS {
            let candidate = (current + offset) % MAX_TASKS;
            if queue.is_ready(candidate) {
                return Some(candidate);
            }
        }
        None
    }
}

// --- first come, first served --------------------------------------------

/// Run whoever arrived first, until they stop wanting the CPU.
///
/// The oldest scheduling policy there is, and the one every batch system used
/// before interactive computing existed. It is **non-preemptive**: the running
/// task keeps the processor until it blocks or exits, and only then does the
/// longest-waiting task get a turn.
///
/// On this machine that is a live demonstration rather than a description. A
/// task that never blocks — `Work::Hog`, or a ring 3 program in a loop — holds
/// the CPU against a shell that is merely more important, and the machine stops
/// answering. That is not a bug in this brick; it is what the policy *is*, and
/// it is why the watchdog below exists.
pub struct Fifo {
    /// When each task arrived, as a monotonically increasing stamp. This is the
    /// state `on_ready` exists for: arrival order cannot be recovered from a
    /// snapshot of who is runnable, because the snapshot has no history in it.
    arrived: [u64; MAX_TASKS],
    stamp: u64,
}

impl Fifo {
    pub const fn new() -> Self {
        Self {
            arrived: [0; MAX_TASKS],
            stamp: 0,
        }
    }
}

impl Default for Fifo {
    fn default() -> Self {
        Self::new()
    }
}

impl Policy for Fifo {
    fn name(&self) -> &'static str {
        "fifo"
    }

    fn describe(&self) -> &'static str {
        "first come, first served, non-preemptive. WILL hang the shell"
    }

    fn pick(&mut self, queue: &RunQueue) -> Option<usize> {
        // Non-preemptive: if whoever is running still wants the CPU, they keep
        // it. This single line is the whole difference from round robin, and
        // the whole reason the machine can stop responding.
        let current = queue.current();
        if queue.is_ready(current) {
            return Some(current);
        }
        queue.ready().min_by_key(|&id| self.arrived[id])
    }

    fn on_ready(&mut self, id: usize) {
        if id < MAX_TASKS {
            self.stamp += 1;
            self.arrived[id] = self.stamp;
        }
    }

    fn on_exit(&mut self, id: usize) {
        if id < MAX_TASKS {
            self.arrived[id] = 0;
        }
    }

    fn reset(&mut self) {
        self.arrived = [0; MAX_TASKS];
        self.stamp = 0;
    }
}

// --- strict priority ------------------------------------------------------

/// Always the most important runnable task, and never anybody else.
///
/// This exists to be compared with [`Aging`], which is the same algorithm plus
/// one subtraction. Run `selftest priority` under each and the difference is
/// the entire argument for aging: here the lowest-priority task runs zero
/// times while a priority 0 task spins, because "most important" is evaluated
/// afresh every tick and the answer never changes.
///
/// Starvation is not a failure mode of this policy. It is the specification.
pub struct Prio;

impl Prio {
    pub const fn new() -> Self {
        Self
    }
}

impl Default for Prio {
    fn default() -> Self {
        Self::new()
    }
}

impl Policy for Prio {
    fn name(&self) -> &'static str {
        "prio"
    }

    fn describe(&self) -> &'static str {
        "strict priority, no aging. starves the bottom by design"
    }

    fn pick(&mut self, queue: &RunQueue) -> Option<usize> {
        let current = queue.current();
        let mut chosen = None;
        let mut best = u16::MAX;
        for offset in 1..=MAX_TASKS {
            let candidate = (current + offset) % MAX_TASKS;
            if !queue.is_ready(candidate) {
                continue;
            }
            let priority = queue.priority(candidate) as u16;
            if priority < best {
                best = priority;
                chosen = Some(candidate);
            }
        }
        chosen
    }
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

// --- multi-level feedback queue ------------------------------------------

/// Work out what a task *is* by watching what it does.
///
/// Every policy above is told how to rank tasks. `prio` and `aging` are told by
/// a priority number somebody assigned; `rr` and `fifo` are told nothing and
/// treat everything alike. This one is told nothing and **infers**, which is
/// the idea that made interactive computing work and is still what Linux,
/// Windows and macOS are all doing underneath.
///
/// The observation it rests on is small: a task that runs out its whole time
/// slice is telling you it is CPU-bound, and a task that blocks before the
/// slice ends is telling you it is waiting on something a person is probably
/// attached to. Neither one had to declare anything. The scheduler simply
/// watched.
///
/// ## The rules
///
/// 1. Higher queue wins. Round robin within a queue.
/// 2. A new task starts at the top -- optimism, and it is cheap to be wrong.
/// 3. Use your whole quantum and you drop one level.
/// 4. Block before it expires and you stay where you are.
/// 5. Every `BOOST` ticks, everybody goes back to the top.
///
/// ## Why rule 5 is not optional
///
/// Rules 1-4 alone starve long-running work exactly as `prio` does, and worse,
/// they do it to tasks that did nothing wrong -- a compile that has been
/// running for a minute is at the bottom forever. The periodic boost is the
/// floor, and it is the same argument aging makes, made in one move instead of
/// continuously. Take rule 5 out and `bench sched` will show you the hole.
///
/// ## Where the quanta come from
///
/// Doubling with depth: 2, 4, 8, 16 ticks. A task that has proved it is
/// CPU-bound is cheaper to run for longer -- fewer switches for the same work,
/// which is `fifo`'s one virtue, applied only where it does no harm.
///
/// The top quantum is 2 rather than 1 because the timer ticks at ~18.2 Hz and
/// this kernel cannot see anything finer. With a 1-tick quantum, "blocked
/// early" and "used it all" are the same measurement, and rule 3 would demote
/// the very tasks rule 4 exists to protect.
pub struct Mlfq {
    level: [u8; MAX_TASKS],
    /// When everybody goes back to the top.
    boost_at: u64,
}

/// How many queues. Four is the usual number and there is nothing magic in it.
const LEVELS: u8 = 4;
/// Ticks between boosts.
const BOOST: u64 = 25;

impl Mlfq {
    pub const fn new() -> Self {
        Self {
            level: [0; MAX_TASKS],
            boost_at: 0,
        }
    }
}

impl Default for Mlfq {
    fn default() -> Self {
        Self::new()
    }
}

impl Policy for Mlfq {
    fn name(&self) -> &'static str {
        "mlfq"
    }

    fn describe(&self) -> &'static str {
        "infers interactivity from behaviour. nobody declares anything"
    }

    fn pick(&mut self, queue: &RunQueue) -> Option<usize> {
        let now = queue.now();
        if now >= self.boost_at {
            // Rule 5. Everybody back to the top, including whoever has been at
            // the bottom long enough to have been forgotten.
            self.level = [0; MAX_TASKS];
            self.boost_at = now + BOOST;
        }

        // Rule 1: best level wins; rule 2: rotate within it, which is what
        // scanning from `current + 1` and taking strictly-better gives us.
        let current = queue.current();
        let mut chosen = None;
        let mut best = u8::MAX;
        for offset in 1..=MAX_TASKS {
            let candidate = (current + offset) % MAX_TASKS;
            if !queue.is_ready(candidate) {
                continue;
            }
            if self.level[candidate] < best {
                best = self.level[candidate];
                chosen = Some(candidate);
            }
        }
        chosen
    }

    fn quantum(&mut self, id: usize, _queue: &RunQueue) -> u32 {
        2u32 << self.level[id.min(MAX_TASKS - 1)]
    }

    fn on_yield(&mut self, id: usize, used_full_quantum: bool) {
        if id >= MAX_TASKS {
            return;
        }
        // Rule 3 and rule 4, and this single branch is the entire policy. Note
        // that nothing here asks what the task *is* -- only what it just did.
        if used_full_quantum && self.level[id] + 1 < LEVELS {
            self.level[id] += 1;
        }
    }

    fn on_ready(&mut self, id: usize) {
        // Rule 2. A task nobody has seen before is assumed to be interactive,
        // because being wrong costs one quantum and being right saves a
        // person's afternoon.
        if id < MAX_TASKS {
            self.level[id] = 0;
        }
    }

    fn on_exit(&mut self, id: usize) {
        if id < MAX_TASKS {
            self.level[id] = 0;
        }
    }

    fn reset(&mut self) {
        self.level = [0; MAX_TASKS];
        self.boost_at = 0;
    }
}

// --- the registry --------------------------------------------------------

static mut RR: Rr = Rr::new();
static mut FIFO: Fifo = Fifo::new();
static mut PRIO: Prio = Prio::new();
static mut AGING: Aging = Aging::new();
static mut MLFQ: Mlfq = Mlfq::new();

/// How many bricks are registered.
pub const COUNT: usize = 5;

/// The one policy that is guaranteed not to starve anybody, and therefore the
/// only safe thing for the watchdog to fall back to.
const SAFE: usize = 3;

/// Which policy is installed, as an index into [`policy_at`].
static mut ACTIVE: usize = SAFE;

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
            0 => &mut *core::ptr::addr_of_mut!(RR),
            1 => &mut *core::ptr::addr_of_mut!(FIFO),
            2 => &mut *core::ptr::addr_of_mut!(PRIO),
            4 => &mut *core::ptr::addr_of_mut!(MLFQ),
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

    let active = active_index();
    let policy = policy_at(active);
    policy.on_yield(current, remaining == 0);

    let next = match policy.pick(&queue) {
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
    };

    watchdog(&queue, next, active, tasks, current, now);
    next
}

// --- the starvation watchdog ---------------------------------------------
//
// A policy is allowed to be wrong. `fifo` will hold the CPU against a shell
// that is merely more important, and `prio` will never run the bottom task at
// all -- those are the lessons, not defects. But a machine you cannot type into
// is a machine you cannot learn anything else from, so the mechanism keeps its
// own count and takes the wheel back.
//
// It counts in the mechanism, not in the policy, for the same reason the
// benchmark counters will: a policy that graded itself could report whatever it
// liked, and the entire value of comparing bricks rests on the numbers being
// collected by something with no stake in the answer.

/// Ticks a Ready task may go unpicked before the watchdog intervenes. The timer
/// runs at the PIT's default ~18.2 Hz, so this is about five and a half
/// seconds -- long enough to be unmistakably a hang, short enough to wait out.
const STARVATION_TICKS: u32 = 100;

/// How long each Ready task has gone without being chosen.
static mut UNPICKED: [u32; MAX_TASKS] = [0; MAX_TASKS];

/// What the watchdog saw, kept until somebody in a safe context reads it.
#[derive(Clone, Copy)]
pub struct Starvation {
    pub task: usize,
    pub ticks: u32,
    /// The policy that was installed when it happened.
    pub policy: usize,
    /// Whether the watchdog put `aging` back.
    pub reverted: bool,
}

static mut REPORT: Option<Starvation> = None;

fn watchdog(
    queue: &RunQueue,
    next: usize,
    active: usize,
    tasks: &[Task; MAX_TASKS],
    current: usize,
    now: u64,
) {
    let unpicked = unsafe { &mut *core::ptr::addr_of_mut!(UNPICKED) };

    let mut worst = 0usize;
    let mut longest = 0u32;
    for id in 0..MAX_TASKS {
        // Not runnable, or it just ran: nothing owed to it.
        if id == next || !queue.is_ready(id) {
            unpicked[id] = 0;
            continue;
        }
        unpicked[id] = unpicked[id].saturating_add(1);
        if unpicked[id] > longest {
            longest = unpicked[id];
            worst = id;
        }
    }

    if longest < STARVATION_TICKS {
        return;
    }

    // Only the first one. A starving machine would otherwise overwrite the
    // report every tick with a slightly larger number and lose the policy that
    // was actually responsible.
    let already = unsafe { core::ptr::read_volatile(core::ptr::addr_of!(REPORT)) };
    if already.is_some() {
        return;
    }

    // `aging` starving somebody would be a genuine bug rather than a policy
    // choice, so it is recorded and not "fixed" by reinstalling itself.
    let reverted = active != SAFE;
    unsafe {
        core::ptr::write_volatile(
            core::ptr::addr_of_mut!(REPORT),
            Some(Starvation {
                task: worst,
                ticks: longest,
                policy: active,
                reverted,
            }),
        );
    }

    if reverted {
        // Silently. `println!` takes a spinlock, and this runs inside the timer
        // interrupt -- if the code we preempted was itself printing, waiting for
        // that lock is a deadlock with interrupts already off. So the watchdog
        // acts here and explains itself later, from the shell, which can only
        // run again *because* it acted. See `report_starvation`.
        install(SAFE, tasks, current, now);
    }
}

/// Print what the watchdog saw, if anything. Clears the report.
///
/// Called from the shell -- an ordinary task, where taking the console lock is
/// safe. The machine is only able to reach this line because the watchdog put a
/// policy back that lets the shell run, which is the demonstration.
pub fn report_starvation() {
    let report = crate::interrupts::without_interrupts(|| unsafe {
        let slot = &mut *core::ptr::addr_of_mut!(REPORT);
        slot.take()
    });
    let Some(report) = report else { return };

    crate::println!();
    crate::println!(
        "  [sched] task {} was runnable and unpicked for {} ticks under `{}`.",
        report.task,
        report.ticks,
        name_at(report.policy)
    );
    crate::println!("          that is starvation, and it is what this policy does.");
    if report.reverted {
        crate::println!(
            "          the watchdog put `{}` back so you could read this.",
            name_at(SAFE)
        );
    } else {
        crate::println!("          `aging` bounds the wait, so this one is a bug. Not a lesson.");
    }
    crate::println!();
}

/// The longest any runnable task has currently gone unpicked, and who.
pub fn worst_wait() -> (usize, u32) {
    let unpicked = unsafe { &*core::ptr::addr_of!(UNPICKED) };
    let mut worst = 0;
    let mut longest = 0;
    for (id, &waited) in unpicked.iter().enumerate() {
        if waited > longest {
            longest = waited;
            worst = id;
        }
    }
    (worst, longest)
}

/// Tell the installed policy that a task slot has been filled.
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
            // The debt was owed by the policy being replaced.
            core::ptr::write_volatile(core::ptr::addr_of_mut!(UNPICKED), [0; MAX_TASKS]);
        }
    });
    true
}

/// Install by name, for the shell.
pub fn install_by_name(
    name: &str,
    tasks: &[Task; MAX_TASKS],
    current: usize,
    now: u64,
) -> bool {
    for index in 0..COUNT {
        if name_at(index) == name {
            return install(index, tasks, current, now);
        }
    }
    false
}