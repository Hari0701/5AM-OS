# Labs

Reading this kernel will teach you less than you think. Every hard decision in it
is already made, correctly, with a comment explaining why — which is exactly the
problem. Nobody learned the stack-alignment rule from a comment. I learned it
from a `#GP` with error code 0 at three in the morning, and the comment is what
was left over.

So these labs take working code *away* from you and ask you to put it back.

## How they work

Each lab names a function, tells you to delete its body, and gives you a command
that says whether your replacement is right:

```bash
./run.sh
5am> selftest memory
    pass  map a page                   0x30000000 -> 0x1fddb000
    pass  the page is writable         read 0x5a4d05015a4d0501
    FAIL  translate finds the frame    walk says 0x0, expected 0x1fddb000
```

The tests run **inside the machine**, because that is the only honest place to
check whether a page really got mapped. `cargo test` on your laptop can check
that an ELF header parses; it cannot take a page fault.

## The loop

```bash
./run.sh                # boots in about two seconds
5am> selftest heap      # or memory, sync, sched, elf, fat -- or nothing, for all
```

`./run.sh` deliberately leaves the neural network out of the image. With it, the
bootloader drags 58 MB through BIOS disk services before the kernel gets its
first instruction and every boot costs you ninety seconds. Use `./run.sh --ai`
when you actually want `llm` or `ask`.

## Getting back to working code

```bash
git diff kernel/src/memory.rs      # what you changed
git checkout kernel/src/memory.rs  # give up and restore the original
```

Do the second one *after* you have a working version of your own, and read the
diff. The differences are where the interesting arguments are.

## Order

They build on each other. 1 and 2 are the foundation everything else stands on.

| # | Lab | You implement | Verify with |
| --- | --- | --- | --- |
| 1 | [Frame allocator](01-frame-allocator.md) | Physical memory bookkeeping | `selftest memory` |
| 2 | [Page tables](02-page-tables.md) | Virtual → physical translation | `selftest memory` |
| 3 | [Heap](03-heap.md) | The allocator behind `Vec` and `Box` | `selftest heap` |
| 4 | [Context switch](04-context-switch.md) | Making one CPU look like several | `tasks`, `workers` |
| 5 | [Semaphore](05-semaphore.md) | Waiting without spinning | `selftest sync`, `selftest sched` |
| 6 | [ELF loader](06-elf-loader.md) | Running a program you did not compile | `selftest elf` |
| 7 | [Filesystem](07-filesystem.md) | Reading a file off a real disk | `selftest fat` |

## One rule

**A passing transcript is not a passing test.** Three separate bugs shipped in
this kernel because the output looked exactly like what I was hoping to see —
including one that left the machine unable to receive a single keystroke, with a
perfect-looking transcript above it. When your lab prints the right first line,
that is the moment to get suspicious, not the moment to stop.

The full story of the worst one is in
[../docs/attempts/blocked-state.md](../docs/attempts/blocked-state.md). Read it
before lab 5.
