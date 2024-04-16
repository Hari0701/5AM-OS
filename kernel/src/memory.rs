//! Physical memory: who owns which 4KiB frame, and how to map one.
//!
//! Everything before this module treated memory as something that was simply
//! *there* — the bootloader had mapped what we needed and we used it. This is
//! where the kernel starts managing memory instead of inheriting it.
//!
//! Two jobs, and they are commonly confused:
//!
//!   * **Allocation** is deciding which physical frames are free. That is
//!     bookkeeping, and it lives in [`FrameAllocator`].
//!   * **Mapping** is telling the CPU that a virtual address should resolve to
//!     a particular physical frame. That means editing the page tables, and it
//!     lives in [`map_page`].
//!
//! You need both, and they are independent: a frame can be allocated and
//! unmapped (memory you own but cannot reach), or mapped without being
//! allocated (which is how you corrupt something else's data).
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
use core::arch::asm;

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

// --- page tables ---------------------------------------------------------

const PRESENT: u64 = 1 << 0;
const WRITABLE: u64 = 1 << 1;
/// Bits 12..51 of an entry are the physical address of the next table.
const ADDRESS_MASK: u64 = 0x000f_ffff_ffff_f000;

fn read_cr3() -> u64 {
    let cr3: u64;
    unsafe { asm!("mov {}, cr3", out(reg) cr3, options(nomem, nostack, preserves_flags)) };
    cr3 & ADDRESS_MASK
}

/// Split a virtual address into the four table indexes the CPU uses.
///
/// This is the whole of address translation, in one function: nine bits per
/// level, four levels, then a twelve-bit offset into the frame.
fn indexes(address: u64) -> [usize; 4] {
    [
        ((address >> 39) & 0x1FF) as usize, // level 4
        ((address >> 30) & 0x1FF) as usize, // level 3
        ((address >> 21) & 0x1FF) as usize, // level 2
        ((address >> 12) & 0x1FF) as usize, // level 1
    ]
}

/// Map `virtual_address` to `frame`, creating any missing tables on the way.
///
/// # Safety
/// Editing live page tables. A wrong entry here does not fault — it silently
/// points at someone else's memory.
pub unsafe fn map_page(virtual_address: u64, frame: u64, flags: u64) -> Result<(), &'static str> {
    let indexes = indexes(virtual_address);
    let mut table = read_cr3();

    // Walk levels 4, 3 and 2, creating tables where the path does not exist.
    for level in 0..3 {
        let entry_ptr = (physical_to_virtual(table) as *mut u64).add(indexes[level]);
        let entry = unsafe { *entry_ptr };

        table = if entry & PRESENT != 0 {
            entry & ADDRESS_MASK
        } else {
            let new = allocator().allocate().ok_or("out of physical memory")?;
            // A fresh table must be zeroed, or its garbage reads as present
            // entries pointing at arbitrary physical addresses.
            unsafe {
                core::ptr::write_bytes(physical_to_virtual(new) as *mut u8, 0, PAGE_SIZE);
                *entry_ptr = new | PRESENT | WRITABLE;
            }
            new
        };
    }

    // Level 1: the entry that actually names the frame.
    let entry_ptr = (physical_to_virtual(table) as *mut u64).add(indexes[3]);
    if unsafe { *entry_ptr } & PRESENT != 0 {
        return Err("that address is already mapped");
    }
    unsafe {
        *entry_ptr = (frame & ADDRESS_MASK) | flags | PRESENT;
    }

    // The CPU caches translations in the TLB and will happily keep using a
    // stale one. `invlpg` drops the entry for this page specifically.
    unsafe {
        asm!("invlpg [{}]", in(reg) virtual_address, options(nostack, preserves_flags));
    }
    Ok(())
}

/// Follow the tables by hand and report what a virtual address resolves to.
///
/// Used by `translate` in the shell — being able to *see* the walk is most of
/// the point of implementing it.
pub fn translate(virtual_address: u64) -> Option<(u64, [u64; 4])> {
    let indexes = indexes(virtual_address);
    let mut table = read_cr3();
    let mut entries = [0u64; 4];

    for level in 0..4 {
        let entry = unsafe { *(physical_to_virtual(table) as *const u64).add(indexes[level]) };
        entries[level] = entry;
        if entry & PRESENT == 0 {
            return None;
        }
        // A set bit 7 at level 3 or 2 means a huge page: the walk stops early
        // and the rest of the address is the offset.
        if level < 3 && entry & (1 << 7) != 0 {
            let size = if level == 1 { 1 << 30 } else { 1 << 21 };
            let base = entry & ADDRESS_MASK;
            return Some((base + (virtual_address & (size - 1)), entries));
        }
        table = entry & ADDRESS_MASK;
    }

    Some((table + (virtual_address & 0xFFF), entries))
}

pub const FLAG_WRITABLE: u64 = WRITABLE;
