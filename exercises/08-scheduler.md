# Lab 8 — Deciding what runs next

Every lab before this one has a right answer. A page table entry either names
the correct frame or it does not; an ELF loader either honours the program
headers or it refuses. You could get them wrong, and the machine would tell you.

This one has no right answer, and that is the whole reason it is here.

## The idea

A scheduler is asked one question, over and over, about eighteen times a second:
*given who is runnable, who runs next?* Every operating system ever written has
answered it differently, and none of them is wrong. They disagree because the
question hides a second one nobody can answer for you — **what is this machine
for?**

Answer "throughput" and you get long time slices and few switches. Answer "the
person waiting at the keyboard" and you get short ones. Answer "the deadline on
this control loop" and you get something else again. The scheduler is where that
choice lives, and it is one function.

In this kernel that function is a **slot**. There are five bricks in it already:

```
5am> sched
  scheduling policies -- `*` is installed:

     rr      everybody in turn, one tick each. fair, and blind
     fifo    first come, first served, non-preemptive. WILL hang the shell
     prio    strict priority, no aging. starves the bottom by design
   * aging   priority minus how long you have waited. starvation is impossible
     mlfq    infers interactivity from behaviour. nobody declares anything
```

You are going to delete the last one and write it again.

## Before you start: look at what you are aiming for

```bash
./run.sh
5am> bench sched
```

That runs one workload — two tasks that never block, one that sleeps and wakes,
one at the lowest priority — under every policy, and prints what happened:

```
  policy  switches  fairness  worst wait  inter wait  first CPU  starved  bg slices
  rr            94  0.9997           3           3          3        0         23
  fifo           8  0.2668          95          93         94        3          1
  prio          95  0.7428          93           2          3        1          1
  aging         97  0.8779          10           4          3        0         10
  mlfq          34  0.9758           5           2          5        0          9
```

Read the rows before you read any further. Four claims that every operating
systems course asserts are sitting in that table as measurements:

- **`fifo` is the cheapest and the worst.** Eight switches against everyone
  else's sixty-six — that is the throughput argument for batch scheduling, and
  it is real. Now look at fairness: 0.2754, which is almost exactly ¼, meaning
  one of the four tasks got essentially everything.
- **`rr` is nearly perfectly fair, and fair is not the same as good.** It has no
  way to prefer the task that wants the CPU briefly and soon. It just happens to
  do well here because a one-tick quantum is short.
- **`prio` has the *best* interactive latency in the table and starves a task
  anyway.** This is the row worth sitting with. Starvation is not a policy being
  bad at scheduling. Strict priority is *excellent* at serving what you declared
  important; it simply has no floor.
- **`aging` is what a floor costs.** Worst wait falls from 66 to 10 — that is
  the bound, visible — and it gives back a little latency and a little fairness
  to buy it.

Then draw the pictures:

```
5am> sched fifo
5am> timeline 80
```

`#` ran, `-` was runnable and did not get picked, `.` was blocked. A table can
tell you a task waited sixty-five ticks. Sixty-five dashes tell you the same
thing in a way you cannot read past.

## Do this

Open `kernel/src/sched.rs`, find `impl Policy for Mlfq`, and delete the bodies
of `pick`, `quantum`, `on_yield` and `on_ready`. Leave `name` and `describe`.

Write them again.

## The contract

You are implementing this trait. Four of its methods have defaults, so the
smallest working brick is `name` and `pick`.

```rust
fn pick(&mut self, queue: &RunQueue) -> Option<usize>;
fn quantum(&mut self, id: usize, queue: &RunQueue) -> u32;
fn on_ready(&mut self, id: usize);
fn on_yield(&mut self, id: usize, used_full_quantum: bool);
fn on_exit(&mut self, id: usize);
fn reset(&mut self);
```

`RunQueue` is everything you are allowed to see, and it is read-only:
`current()`, `now()`, `is_ready(id)`, `priority(id)`, `is_user(id)`, `ready()`.

Three things about the contract are worth more than the algorithm.

**Your state lives in your struct, not in `Task`.** `Aging` keeps a `waited`
array; `Fifo` keeps arrival stamps; you keep queue levels. A task struct
carrying every policy's bookkeeping would have to change every time somebody
wrote a new brick, which is the opposite of a slot.

**You cannot see the whole task.** No stack pointers, no address spaces, no way
to mark anything Ready. If a policy could do those things, a brick could break
the machine in ways nothing could attribute to it, and swapping one for another
would stop being safe.

**The notifications are not decoration.** `pick` receives a snapshot, and a
snapshot has no history in it. Arrival order, whether a task blocked early, how
long something has been demoted — none of that is recoverable from "who is
runnable right now". That is what `on_ready` and `on_yield` are for.

## The algorithm

Multi-level feedback queue. Five rules:

1. Higher queue wins. Round robin within a queue.
2. A new task starts at the top.
3. Use your whole quantum and you drop one level.
4. Block before it expires and you stay where you are.
5. Every `BOOST` ticks, everybody goes back to the top.

