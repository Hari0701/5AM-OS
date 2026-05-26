# SMP: bringing up a second processor

> The first attempt was reverted after producing nine boot banners and no
> information. What follows is the debugging session that fixed it, kept in
> full because the method mattered more than either bug.

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

## Assembling it

Two syntax problems, both familiar from elsewhere in this project:

- Label arithmetic inside a memory operand is rejected. `.set` collapses each
  address into a single symbol first — the same fix the ELF loader needed.
- Neither assembler parser would take a far jump with an immediate selector, so
  both are emitted as opcodes: `0x66 0xEA` in 16-bit mode, `0xEA` in 32-bit.

## Then it reset the machine, nine times

Booting with four processors produced nine boot banners and nothing else. Three
guesses were available and all of them were wrong, which is the point of what
came next.

**The method.** Two changes made the difference. Move the whole sequence out of
boot and behind a shell command, so a fault *prints* instead of resetting.
And have the trampoline write a progress byte at each stage, so "how far did it
get" is a number rather than an inference:

```
5am> smp apic       the APIC answers -- so the first guess was wrong
5am> smp install    trampoline in place, progress byte 0
5am> smp wake       ... and the answer
```

Run with `-d int,cpu_reset -no-reboot` and QEMU prints the register state at the
fault, which is where both bugs were actually found.

## Bug one: descriptor tables are per-processor

```
RIP=000001000002b5f4   CS=0018   GDT=0000000000008090
IDT= 0000000000000000
check_exception old: 0x8 new 0xe
```

The processor had reached kernel code — so the trampoline worked — and was
running on the *trampoline's* GDT with a **null IDT**. `lgdt` and `lidt` load
registers that belong to one core. The first processor executing them did
nothing whatever for this one, and a core with a null IDT cannot dispatch its
first exception: double fault, cannot dispatch that either, triple fault, reset,
nothing printed.

## Bug two: shared page tables, unshared flags

Loading the tables first moved the fault but did not remove it:

```
v=0e e=0008  IP=0018:000001000002a7d4  CR2=00000100000fcd98
EFER=0000000000000500
```

Error code 8 is bit 3: a **reserved bit violation**, not a missing page. And
`EFER` has LME and LMA but not NXE.

The first processor turned NXE on when this kernel gained no-execute pages, so
the page tables have bit 63 set on everything not executable. On a core where
NXE is clear, **bit 63 is a reserved bit** — so walking the same tables faults on
the fourth instruction, before anything can say so.

> Page tables are shared between processors. The flags that decide how to read
> them are not.

The trampoline now sets NXE alongside LME, and CR4.OSFXSR beside PAE for the
same reason: this kernel is compiled with SSE, and a core without it faults on
the first vector instruction.

```
  it got to stage 6:
    3  long mode, paging on, identity map survived
    4  running Rust on its own stack
    5  loaded its own GDT and IDT
    6  past the point that used to triple fault

  2 processors awake.
```

That `[smp ]` line is printed by the *other* core, through the shared console
lock, without interleaving — the first real evidence that the spinlock's atomic
compare-exchange was doing something. It looked like ceremony when there was one
processor.

## What is still not done

Starting the cores is step one of four, and the other three are where the kernel
actually changes:

2. **Per-CPU state.** `CURRENT` must become per-core, and each core needs its own
   TSS — sharing one means two trap frames landing on the same stack. The woken
   core deliberately does not `ltr` for exactly this reason.
3. **Real locks** on the frame allocator, the heap, the task table and the page
   tables. Every `without_interrupts` in this tree means *"nothing else runs on
   this core"*, which is a complete argument on one processor and worth nothing
   on two.
4. **TLB shootdown.** Unmapping a page on one core leaves stale translations in
   every other core's TLB, and only an inter-processor interrupt fixes it.
   Without it `unmap_page` is silently wrong on a multiprocessor.

So the other cores are started and then **parked**, on purpose. They are not in
the scheduler and they touch nothing shared except the console, whose lock is
already a real one.

## The rules this cost

- **A reset tells you nothing. Move the code somewhere a fault can print.** The
  first attempt produced nine boot banners; the second produced a register dump
  naming the exact bug, and the only difference was where the call was made from.
- **Make progress a number.** "It does not work" is not a bug report, even to
  yourself. Six stages and a byte turned it into one.
