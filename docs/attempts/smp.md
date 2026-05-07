# SMP: an attempt, reverted

The other processors were started, the machine rebooted in a loop, and the code
is not in the tree. The trampoline that got as far as assembling is kept beside
this file as `smp-trampoline.rs.txt`.

## What is actually required

Not one feature. SMP is a property of the whole kernel, and this one does not
have it.

Grep the tree for `without_interrupts` and read what each use is claiming. Every
one means *"nothing else can run on this core, so nothing else can touch this"*
— which is a complete argument on one processor and worth nothing on two. The
frame allocator, the heap, the task table, the swap bitmap and the FAT layer all
rest on it.

So the work is, in order:

1. **Start the other cores.** Local APIC, an INIT–SIPI–SIPI sequence, and a
   trampoline in the first megabyte that walks real mode → protected → long in
   about forty instructions, because the startup protocol has not changed since
   1978.
2. **Per-CPU state.** `CURRENT` becomes per-core; each core needs its own TSS,
   its own kernel stack, and a `GS` base to find them through.
3. **Real locks on everything shared.** Not `without_interrupts` — actual
   spinlocks, on the allocator, the heap, the task table and the page tables.
4. **TLB shootdown.** Unmapping a page on one core leaves stale translations in
   every other core's TLB, and the only fix is an interrupt telling them to
   flush. Without it, `unmap_page` is silently wrong on a multiprocessor.

Step 1 alone is not SMP. Steps 2–4 are where the kernel actually changes, and
none of them can be tested until step 1 works.

## What happened

The trampoline assembled after two syntax problems, both familiar:

- Label arithmetic inside a memory operand is rejected. `.set` collapses each
  address into a single symbol first — the same fix the ELF loader needed.
- Neither assembler parser would take a far jump with an immediate selector, so
  both are emitted as opcodes: `0x66 0xEA` in 16-bit mode, `0xEA` in 32-bit.

Then booting with four processors produced **nine boot banners** — a reset loop.
Something in `start_others` triple faults the machine before the shell appears,
and the most likely candidates, untested:

- `physical_to_virtual(0xFEE00000)` for the APIC. The bootloader maps physical
  *memory*; the APIC is memory-mapped I/O above RAM and may simply not be in
  that mapping. Touching it would fault.
- The identity mapping at `0x8000` may collide with something the bootloader
  already put there, or may not survive into the page tables the AP loads.
- The AP itself may be faulting after `mov cr0` and taking the machine with it.

Distinguishing those needs the QEMU monitor and `-d int,cpu_reset`, one core at
a time, which is where the next attempt should start.

## Why it was reverted rather than left in

A reboot loop is worse than a missing feature. Everything else in this kernel is
tested and works; a broken boot makes all 53 of those checks unreachable, and
"it mostly boots" is not a state anything can be built on.

## The rule this is an instance of

Ship the machine working. A feature that is half-done can live in a document; a
kernel that does not boot cannot live in `main`.
