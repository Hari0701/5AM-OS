# Lab 1 — The frame allocator

Before this exists, memory is something the bootloader handed you. After it,
memory is something you own and can give away. Everything in the kernel that
allocates anything stands on this.

## The idea

Physical memory is an array of 4 KiB frames. The firmware tells you which ranges
are usable. Your job is to keep track of which of those frames are free, and to
hand them out and take them back one at a time.

The catch is that you have nowhere to put the bookkeeping. This runs before the
heap exists — there is no `Vec` to keep a list in, and a bitmap for 512 MiB of
memory is 16 KiB you have not got anywhere to put either.

The trick is to store the list **inside the free frames themselves**. A free
frame is not being used for anything, so its first eight bytes can hold the
address of the next free frame. The allocator is then one `u64`: the address of
the first one.

## Do this

Open `kernel/src/memory.rs` and delete the bodies of:

```rust
impl FrameAllocator {
    pub fn allocate(&mut self) -> Option<u64>
    pub unsafe fn deallocate(&mut self, frame: u64)
}
```

Keep the struct and `init`. Write them again.

## The contract

- `allocate` returns the physical address of a frame nobody else has, or `None`.
- `deallocate` puts one back.
- A frame that has been allocated and freed must be handed out again.
- `self.free` tracks how many are available.

## Where people go wrong

- **You cannot dereference a physical address.** The kernel runs on virtual
  addresses. `physical_to_virtual` exists because the bootloader mapped all of
  physical memory at a known offset — that offset is the single most
  load-bearing number in this file.
- **Read the next pointer before you hand the frame over.** The caller is about
  to write all over it, including the eight bytes holding your list.
- **A frame you free must not still be mapped anywhere.** That is why
  `deallocate` is `unsafe`: nothing here can check it for you.

## Verify

```bash
./run.sh
5am> selftest memory
5am> heap
```

`selftest memory` allocates a frame, maps it, writes to it, unmaps it and checks
the count came back. `heap` shows the free-frame count directly.

## Going further

The free list has no idea whether two frames are adjacent, so it cannot hand out
16 KiB that is contiguous — which is what a DMA-capable device will one day
need. Look up the buddy allocator and think about what it costs to get that.
