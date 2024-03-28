//! Physical memory: who owns which 4KiB frame, and how to map one.
//!
//! Everything before this module treated memory as something that was simply
//! *there* — the bootloader had mapped what we needed and we used it. This is
//! where the kernel starts managing memory instead of inheriting it.
//!
//! This half is **allocation**: deciding which physical frames are free. That
//! is pure bookkeeping. Mapping -- telling the CPU that a virtual address
//! resolves to one of those frames -- is a separate problem, and comes next.
//!
//! ## Reaching the page tables at all
//!
//! There is a bootstrapping problem here that took a while to see. Page table
//! entries hold *physical* addresses, but every address the kernel uses is
//! *virtual* — so to read the table at the physical address in CR3, we need a
//! virtual address that maps to it. We cannot make one without first reading
//! the table.
//!
//! The escape is to have the bootloader map all of physical memory at a known
//! offset before the kernel starts (see `BOOTLOADER_CONFIG` in main.rs). Then
//! physical address P is always readable at virtual address P + offset, and the
//! walk becomes possible. That offset is the single most load-bearing number in
//! this file.

use crate::println;
use bootloader_api::info::{MemoryRegionKind, MemoryRegions};

/// x86_64 pages are 4KiB. Everything here is in units of that.
pub const PAGE_SIZE: usize = 4096;

/// Where the bootloader mapped all of physical memory.
static mut PHYSICAL_OFFSET: u64 = 0;

/// Turn a physical address into one the kernel can actually dereference.
pub fn physical_to_virtual(physical: u64) -> u64 {
    physical + unsafe { core::ptr::read_volatile(core::ptr::addr_of!(PHYSICAL_OFFSET)) }
}

// --- the frame allocator -------------------------------------------------

/// A free-list of physical frames, built from the firmware's memory map.
///
/// The list is stored *inside the free frames themselves*: each free frame's
/// first eight bytes hold the physical address of the next one. This costs no
/// separate bookkeeping memory at all — which matters, because at this point in
/// boot there is nowhere to put bookkeeping memory. It is also why the frames
/// have to be mapped (via the physical offset) before they can be linked.
pub struct FrameAllocator {
    /// Physical address of the first free frame, or 0 when exhausted.
    head: u64,
    free: usize,
    total: usize,
}

static mut ALLOCATOR: FrameAllocator = FrameAllocator {
    head: 0,
    free: 0,
    total: 0,
};

pub fn allocator() -> &'static mut FrameAllocator {
    unsafe { &mut *core::ptr::addr_of_mut!(ALLOCATOR) }
}

impl FrameAllocator {
    /// Take one frame. Returns its physical address.
    pub fn allocate(&mut self) -> Option<u64> {
        if self.head == 0 {
            return None;
        }
        let frame = self.head;
        // The next pointer lives in the frame we are handing out, so read it
        // before the caller scribbles over it.
        self.head = unsafe { *(physical_to_virtual(frame) as *const u64) };
        self.free -= 1;
        Some(frame)
    }

    /// Give a frame back.
    ///
    /// # Safety
    /// The frame must not be mapped anywhere or referenced by any page table.
    pub unsafe fn deallocate(&mut self, frame: u64) {
        unsafe {
            *(physical_to_virtual(frame) as *mut u64) = self.head;
        }
        self.head = frame;
        self.free += 1;
    }

    pub fn stats(&self) -> (usize, usize) {
        (self.free, self.total)
    }
}

/// Build the free list from the regions the firmware called usable.
///
/// # Safety
/// Must run once, before anything else allocates, with `physical_offset`
/// matching what the bootloader actually mapped.
pub unsafe fn init(regions: &MemoryRegions, physical_offset: u64) {
    unsafe {
        PHYSICAL_OFFSET = physical_offset;
    }
    let allocator = allocator();

    for region in regions.iter() {
        if region.kind != MemoryRegionKind::Usable {
            continue;
        }
        // Round inward: a partial frame at either end is not a frame.
        let start = (region.start + PAGE_SIZE as u64 - 1) & !(PAGE_SIZE as u64 - 1);
        let end = region.end & !(PAGE_SIZE as u64 - 1);

        let mut frame = start;
        while frame + PAGE_SIZE as u64 <= end {
            // Skip the first megabyte. It is technically usable but full of
            // real-mode leftovers — the BIOS data area, the interrupt vector
            // table — and handing it out invites a class of bug that only
            // appears on real hardware, never in an emulator.
            if frame >= 0x10_0000 {
                unsafe { allocator.deallocate(frame) };
                allocator.total += 1;
            }
            frame += PAGE_SIZE as u64;
        }
    }
    // deallocate() counted these as frees; total was counted alongside.
    println!(
        "[mem ] {} usable frames ({} MiB) on the free list",
        allocator.total,
        allocator.total * PAGE_SIZE / 1024 / 1024
    );
}
