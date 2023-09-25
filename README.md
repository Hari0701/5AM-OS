# 5AM-OS

An x86_64 kernel, written in Rust, whose purpose is to **explain itself**.

Most teaching operating systems are a normal kernel plus a textbook. This one
puts the teaching inside the machine: it narrates its own boot, and its shell
reads live CPU state to explain what is under you. `explain gdt` does not print
a description of a GDT — it asks the CPU where its GDT is, walks it, and decodes
the bytes that are actually there.

```
5am> explain rings
PRIVILEGE RINGS
  x86 has four; everyone uses two.

  Current CS = 0x0008 -> ring 0

  Ring 0 can execute any instruction, read any register,
  touch any memory. Ring 3 cannot do I/O, cannot load
  descriptor tables, cannot disable interrupts. Every
  program you have ever run was in ring 3, asking a ring 0
  kernel for permission. You are currently the thing that
  grants permission.
```

---

## Running it

Needs Rust nightly and QEMU. Nothing else — no cross-compiler, no linker to
build.

```bash
rustup toolchain install nightly
brew install qemu          # or your platform's package manager
```

```bash
./run.sh
```

That builds the kernel for bare metal, wraps it in a bootable disk image, and
boots it in QEMU with the serial console attached to your terminal. Quit with
**Ctrl-A** then **X**.

---

## What it does so far

| Subsystem | State | What it taught |
| --- | --- | --- |
| Serial (16550 UART) | working | Port I/O, and why serial outlives every other console |
| Boot narration | working | What the firmware and bootloader did before our first instruction |
| GDT + TSS | working | Segmentation's remains in 64-bit mode; privilege levels |
| IST stack | working | Why a stack overflow reboots a machine, and how to survive it |
| IDT + exceptions | working | Faults you can handle vs. faults that end you |
| PIC + timer | working | Why the 8259's defaults collide with Intel's own vectors |
| PS/2 keyboard | working | Scancodes are not characters; layouts are software |
| Shell | working | — |
| Paging / allocator | not yet | |
| Multitasking | not yet | |

### Shell commands

```
help              this list
explain <topic>   read the machine and explain a subsystem
                  topics: boot gdt idt interrupts paging rings serial keyboard
regs              live control registers
gdt               decode every GDT entry
idt               which interrupt vectors are wired up
mem               physical memory map from the firmware
uptime            timer ticks since boot
fault <kind>      deliberately break something: int3 | div0 | page | stack
clear             clear the screen
```

`fault stack` is the interesting one. It recurses until the kernel stack runs
out, which faults, and faulting while handling a fault is a **double fault**.
Without a known-good stack to land on, that becomes a triple fault and the
machine silently resets. 5AM-OS survives it and tells you why:

```
5am> fault stack
  recursing until the stack runs out ...

[trap] DOUBLE FAULT at 0x10000005098
       A fault occurred while handling a fault.
       Reached this handler on the IST stack, which is the only
       reason you are reading this instead of watching a reboot.
```

---

## Layout

```
kernel/          the OS. compiled for x86_64-unknown-none, #![no_std]
  serial.rs      16550 UART. the first thing that can speak
  gdt.rs         segment descriptors + TSS + the double-fault stack
  interrupts.rs  IDT, exception handlers, 8259 PIC
  keyboard.rs    PS/2 scancodes -> characters
  shell.rs       the REPL, and every `explain` topic
  narrate.rs     the teaching layer for boot
boot/            runs on your machine: wraps the kernel in a disk image
run.sh           build + boot
```

Dependencies are deliberately near-zero: `bootloader_api` for the handover
struct, and that is all. Everything else — port I/O, descriptor tables,
interrupt handlers, the scancode table — is written out longhand in this repo,
because reading it is the point.

---

## Known limitations

- **UEFI images are disabled.** The `bootloader` crate's UEFI stage pins a
  version of the `uefi` crate that does not link against current nightly. BIOS
  boot works, which is all QEMU needs. See the note in `boot/Cargo.toml`.
- **The mode switch is borrowed.** Real mode → protected → long mode is done by
  the `bootloader` crate, not by us. That is the most interesting part of boot,
  and writing our own stage is on the list.
- **No memory allocator.** Everything is fixed-size buffers and statics. That is
  why there is no `String` anywhere in this codebase.
- **Single core, no scheduler.** One thread of execution, forever.

---

## License

MIT.
