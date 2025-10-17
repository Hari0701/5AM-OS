# Lab 4 — The context switch

Making one CPU look like several. This is the lab where a wrong answer does not
print an error — it triple faults and the machine silently resets.

## The idea

**A task is register values plus a stack.** That is the whole thing.

To switch, you push every register onto the stack you are on, remember that
stack pointer, load a different one, and pop. The pops restore somebody else's
registers, and execution continues as them. The strange part is that the return
does not come back to you; something else continues, and later something returns
to you as though no time had passed.

The timer interrupt is the only handler in this kernel not written with Rust's
`extern "x86-interrupt"` ABI, and the reason is worth understanding before you
start. That ABI writes its own prologue and epilogue, which is exactly right
when a handler returns to whatever it interrupted — and useless when the entire
point is to return somewhere else. So vector 32 is hand-written assembly that
pushes registers in a known order, hands `rsp` to Rust, and resumes on whatever
Rust hands back.

## Do this

Open `kernel/src/task.rs` and delete the bodies of:

```rust
unsafe fn build_frame(top: u64, entry: u64) -> u64
extern "C" fn schedule(stack_pointer: u64) -> u64
```

Leave `timer_entry` alone the first time through — read it carefully instead,
and make `build_frame` match it. Then, if you want the real version of this lab,
delete `timer_entry` too and write the assembly yourself.

## The contract

- `build_frame` lays out a stack that *looks like* a task which was interrupted:
  a fake interrupt frame with the entry point where the return address goes, and
  zeros where the saved registers go. The first switch pops the zeros and
  `iretq`s into a function nobody ever called.
- `schedule` picks the next runnable task and returns its saved stack pointer.
  Whatever it returns becomes the stack the CPU resumes on — that single return
  value *is* the mechanism.

## Where people go wrong

- **Push order and pop order must mirror exactly.** `build_frame` writes what
  `timer_entry` will pop. One register out of place and you `iretq` into
  garbage.
- **A new task's stack must be misaligned by eight.** The ABI does not promise a
  function 16-byte alignment at its first instruction; it promises the stack was
  16-aligned *before the call*, and the call pushed eight bytes of return
  address. So every function begins with `rsp % 16 == 8` and the compiler emits
  `movaps` on that assumption. Hand a task a cleanly aligned stack and every SSE
  spill is off by eight — which the CPU reports as `#GP` with error code 0, a
  message that mentions nothing about alignment. This cost me a day.
- **EOI before the switch, not after.** Acknowledge the interrupt controller
  after you have switched away and it waits forever for an acknowledgement from
  a task that is no longer running. No further timer interrupts arrive, ever.
- **The scheduler runs in interrupt context.** Anything it touches, ordinary
  code must touch with interrupts disabled.

## Verify

```bash
./run.sh
5am> tasks
5am> workers
```

Surviving the first timer tick is the milestone — if `tasks` prints at all, your
register order is right. `workers` then spawns three tasks that genuinely
interleave.

If the machine reboots in a loop, you are triple faulting: the register order is
wrong, or the frame layout is.

## Going further

This scheduler is plain round robin. Add priorities — and then find out why the
version that used to be here was deleted: it starved two of three workers. Any
priority scheme needs an answer for starvation, and aging is the usual one.
