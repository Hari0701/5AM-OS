# Lab 9 — Choosing what to throw away

Do [lab 8](08-scheduler.md) first. This is the same shape one slot over, and the
point of doing both is that the shape is the lesson.

## The idea

Memory runs out. When a frame is wanted and none is free, something resident has
to go, and the kernel has to pick it.

The best page to evict is the one that will be needed furthest in the future.
That is provably optimal, it is called Bélády's MIN, and it is **unimplementable**
— it requires knowing what the program will do next. Every real algorithm is a
guess at the past standing in for the future, and specifically a guess at "least
recently used", because recently-used is the cheapest available correlate of
about-to-be-used.

Now look at what you have to guess *with*:

```rust
const ACCESSED: u64 = 1 << 5;
```

One bit per page. The CPU sets it whenever it translates through the entry, and
never clears it. That is the entire input. Every policy in `replace.rs` is a
different answer to *how do I make one bit enough?*, and the field exists
because one bit is not very much.

```
5am> paging
  page replacement policies -- `*` is installed:

   * clock   second chance, swept in a circle. one bit, made to be enough
     fifo    oldest resident page goes. shows Belady's anomaly
     nru     four classes from two bits, lowest non-empty wins
     random  no state, no scan, no bits. the control you compare against
```

## Before you start: the result worth knowing

```bash
./run.sh
5am> bench paging
```

```
  Belady's string:  1 2 3 4 1 2 5 1 2 3 4 5

    policy    3 frames  4 frames
    clock            9         9
    fifo             9        10  <- more memory, MORE faults
    nru              8         7
    random          10         6
```

Read the `fifo` row twice. **The machine was given more memory and did more
work.** Nine faults with three frames, ten with four, on the same references in
the same order.

Every other resource in a computer behaves the way you expect — more cache, more
disk, more cores, never worse. Memory does not, and Bélády found it in 1969.
Those are not simulated numbers: those are real pages in a real address space,
real accessed bits set by the CPU, real 4 KiB writes to the IDE disk.

The reason is that FIFO is not a **stack algorithm**. The set of pages it keeps
with three frames is not guaranteed to be a subset of what it keeps with four,
so adding a frame can rearrange the entire future. LRU and MIN are stack
algorithms and provably cannot do this. Clock is not one either, strictly, and
in practice does not.

Then look at the second table, and notice `nru` doing it too. Nobody planned
that; the benchmark found it.

## Do this

Open `kernel/src/replace.rs`, find `impl Replacer for Clock`, and delete the
body of `choose`. Leave `name`, `describe` and `reset`.

Write it again.

## The contract

```rust
fn choose(&mut self, pages: &PageSet) -> Option<usize>;
fn on_resident(&mut self, address: u64);
fn on_evicted(&mut self, address: u64);
fn reset(&mut self);
```

`PageSet` is the resident user pages of one address space: `len()`,
`address(i)`, `accessed(i)`, `dirty(i)`, `eligible(i)`, `clear_accessed(i)`.

Three things about it matter more than the algorithm.

**`clear_accessed` is the only mutation you are allowed**, and you need it.
The bit is set by hardware and cleared by nobody, so a policy that can only read
it sees every page as used and can never tell the difference between busy and
long-dead. Clearing is how you start a new observation window. It is safe
because it is harmless — the CPU sets it again on the next touch.

**`eligible` is not advice.** A page that is shared with another address space
has a second page table pointing at the same frame, and that table knows nothing
about swap slots. Take one and you hand away a frame another process is still
reading through. The mechanism filters those out before you see them *and*
re-checks whatever index you return, so a wrong answer costs you a refused
eviction rather than somebody else's memory. Do not rely on that; understand why
it is there.

**`accessed(i)` reads the live entry, not a snapshot.** Clock depends on seeing
the effect of its own previous pass — clear the bits on lap one, test them on
lap two. Cache the values at the top of `choose` and the hand goes round forever
taking nothing, which presents as the machine reporting it is out of memory
while holding pages it was about to release.

## Where people go wrong

**One lap instead of two.** If every page has been accessed, a single pass
clears bits and finds no victim, and you return `None` — out of memory, with
memory available. The second lap is what makes the sweep terminate.

**Forgetting the hand.** Restarting the sweep at index 0 every call is a
different and much worse policy: the first few pages of an address space absorb
every eviction and the rest are never even considered. The position is the only
state clock has and it exists for exactly this reason.

**Reading the snapshot instead of the entry.** See above. Like the one-lap
mistake, this ends with `choose` returning `None` while a page was available —
so `selftest replace` catches it on "finds the only candidate". What it does
*not* do is look broken: see the warning below.

**Believing `dirty` will help.** It is real hardware state and it is exposed,
and in this kernel it is not actionable: there are no file-backed pages, so
every victim costs a disk write whether it was modified or not. Elsewhere a
clean page can simply be dropped. `nru` uses it to *classify*, not to save work.

## Verify

Correctness first:

```bash
5am> selftest replace
```

Twenty checks across every registered brick, against fabricated page table
entries that belong to no address space — so "every candidate is already
swapped" and "exactly one of six may be taken" are one line each instead of a
paragraph of setup, and a broken policy is a failed line rather than a fault
inside the fault handler.

Then quality:

```bash
5am> selftest swap
5am> bench paging
5am> paging fifo
5am> bench paging
```

As in lab 8, these are different questions. Conformance is safety and has one
right answer — name an ineligible page and the mechanism would be writing out a
frame somebody else owns. Quality has no right answer, only reference strings
and trade-offs.

Your clock should match the numbers in the table above. If it does not, work out
which reference behaves differently and why before you change anything.

### Run the conformance suite first, and mean it

This is worth more than the rest of the section, and it was found by doing this
lab rather than by writing it.

Break the clock two different ways — take away the second lap, or cache the
accessed bits at the top of `choose` instead of re-reading them — and both
versions come back from `bench paging` with **six** faults where the working one
has nine. They look like an improvement.

They are not. Both bugs end with `choose` returning `None` when a page was
available, so nothing is evicted, so the resident set quietly grows past the
frame limit, so later references find their pages still there. You have not
written a better policy. You have written one that declines to do its job and
scored well because the benchmark let it.

The benchmark now counts refusals and flags the row:

```
    clock            6         5  <- REFUSED to evict; the cap was not held, ignore these
```

But the lesson survives the fix, because it generalises: **a measurement you
have not bounded will reward a component for not participating.** `selftest
replace` catches both of these instantly, which is why safety runs before
quality and not after.

## Going further

**Implement true LRU and find out what it costs.** Keep the pages in
recency order and move one to the front on every access — except you cannot,
because there is no notification when a page is *touched*. The hardware tells
you a page was accessed at some point, not when, and not in what order. Getting
real LRU means taking a fault on every first access after a reset, which is the
trade the aging algorithms exist to avoid. Working out why exact LRU is
impractical is worth more than implementing an approximation of it.

**Then the aging register.** Give each page an 8-bit shift register; on a timer,
shift right and put the accessed bit in the top. The page with the smallest
value is the least recently used, to eight levels of resolution. It needs
something this kernel does not have — a periodic sweep — so you will have to add
one, and deciding how often it should run is the entire tuning problem in
miniature.

**Then break the mechanism instead of the policy.** Every policy here is *local*:
it chooses among the pages of one address space, so a process cannot lose a
frame to another process. Real systems mostly do the opposite. Change
`evict_one` to consider every user page on the machine and you have global
replacement — better utilisation, and now one badly-behaved program can page
out everything else. That argument has been running since the 1960s and you can
have it with your own kernel.
