# Blocked tasks: the bug, and how it was actually found

This file was written first as a record of a failure — a blocked/sleeping task
state that deadlocked and got left out of the tree. It is kept, updated, because
the way the bug was eventually found is worth more than the feature.

## The symptom

Three tasks, one semaphore, each holding it across a sleep. One ran to
completion, one managed a single round, one never executed an instruction. No
error, no fault, no output — the machine stayed responsive and two tasks simply
never ran again.

## What did not find it

Reading the code. I went through the wake path repeatedly and it was correct
every time, because it *was* correct. Two real bugs turned up on the way and
neither was the cause:

1. **A lost wakeup.** `Semaphore::wait` tested the count with interrupts
   disabled and blocked *outside* that region. A `signal` landing in the gap
   wakes nobody. I had written the warning comment for exactly this on the
   function directly above it.
2. **A `wait` that polled.** `wait_for` slept a tick and re-checked, so the
   waiter became runnable every tick and starved the task it was waiting for.

Fixing both improved the symptom and did not remove it. I also suspected the
priority scheduler and deleted it; plain round robin behaved identically.

## What found it

**Bisection, then a single number.**

Four variants of the same demo, each adding one feature:

| variant | what it does | result |
| --- | --- | --- |
| `plain` | spawn, print, exit | works |
| `sleep` | blocks on a deadline | works, interleaves cleanly |
| `lock` | takes the semaphore, nothing slow inside | works |
| `hold` | takes the semaphore and sleeps while holding it | **deadlocks** |

That narrowed six suspects to one. Then printing the semaphore count alongside
the task table settled it:

```
  WORK_LOCK count = 1
  id  name      state     switches
  0   shell     running   1
  1   w         waiting   7
  2   w         waiting   6
```

The resource was **available** and two tasks were asleep waiting for it. That is
not a lost signal. That is a lost read.

## The cause

The count was a plain `u32` in an `UnsafeCell`, read and written with interrupts
disabled. That is correct mutual exclusion and still completely broken.

`wait` re-tests the value in a loop. Nothing in a plain load tells the compiler
the value can change, and on a single core LLVM can see no other writer — so it
is entitled to read once and reuse the answer forever. The waiter spins on a
register holding zero while memory holds one.

> Disabling interrupts stops another **task** from running.
> It says nothing whatever to the **compiler**.

Different problems, different tools. This project has now made that mistake
twice — the first time with the keyboard ring buffer's indices, fixed with
`read_volatile`.

## The fix

`AtomicU32`, compare-exchange for the decrement, `fetch_add` for the signal,
count raised before the wake so a waiter that runs instantly finds the resource
there. Plus a `compiler_fence` at the top of `block_until`'s loop, so any
condition it tests is genuinely re-read each time round.

## Rules this cost

- **A subsystem is not done because its demo prints the right first line.** Every
  broken version printed `worker 0 round 0: counter is now 1` and looked fine.
  Test the case with *contention*, not the case with one participant.
- **When reading the code twice has not found it, stop reading and bisect.**
  Narrowing which feature breaks beats reasoning about which line looks wrong.
- **Print the state you are theorising about.** One line showing `count = 1`
  next to two sleeping tasks ended an hour of plausible wrong explanations.
