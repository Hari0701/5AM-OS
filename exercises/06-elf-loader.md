# Lab 6 — The ELF loader

The difference between a kernel that runs code compiled into it and one that
runs programs.

## The idea

An ELF file has **two completely separate tables of contents**, and the surprise
is which one matters.

*Sections* — `.text`, `.rodata`, `.bss` — are for the linker. A loader may
ignore them entirely; a stripped binary has none and still runs. *Segments*,
described by program headers, are for the loader, and they are all you need.

Loading a program is: walk the program headers, and for each one that says
`PT_LOAD`, put those bytes at that address with those permissions. That is the
entire algorithm. Everything else you write will be checking.

## Do this

Open `kernel/src/elf.rs` and delete the body of `load`. Write it again.

## The contract

- Validate the header: magic, 64-bit, little-endian, `ET_EXEC`, x86-64.
- For each `PT_LOAD`, map pages and copy `file_size` bytes to `virtual_address`.
- Apply permissions from `p_flags`.
- Return the entry point and the list of pages you mapped.
- Refuse anything you do not fully understand.

## Where people go wrong

- **`memsz` can exceed `filesz`.** The difference is `.bss` — memory the file
  promises but does not contain, because storing megabytes of zeroes on disk
  would be absurd. You owe the program that memory, **zeroed**. Not as
  tidiness: those frames held something before, and handing them over uncleared
  leaks it into a program that should not see it, while looking like it worked.
- **Permissions come after all the copying.** Seal as you go and you will seal a
  page a later segment still needs to write to.
- **The code page has to be writable while you load it.** Map it read-only from
  the start and the kernel's own copy *into* it faults — `CR0.WP` means ring 0
  obeys the read-only bit too, which is the entire reason that bit is worth
  setting.
- **Do not trust the file.** Every offset and size in it was chosen by somebody
  else. Check them against the file length and against where userspace ends. A
  loader that guesses is a loader that executes attacker-chosen bytes.
- **Remember the pages you mapped.** Nothing else can know which ones to free
  when the program exits, and a loader that forgets leaks them for the lifetime
  of the machine.

## Verify

```bash
./run.sh
5am> selftest elf
5am> exec hello.elf
```

`selftest elf` checks that it loads, that every page comes back, and — just as
importantly — that a corrupted magic and a truncated file are **refused**.

## Going further

This only handles `ET_EXEC` at a fixed address. Position-independent
executables are type `ET_DYN`, and running one means choosing a base and
applying relocations. That is how every program on your own machine is loaded,
and it is why they can be loaded anywhere.
