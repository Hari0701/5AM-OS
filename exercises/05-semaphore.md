# Lab 5 — Waiting without spinning

Read [../docs/attempts/blocked-state.md](../docs/attempts/blocked-state.md)
before you start this one. It is the postmortem of the worst bug in this
repository, and it lives in the exact code you are about to write.

## The idea

A spinlock burns the CPU while it waits, which is fine for something short and
catastrophic for something long. Holding one across a fifteen-second neural
network run would suspend the timer for fifteen seconds: a machine that has a
scheduler and never runs it.

What you want instead is to *sleep* — to be taken out of the scheduler's
rotation entirely until somebody says the thing you wanted is available. That is
only expressible once a task can be marked not-runnable, which is what
`State::Blocked` is for.

## Do this

Open `kernel/src/sync.rs` and delete the body of `Semaphore` — `wait`,
`try_take`, `signal`. Then open `kernel/src/task.rs` and delete `block_until`
and `wake_all`.

Write them again.

## The contract

- `wait` blocks until the count is positive, then decrements it.
- `signal` increments and wakes anyone waiting.
- `block_until(channel, test)` blocks the current task until `test` returns
  `Some`.
- `wake_all(channel)` makes every task blocked on that channel runnable.

## Where people go wrong

There are exactly two ways to get this wrong, and this kernel shipped both.

**1. The lost wakeup.** The test and the block must be one uninterruptible step.
Test the condition, get preempted, have the waker signal *before* you mark
yourself blocked, and you sleep waiting for something that already happened.
Nothing reports an error; the task simply never runs again.

I wrote the comment warning about this on the function directly above the one
where I then implemented it.

**2. The load that is never re-read.** `wait` re-tests the count in a loop. If
that count is a plain `u32`, nothing tells the compiler it can change — on a
single core LLVM can see no other writer, so it reads once and reuses the answer
forever. The waiter spins on a register holding zero while memory holds one.

> Disabling interrupts stops another **task** from running.
> It says nothing whatever to the **compiler**.

Those are different problems needing different tools: `cli` for one, atomics and
fences for the other. This project made that mistake twice — the second time it
deadlocked three tasks against a semaphore whose count was visibly 1.

Also: **wake all, not one.** Waking a single waiter needs a policy for which,
and every policy is wrong for somebody. Let them all re-test. That is the same
reason condition variables are always used in a `while` and never an `if` — a
wakeup is a hint that the world may have changed, never a promise that it
changed for you.

## Verify

```bash
./run.sh
5am> selftest sync
5am> selftest sched
5am> workers
```

`selftest sched` is the one that matters: three tasks, one semaphore, a shared
counter, nine increments. It deadlocked the broken version.

**Test with contention.** Every broken version of this printed
`worker 0 round 0: counter is now 1` and looked like it worked.

## Going further

`wait` cannot time out and cannot be interrupted. Add a deadline. Then ask what
should happen to a task that is blocked on a semaphore nobody will ever signal,
and you have arrived at why real kernels care about deadlock detection.
