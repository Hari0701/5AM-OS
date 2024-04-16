//! The allocator, and therefore `Vec`, `String` and `Box`.
//!
//! Every buffer in this kernel up to now has been a fixed-size array decided at
//! compile time — `[f32; MAX_VOCAB]`, `[u8; 128]`, the 8MB of static scratch the
//! transformer scribbles on. That was not a stylistic choice. Rust's `alloc`
//! crate needs somewhere to put things, and until a kernel can answer "give me
//! N bytes" there is no such place.
//!
//! Answering it takes two steps, and only the second is the allocator:
//!
//!   1. Claim a range of *virtual* addresses and map real frames behind every
//!      page of it. That is [`init`], and it uses `memory.rs`.
//!   2. Hand out pieces of that range on request. That is [`LinkedListAllocator`].
//!
//! The design is a free list of holes: `alloc` finds one big enough and splits
//! it, `dealloc` pushes the freed range back onto the list.
//!
//! It is emphatically not fast. Allocation is O(number of holes) and there is no
//! size binning, no per-CPU cache, and no locking beyond disabling interrupts.
//! A real kernel allocator (slab, buddy, magazine) is a different project.

use crate::interrupts::without_interrupts;
use crate::memory::{self, FLAG_WRITABLE, PAGE_SIZE};
use core::alloc::{GlobalAlloc, Layout};
use core::ptr;

/// Where the heap lives in virtual memory.
///
/// The value is arbitrary — it just has to be an address range nothing else
/// uses. Picking something visually distinctive makes a stray pointer obvious
/// in a fault report: an address starting 0x4444 is unmistakably heap.
pub const HEAP_START: u64 = 0x_4444_4444_0000;
pub const HEAP_SIZE: usize = 2 * 1024 * 1024;

/// A free region, with the list threaded through the free memory itself.
struct Hole {
    size: usize,
    next: *mut Hole,
}

impl Hole {
    fn start(&self) -> usize {
        self as *const Self as usize
    }
    fn end(&self) -> usize {
        self.start() + self.size
    }
}

pub struct LinkedListAllocator {
    head: *mut Hole,
    allocated: usize,
    live_allocations: usize,
}

unsafe impl Send for LinkedListAllocator {}

impl LinkedListAllocator {
    pub const fn new() -> Self {
        Self {
            head: core::ptr::null_mut(),
            allocated: 0,
            live_allocations: 0,
        }
    }

    /// Return a region to the free list.
    ///
    /// # Safety
    /// The range must be mapped, writable, and not already free.
    unsafe fn add_region(&mut self, address: usize, size: usize) {
        let hole = address as *mut Hole;
        unsafe {
            (*hole).size = size;
            (*hole).next = self.head;
        }
        self.head = hole;
    }

    /// Find a hole big enough, split it, and unlink what we took.
    fn take_region(&mut self, size: usize, align: usize) -> Option<(usize, usize)> {
        let mut previous: *mut Hole = core::ptr::null_mut();
        let mut current = self.head;

        while !current.is_null() {
            if let Ok(start) = unsafe { Self::fits(&*current, size, align) } {
                let (region_start, region_end) =
                    unsafe { ((*current).start(), (*current).end()) };
                let next = unsafe { (*current).next };

                if previous.is_null() {
                    self.head = next;
                } else {
                    unsafe { (*previous).next = next };
                }

                if start > region_start {
                    unsafe { self.add_region(region_start, start - region_start) };
                }
                let end = start + size;
                if region_end > end {
                    unsafe { self.add_region(end, region_end - end) };
                }
                return Some((start, size));
            }
            previous = current;
            current = unsafe { (*current).next };
        }
        None
    }

    /// Can this allocation sit in this hole, once aligned?
    fn fits(hole: &Hole, size: usize, align: usize) -> Result<usize, ()> {
        let start = align_up(hole.start(), align);
        let end = start.checked_add(size).ok_or(())?;
        if end > hole.end() {
            return Err(());
        }
        // Any remainder must be big enough to hold a Hole, or it becomes
        // unreclaimable: there would be nowhere to store its own list node.
        let front = start - hole.start();
        if front > 0 && front < size_of::<Hole>() {
            return Err(());
        }
        let back = hole.end() - end;
        if back > 0 && back < size_of::<Hole>() {
            return Err(());
        }
        Ok(start)
    }

    fn stats(&self) -> (usize, usize, usize) {
        let mut free = 0;
        let mut holes = 0;
        let mut current = self.head;
        while !current.is_null() {
            free += unsafe { (*current).size };
            holes += 1;
            current = unsafe { (*current).next };
        }
        (free, holes, self.live_allocations)
    }
}

/// Wrapper that makes the allocator usable as Rust's global one.
///
/// Interrupts are disabled around every operation rather than taken with a
/// lock. With one CPU and no preemption that is sufficient and cheaper — but it
/// is exactly the assumption that would break first on a second core.
pub struct Locked(());

#[global_allocator]
static ALLOCATOR: Locked = Locked(());

static mut INNER: LinkedListAllocator = LinkedListAllocator::new();

fn inner() -> &'static mut LinkedListAllocator {
    unsafe { &mut *ptr::addr_of_mut!(INNER) }
}

unsafe impl GlobalAlloc for Locked {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let (size, align) = adjust(layout);
        without_interrupts(|| {
            let allocator = inner();
            match allocator.take_region(size, align) {
                Some((start, size)) => {
                    allocator.allocated += size;
                    allocator.live_allocations += 1;
                    start as *mut u8
                }
                // Returning null is how a GlobalAlloc reports failure; Rust
                // turns that into a call to handle_alloc_error.
                None => ptr::null_mut(),
            }
        })
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        let (size, _) = adjust(layout);
        without_interrupts(|| {
            let allocator = inner();
            unsafe { allocator.add_region(ptr as usize, size) };
            allocator.allocated -= size;
            allocator.live_allocations -= 1;
        })
    }
}

/// Every allocation must be able to hold a `Hole` when it is freed, since the
/// free list lives inside freed memory.
fn adjust(layout: Layout) -> (usize, usize) {
    let layout = layout
        .align_to(align_of::<Hole>())
        .expect("alignment overflow")
        .pad_to_align();
    (layout.size().max(size_of::<Hole>()), layout.align())
}

fn align_up(address: usize, align: usize) -> usize {
    (address + align - 1) & !(align - 1)
}

/// Map the heap's pages and hand the range to the allocator.
///
/// # Safety
/// Run once, after the frame allocator is initialised.
pub unsafe fn init() -> Result<(), &'static str> {
    let pages = HEAP_SIZE / PAGE_SIZE;
    for page in 0..pages {
        let frame = memory::allocator()
            .allocate()
            .ok_or("not enough physical memory for the heap")?;
        let address = HEAP_START + (page * PAGE_SIZE) as u64;
        unsafe { memory::map_page(address, frame, FLAG_WRITABLE)? };
    }
    unsafe {
        inner().add_region(HEAP_START as usize, HEAP_SIZE);
    }
    Ok(())
}

/// (free bytes, holes, live allocations)
pub fn stats() -> (usize, usize, usize) {
    without_interrupts(|| inner().stats())
}
