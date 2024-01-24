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
| FPU / SSE | working | The CPU boots unable to do float math |
| Answer engine | working | Decoding hardware beats guessing about it |
| Transformer (15M) | working | Real inference in ring 0, nothing linked in |
| AI bridge (COM2) | optional | Serial is the kernel's only reach into the outside world |
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
fault <kind>      break something: int3 | div0 | page | null | wild | stack
ask <question>    ask the kernel about itself -- answered inside this
                  machine, no network and no host process
bridge <question> send the question to a host process over COM2 instead
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

## Asking the kernel about itself

`ask` is answered **entirely inside 5AM-OS**. No network, no host process,
nothing to install:

```
5am> ask why is cr3 not the same as where my kernel loaded

PAGING -- why your addresses are fiction

  CR3 = 0x0000000001195000
  That is a PHYSICAL address, and it is the root of a four-level tree.
  ...
```

**It is not a model, and it says so** — run `ask how do you work`. It is keyword
matching over a hand-written corpus plus decoders that read live registers.
Every sentence was written by a human; every number came out of the hardware a
moment ago.

That trade is deliberate. What it gives up in flexibility it gains in being
*correct*, which is exactly what you want when the machine has just crashed:

```
5am> fault null

[trap] PAGE FAULT
       tried to touch : 0x0000000000000000
...
[why ] Reading the fault, not guessing at it:

       It was a write of data, from ring 0.
       The page is not mapped at all.

       Most likely cause, judging by the address:
       a null pointer dereference. Something unwrapped a null.
```

It decodes the real error-code bits and classifies the real faulting address, so
it distinguishes a null dereference from an unmapped page from a malformed
pointer — and it cannot invent a confident wrong answer, because it cannot
invent anything.

Writing `fault wild` taught the engine something true: a non-canonical address
raises a **general protection fault**, not a page fault, because the CPU rejects
the pointer shape before it ever consults the page tables. `#GP` carries no CR2,
so the engine reads the error code's selector fields instead.

---

## The neural network

5AM-OS runs a **Llama-2 transformer in ring 0** — 15 million parameters, a full
forward pass, with no operating system beneath it and nothing linked in.

```
5am> model
  A Llama-2 transformer, running in ring 0.

    dim         288
    hidden      768
    layers      6
    heads       6  (6 kv)
    vocab       32000 tokens
    context     256 tokens
```

There is no BLAS, no libm, and no allocator. Every matmul is the loop in
`llm.rs`; `exp` is a hand-written polynomial; every buffer is a fixed-size
static. The weights arrive as a **ramdisk** the bootloader places in memory,
because a kernel with no filesystem has no other way to read 58 MB — and they
are read in place, never copied.

### Getting the weights

They are not in this repository — 58 MB, and not ours to redistribute:

```bash
mkdir -p assets
curl -L -o assets/model.bin \
  https://huggingface.co/karpathy/tinyllamas/resolve/main/stories15M.bin
curl -L -o assets/tokenizer.bin \
  https://raw.githubusercontent.com/karpathy/llama2.c/master/tokenizer.bin
```

The build packs both into the ramdisk automatically. Without them the OS boots
exactly as before and `llm` says the model is missing.

### Three things this cost

**The CPU cannot do arithmetic when it boots.** x86_64 comes up with the FPU
*emulated* — a setting inherited from when the FPU was a chip you might not have
bought. An SSE instruction in that state raises an exception rather than
computing anything. Four bits fix it (`CR0.EM`, `CR0.MP`, `CR4.OSFXSR`,
`CR4.OSXMMEXCPT`), and `fpu` shows them.

**Enabling it late kills the machine silently.** LLVM does not reserve XMM
registers for float math — it uses them for ordinary struct and memory copies
too. So SSE instructions appear in code that has nothing to do with arithmetic,
including the code that would print a complaint. `fpu::init()` is now the very
first thing `kernel_main` does.

