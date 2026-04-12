//! Swap: the other half of demand paging.
//!
//! Demand paging makes a page appear when it is touched. Swapping makes one
//! disappear when it is not — and together they mean the memory a program can
//! address stops being limited by the memory the machine has.
//!
//! ## Where it goes
//!
//! Past the end of the filesystem. `mkfs` builds an image larger than the
//! volume it describes, and everything after sector 32768 is unstructured
//! blocks the filesystem knows nothing about. That is what a swap partition is
//! and why it is a partition: swap has no names, no directories and no
//! ordering, so a filesystem would be pure overhead.
//!
//! ## Choosing the victim
//!
//! The hard part is not writing a page out, it is deciding which. The best
//! choice is the page that will be needed furthest in the future, which is
//! unknowable, so every real algorithm is a guess at "least recently used" from
//! the little the hardware records.
//!
//! What the hardware records is one bit. The CPU sets the *accessed* bit in a
//! page table entry whenever it translates through it, and never clears it. So:
//! sweep the pages in a circle, and at each one either clear that bit and move
//! on, or — if it is already clear — take the page. A page survives exactly as
//! long as it keeps being touched between two passes of the hand.
//!
//! That is the clock algorithm, and it is in every operating system, because
//! one bit of hardware support is all anybody ever got.

use crate::ata::{self, SECTOR_SIZE};
use crate::memory::PAGE_SIZE;

/// First sector past the filesystem. Must match `mkfs`.
const SWAP_START: u32 = 32768;
const SECTORS_PER_PAGE: u32 = (PAGE_SIZE / SECTOR_SIZE) as u32;
pub const SLOTS: usize = 2048;

/// One bit per slot. Sixty-four slots to a word, and nothing else -- a slot has
/// no owner, no name and no state beyond used or free, because the page table
/// entry that points at it is the only reference there will ever be.
static mut USED: [u64; SLOTS / 64] = [0; SLOTS / 64];
static mut EVICTIONS: u64 = 0;
static mut FAULTS_IN: u64 = 0;

fn used() -> &'static mut [u64; SLOTS / 64] {
    unsafe { &mut *core::ptr::addr_of_mut!(USED) }
}

pub fn allocate_slot() -> Option<usize> {
    crate::interrupts::without_interrupts(|| {
        let used = used();
        for (index, word) in used.iter_mut().enumerate() {
            if *word != u64::MAX {
                let bit = (!*word).trailing_zeros() as usize;
                *word |= 1 << bit;
                return Some(index * 64 + bit);
            }
        }
        None
    })
}

pub fn free_slot(slot: usize) {
    if slot >= SLOTS {
        return;
    }
    crate::interrupts::without_interrupts(|| {
        used()[slot / 64] &= !(1 << (slot % 64));
    })
}

fn slot_lba(slot: usize) -> u32 {
    SWAP_START + slot as u32 * SECTORS_PER_PAGE
}

/// Write a frame's contents to a slot.
///
/// # Safety
/// `frame` must be a valid physical frame.
pub unsafe fn write_out(slot: usize, frame: u64) -> Result<(), &'static str> {
    let source = crate::memory::physical_to_virtual(frame) as *const u8;
    let bytes = unsafe { core::slice::from_raw_parts(source, PAGE_SIZE) };
    ata::write(slot_lba(slot), bytes)?;
    unsafe { EVICTIONS += 1 };
    Ok(())
}

/// Read a slot back into a frame.
///
/// # Safety
/// `frame` must be a valid physical frame, and `slot` must hold a page.
pub unsafe fn read_in(slot: usize, frame: u64) -> Result<(), &'static str> {
    let destination = crate::memory::physical_to_virtual(frame) as *mut u8;
    let bytes = unsafe { core::slice::from_raw_parts_mut(destination, PAGE_SIZE) };
    ata::read(slot_lba(slot), bytes)?;
    unsafe { FAULTS_IN += 1 };
    Ok(())
}

pub fn stats() -> (usize, u64, u64) {
    let in_use = used().iter().map(|word| word.count_ones() as usize).sum();
    unsafe {
        (
            in_use,
            core::ptr::read_volatile(core::ptr::addr_of!(EVICTIONS)),
            core::ptr::read_volatile(core::ptr::addr_of!(FAULTS_IN)),
        )
    }
}

pub fn reset() {
    crate::interrupts::without_interrupts(|| {
        *used() = [0; SLOTS / 64];
    })
}
