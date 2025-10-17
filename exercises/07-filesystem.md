# Lab 7 — Reading a file

Until this works, a program has to be compiled into the kernel to exist.
`include_bytes!` is not a filesystem; it is a promise made by the compiler.

## The idea

FAT is one array — the File Allocation Table — with one entry per cluster, each
holding the number of the *next* cluster in the same file. **A file is a linked
list, and the list lives in a table at the front of the disk rather than in the
data.**

Everything good and bad about FAT follows from that one decision. Appending is
trivial. Seeking to byte 40,000 means walking from the beginning, because
nothing indexes it. And if the table is damaged the data is all still there, in
an order nothing records.

## Do this

Open `kernel/src/fat.rs` and delete the bodies of:

```rust
pub fn mount() -> Result<Volume, &'static str>
fn next_cluster(&self, cluster: u16) -> Result<u16, &'static str>
pub fn read_file(&self, entry: &Entry) -> Result<Vec<u8>, &'static str>
```

Write them again. `ata::read` gives you sectors; everything above that is yours.

## The contract

- `mount` reads sector 0, parses the BIOS Parameter Block, and works out where
  the FAT, the root directory and the data region begin.
- `next_cluster` reads one FAT entry.
- `read_file` walks the chain and returns the bytes.

## Where people go wrong

- **Cluster numbering starts at 2.** Entries 0 and 1 of the table describe no
  cluster at all, which is why every cluster-to-sector calculation ever written
  subtracts two.
- **Nothing on the volume says it is FAT16.** The "16" is the width of a table
  entry, and a reader is expected to compute the cluster count and take the
  answer — fewer than 4085 makes it FAT12, more than 65524 makes it FAT32. This
  is why resizing a volume can silently turn it into a different filesystem.
- **Every region's position is derived, not stored.** The BPB says how many of
  each there are; where they start is arithmetic. Wrong arithmetic gives you a
  filesystem that reads plausible garbage rather than failing, which is far
  harder to debug than a refusal.
- **Bound the chain walk.** A corrupt table can point a cluster at itself. "The
  shell hung" is a poor way to report "the filesystem is damaged".
- **A zero first byte in a directory entry means stop**, not skip: every slot
  after it is unused too. `0xE5` means skip.

## Verify

```bash
./run.sh
5am> selftest fat
5am> ls
5am> cat readme.txt
5am> exec hello.elf
```

And the check no amount of your own code can give you — mount the image on your
own machine:

```bash
hdiutil attach target/fs.img      # macOS
```

If Finder opens it and 5AM-OS reads it, you and Apple implemented the same
specification. That is the only real test of a filesystem reader, and it is why
this project uses FAT instead of something nicer that I made up.

## Going further

This is read-only, and writing is most of what a filesystem *is*: allocating
clusters, updating both copies of the table, and never leaving the volume
inconsistent if the power fails halfway. That last clause is where journalling
comes from.