**The stock target forbids hardware float entirely.** `x86_64-unknown-none`
ships `+soft-float` with SSE off, and carries `rustc-abi: softfloat`, which pins
floats to general-purpose registers. `x86_64-5am_os.json` drops that field so
the normal SysV ABI applies, which is what SSE code expects. No precompiled
`core` exists for a custom target, hence `build-std` — and a slower first build.

### What it knows

Nothing about this kernel. It was trained on children's stories:

```
5am> llm once upon a time there was a little robot

once upon a time there was a little robot. He was very happy and loved to
play. One day he was playing in the park when he saw a big, shiny ball. He
wanted to play with it, so he ran over to it.
But when he got close, he saw that it was a big, scary monster! The robot was
so scared that he started to cry.
The monster said, "Don't be scared

[llm ] 96 tokens in ~13 s (237 ticks) -- 15M params, 6 layers
```

That is greedy decoding — always the most likely next token — which is why it
is deterministic and occasionally repetitive. Sampling with a temperature is a
small change to `argmax` in `llm.rs`.

Ask it about a page fault and it will invent something confident and wrong. That
is the honest split in this OS, and both halves are deliberate:

| | `ask` | `llm` |
| --- | --- | --- |
| What it is | keyword matching + hardware decoders | a real transformer |
| Correct? | yes, it reads the registers | often not |
| Can generalise? | no, only what it was taught | yes, to anything |
| Good for | why the machine just crashed | continuing a story |

Neither is pretending to be the other, and the code says so in both module docs.

**Speed.** About 7 tokens/second on x86 emulated under TCG on an ARM Mac —
faster than expected for a scalar matmul with no SIMD beyond what the compiler
finds. Boot takes ~90 seconds, almost all of it the bootloader pulling 58 MB
through BIOS disk services before the kernel ever runs.

---

## The AI bridge (optional)

5AM-OS can ask a language model about itself — including about its own crashes.

**The model does not run inside the kernel, and the code does not pretend it
does.** A kernel with no allocator, no filesystem and no network stack cannot
load a multi-gigabyte model. What it *can* do is write bytes to a serial port,
so that is what it does: the question, plus the live register state, goes out of
COM2 to `bridge/bridge.py` on the host, which calls the Claude API and writes
the answer back down the same wire.

The serial port is genuinely the only I/O this kernel has, so this is the whole
of its ability to reach the outside world — not a shortcut around one.

```bash
pip install -r bridge/requirements.txt
export ANTHROPIC_API_KEY=...
python3 bridge/bridge.py     # in one terminal
./run.sh                     # in another
```

```
5am> ask why is CR3 different from the address my kernel was loaded at?
```

Nothing else changes if the bridge is not running: `ask` times out after 90
seconds and tells you how to start it, and every other command is unaffected.

### Explaining its own crashes

The interesting part is that the page fault handler uses the same channel. When
the kernel faults, it ships its own wreckage — fault type, RIP, error code, CR2
— out of the port *from inside the handler*, with interrupts disabled, before it
halts:

```
5am> fault page

[trap] PAGE FAULT
       tried to touch : 0x00000000deadbeef
       ...
[ai  ] asking the bridge ...
```

### Wire protocol

Plain text, so you can watch it with `nc` or replace the bridge with anything:

```
-> 5AMOS/1 ASK                 (or FAULT)
-> state: cr0=0x... cr3=0x... ring=0 ticks=77 [fault=... rip=... cr2=...]
-> q: <question>
-> END
<- <answer lines>
<- END
```

The bridge is deliberately the dumbest possible participant. When 5AM-OS grows a
virtio-net driver and a TCP stack, it is deleted and the kernel calls the API
itself — the protocol above is shaped so that change touches nothing in the
kernel above `ai.rs`.

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
  ai.rs          the serial protocol for talking to a model
bridge/          runs on your machine: serial <-> Claude API
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
- **The AI bridge needs a host process.** The kernel cannot reach a network by
  itself yet, so `ask` talks to `bridge/bridge.py` over a serial port rather
  than to the API directly. Removing that dependency means writing a virtio-net
  driver and a TCP stack — a real milestone, not a small one.

---

## License

MIT.
