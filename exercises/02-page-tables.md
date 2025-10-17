# Lab 2 — Page tables

This is the one that makes an address a lie in a useful way.

## The idea

The CPU never touches the address in your pointer. It splits it into four
nine-bit indexes and a twelve-bit offset, then walks four tables in memory —
each entry pointing at the next table — until it reaches the frame. Every memory
access your kernel makes is doing this, in hardware, constantly.

Four levels, nine bits each, then the offset:

```
  63    48 47      39 38      30 29      21 20      12 11         0
  [ sign  ][ level 4 ][ level 3 ][ level 2 ][ level 1 ][   offset   ]
```

There is a bootstrapping problem worth noticing before you start. Page table
entries hold *physical* addresses, and every address the kernel uses is
*virtual*. To read the table whose physical address is in CR3, you need a
virtual address that maps to it — which you cannot construct without first
reading the table. The escape is that the bootloader mapped all of physical
memory at a known offset before the kernel started.

## Do this

Open `kernel/src/memory.rs` and delete the bodies of:

```rust
fn indexes(address: u64) -> [usize; 4]
pub unsafe fn map_page(virtual_address: u64, frame: u64, flags: u64) -> Result<(), &'static str>
pub fn translate(virtual_address: u64) -> Option<(u64, [u64; 4])>
```

Write them again. Start with `indexes` — it is four lines and everything else
depends on getting the shifts right.

## The contract

- `map_page` makes `virtual_address` resolve to `frame`, creating any missing
  tables on the way, and returns `Err` if something is already mapped there.
- `translate` follows the same path and reports the physical address.
- Both use `physical_to_virtual` to read a table.

## Where people go wrong

- **A fresh table must be zeroed.** Whatever garbage was in that frame will be
  read as present entries pointing at arbitrary physical addresses. This does
  not fault — it silently corrupts something else.
- **`invlpg` after you change a mapping.** The CPU caches translations and will
  happily keep using a stale one. Skip this and the page stays readable at the
  old address after the frame belongs to somebody else, which shows up as
  corruption in a subsystem you did not touch.
- **The user bit is checked at every level.** Setting it only on the final entry
  is a classic afternoon: the page says ring 3 may read it, a table above it
  says otherwise, and the fault points at the page rather than at the table
  three levels up that actually refused.
- **Wrong shifts produce plausible garbage, not a crash.** This is why
  `selftest memory` checks that `translate` returns *the frame you asked for*
  rather than merely returning something.

## Verify

```bash
./run.sh
5am> selftest memory
5am> translate 444444440000
```

`translate` prints the whole walk, entry by entry. Use it — being able to *see*
the four reads is most of the point of implementing this.

## Going further

Nothing here handles huge pages, where the walk stops early at level 2 or 3 and
the rest of the address is the offset. `translate` reads the bit; `map_page`
cannot make one. Why would you want 2 MiB pages at all? (Answer: the TLB has
very few entries.)
