# Blocked tasks and sleeping: an attempt that does not work

Written down because the failure is more instructive than the feature would
have been, and because the next person to try this — probably me — should not
rediscover it from scratch.

## What was built

- `State::Blocked(Reason)` on a task, where `Reason` is either `Until(tick)` or
  `Channel(u64)`. Keeping the reason inside the state makes "blocked on nothing"
  unrepresentable.
- `block_until(channel, test)` — tests a condition and blocks atomically, with
  interrupts disabled across both halves.
- `wake_all(channel)`, `sleep(ticks)`, `wait_for(task)`.
- A `Semaphore` that sleeps rather than spins, built on the above.
- The shell blocking for a keystroke instead of polling and halting.
- The timer waking any task whose `Until` deadline has passed, which is the
  entire implementation of `sleep` — no timer subsystem, just a scheduler that
  checks the clock before it chooses.

## What worked

The shell genuinely blocks and wakes on a keystroke: commands typed over serial
still arrive, and the task leaves the run queue in between. One worker task ran
three full rounds through the semaphore, taking and releasing it correctly.

## What did not

With three worker tasks contending, **two of them stall permanently.** One runs
to completion, one manages a single round, one never executes an instruction.
Reproduce with the `workers` command in that branch.

Two real bugs were found and fixed along the way, and neither was the cause:

1. **The lost wakeup.** `Semaphore::wait` tested the count with interrupts
   disabled and then called `block_on` *outside* that region. A `signal` landing
   in the gap wakes nobody, and the waiter sleeps forever. I had written a
   comment warning about exactly this on the function directly above it.
2. **A `wait` that polled.** `wait_for` slept a tick and re-checked. That looks
   equivalent to blocking and is not: the waiter becomes runnable on every tick,
   so a high-priority waiter starves the very task it is waiting for. A poll
   loop turns "wait for you" into "prevent you".

Fixing both improved the symptom and did not remove it. A priority scheduler
with aging was suspected and removed; plain round-robin behaves identically, so
the fault is in the blocking and waking, not in the choosing.

## Where to look next

- Whether `sleep`'s `while ticks() < deadline { yield_now() }` re-blocks
  correctly after a spurious wake, or leaves the task `Ready` and spinning.
- Whether a task woken while inside `block_until`'s `hlt` re-tests its condition
  before parking again, or parks unconditionally and loses the wakeup a second
  time.
- Whether the scheduler's "current task is blocked and nothing else is runnable"
  fallback can resume a `Blocked` task in a way that skips its re-test.

## The rule this cost

A subsystem is not done because its demo prints the right first line. Every
version of this printed `worker 0 round 0: counter is now 1` and looked like it
worked. Test the case with *contention*, not the case with one participant.
