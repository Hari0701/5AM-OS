# Lab 3 — The heap

The line where `Vec`, `String` and `Box` start existing. Until this works, the
kernel cannot hold a piece of data whose size it did not know at compile time.

## The idea

You have 2 MiB of mapped memory. You need to hand out pieces of it in any size,
in any order, and take them back in any order, forever, without gradually
turning it into confetti.

The structure here is a **free list sorted by address**: every hole knows its
size and points to the next hole. Allocation walks the list for one big enough.
Freeing puts the hole back in address order — and then merges it with its
neighbours if they are adjacent.

That merge is the whole game. Without it the number of holes only ever goes up:
allocate and free a thousand times and you have a thousand fragments, plenty of
free bytes in total, and no single hole big enough for anything.

## Do this

Open `kernel/src/heap.rs` and delete the bodies of:

```rust
fn take_region(&mut self, size: usize, align: usize) -> Option<(usize, usize)>
fn give_back(&mut self, start: usize, size: usize)
```

Write them again.

## The contract

- `take_region` finds a hole that fits `size` bytes at `align` alignment, splits
  it, and returns where the allocation starts.
- `give_back` inserts a freed region and merges it with any adjacent hole,
  **both** the one before and the one after.
- Alignment padding at the front of a hole must not be lost.

## Where people go wrong

- **Merging forwards only.** Free A then B where they are adjacent and it works;
  free B then A and it does not. Both directions, every time.
- **Losing the alignment padding.** If a hole starts at 0x1004 and you need
  8-byte alignment, the four bytes you skipped are still free — silently
  dropping them is a leak that grows with every allocation.
- **A hole too small to hold a hole.** If splitting leaves eight bytes, you
  cannot store a header in it. Decide what to do about that and be consistent.

## Verify

```bash
./run.sh
5am> selftest heap
5am> heap
```

`selftest heap` runs 200 interleaved allocate/free cycles and asserts that every
byte comes back **and that the hole count returns to one**. The second half is
the one that catches a missing merge; the first half passes without it.

I shipped a version of this whose own documentation claimed it coalesced, and it
did not. The test is why you will not.

## Going further

This is a first-fit allocator: it takes the first hole that fits. Look up
best-fit and next-fit, and think about which fragments worse. Then look at what
a real kernel does instead (slab allocation) and why "objects of one size" makes
the problem disappear rather than solving it.