Rules 3 and 4 are the interesting pair, and they are one branch in `on_yield`.
Notice what they do *not* do: they never ask what a task is. A task that runs
out its slice is telling you it is CPU-bound. A task that blocks early is
telling you something is waiting on it — a disk, a pipe, a person. Neither one
declared anything. The scheduler watched.

That is the idea that made interactive computing work on machines shared by
dozens of people, and it is still roughly what Linux, Windows and macOS are
doing underneath.

## Where people go wrong

**Leaving out rule 5.** Rules 1–4 alone let long-running work sink to the bottom
and stay there, including work that did nothing wrong. Write it without the
boost first, run `bench sched`, and compare — measured here, fairness fell from
0.976 to 0.888, worst wait doubled from 5 to 9, and the background task lost a
third of its slices. Then add the boost.

Note what does *not* happen: `starved` stays 0. Ninety ticks is not long enough
for anyone to cross the 40-tick threshold under this workload, so the column you
might expect to catch it does not. The damage is real and it shows up in
fairness and worst-wait instead. A measurement that does not move is not the
same as a policy that is fine.

**A one-tick quantum at the top level.** The timer runs at the PIT's default
~18.2 Hz, the finest thing this kernel can see. With a one-tick quantum,
"blocked early" and "used it all" are the same measurement.

Measured, this one is subtler than it sounds: interactive wait only slipped from
2 to 3, and first-CPU actually improved. What really happened is that the policy
stopped being MLFQ at all — switches jumped 34 → 52 and fairness climbed to
0.9988, which is round robin's 0.9997 in all but name. **The growing quantum is
the entire difference between a multi-level queue and round robin with extra
bookkeeping.** Start at 2 and double with depth.

**Keeping state on `Task`.** It will work, and it will be the wrong shape, and
you will find out when you write your second brick.

**Forgetting that slot 3 today is not slot 3 from ten seconds ago.** Task ids
are recycled. If `on_ready` does not clear whatever you remembered about that
id, a fresh task inherits the sins of a dead one. This is what `on_ready` and
`on_exit` are for and it is the bug that will look like the scheduler being
haunted.

## Verify

Correctness first:

```bash
5am> selftest policy
```

Twenty-five checks across every registered brick, run against a synthetic task
table rather than the live machine — so a broken policy is a failed line rather
than a console that stops answering. It checks the things that have exactly one
right answer:

- never picks a task that is not runnable
- always picks somebody when somebody is runnable
- picks nobody when nobody is
- never asks for a quantum of zero

Then quality:

```bash
5am> selftest
5am> bench sched
5am> sched mlfq
5am> timeline 80
```

**These two are not the same question, and keeping them apart is most of what
this lab is teaching.** `fifo` passes every conformance check and will still
hang your machine, and it is supposed to. Conformance is safety: get it wrong
and the scheduler resumes a stack belonging to nobody. Quality is a trade-off
you measure and then argue about.

**Record your own baseline before you delete anything.** Run `bench sched` on
the working kernel and write the `mlfq` row down. The absolute numbers in this
document drifted once already — they were measured before an unrelated change to
how the mechanism counts idle ticks, and every figure in the table moved. What
did not move was the shape: `prio` best at latency and starving somebody, `mlfq`
matching it at a third of the switches with nobody starved.

So compare against *your* baseline, not against the printed one. If your version
has the same shape, you have written a multi-level feedback queue.

## Do not trust the first table

The machine has a starvation watchdog: if a runnable task goes 100 ticks
unpicked, it silently reinstalls `aging` and tells you afterwards. It is off
during `bench sched` — a run that got rescued halfway through is not a
measurement of anything — but it is on the rest of the time, so a policy that
looks fine interactively may have been quietly rescued.

If you see this, your policy did not work; the watchdog did:

```
  [sched] task 2 was runnable and unpicked for 100 ticks under `mlfq`.
          that is starvation, and it is what this policy does.
          the watchdog put `aging` back so you could read this.
```

Remember the one rule from [README.md](README.md): a passing transcript is not a
passing test. A scheduler that prints a plausible first line is the single
easiest thing in this kernel to fool yourself about, because every wrong answer
still produces a machine that runs.

## Going further

**Write a brick nobody has written here.** Lottery scheduling: give each task
tickets, draw one at random, run the winner. It needs a random number generator
this kernel does not have — `rdtsc` is a reasonable place to start, and asking
why that is a bad source of randomness is its own lesson. Roughly fifteen lines,
and it gets fairness without any bookkeeping at all, which is a genuinely
surprising result to produce yourself.

Then earliest-deadline-first, and discover that the `Policy` trait has nowhere
to put a deadline — so either the contract grows or `priority` gets reinterpreted
as one. Both answers are defensible. Working out which is the argument every
real kernel interface has had.

**Then break the mechanism instead of the policy.** Everything above assumes
`choose` asks the right question. Open `sched.rs` and find where `on_yield` is
called with `remaining == 0`. What happens to a task that is picked, blocks
after one tick of a sixteen-tick quantum, wakes, and is picked again? Is it
demoted? Should it be? The trait has no `on_wake` hook yet, and the comment
above `on_exit` says why. Add one, and you will find out immediately whether it
changes what `Aging` does — which is the reason it was left out.
