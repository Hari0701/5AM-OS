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
use alloc::vec::Vec;
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
/// Ring 3 may touch this page. Cleared, the page is kernel-only.
const USER: u64 = 1 << 2;
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
    unsafe { map_page_in(read_cr3(), virtual_address, frame, flags) }
}

/// As `map_page`, but into a page table tree that is not the active one.
///
/// This is what an address space *is*, mechanically: the same walk, rooted
/// somewhere else. Everything about mapping is unchanged -- only the first
/// table read differs.
///
/// # Safety
/// As `map_page`, and `root` must be a valid level-4 table.
pub unsafe fn map_page_in(
    root: u64,
    virtual_address: u64,
    frame: u64,
    flags: u64,
) -> Result<(), &'static str> {
    let indexes = indexes(virtual_address);
    let mut table = root;

    // The user bit is checked at *every* level, and the CPU takes the strictest
    // answer it finds. Setting it only on the final entry is the classic way to
    // spend an afternoon: the page says ring 3 may read it, some table above it
    // says otherwise, and the access faults with everything apparently correct.
    // So it has to travel down the walk with us.
    let user = flags & USER;

    // Walk levels 4, 3 and 2, creating tables where the path does not exist.
    for level in 0..3 {
        let entry_ptr = (physical_to_virtual(table) as *mut u64).add(indexes[level]);
        let entry = unsafe { *entry_ptr };

        table = if entry & PRESENT != 0 {
            // An existing table on the path may have been built for the kernel.
            // Widening it is safe -- the leaf entry still decides -- and it is
            // the only way to hang a user page under a kernel branch.
            if user != 0 && entry & USER == 0 {
                unsafe { *entry_ptr = entry | USER };
            }
            entry & ADDRESS_MASK
        } else {
            let new = allocator().allocate().ok_or("out of physical memory")?;
            // A fresh table must be zeroed, or its garbage reads as present
            // entries pointing at arbitrary physical addresses.
            unsafe {
                core::ptr::write_bytes(physical_to_virtual(new) as *mut u8, 0, PAGE_SIZE);
                *entry_ptr = new | PRESENT | WRITABLE | user;
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
    translate_in(read_cr3(), virtual_address)
}

/// As `translate`, rooted at a table you name.
pub fn translate_in(root: u64, virtual_address: u64) -> Option<(u64, [u64; 4])> {
    let indexes = indexes(virtual_address);
    let mut table = root;
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

/// Undo a mapping and hand back the frame it pointed at.
///
/// The counterpart `map_page` never had, which is why every `exec` used to cost
/// the machine a few pages permanently. A kernel that can only map is a kernel
/// with a memory leak measured in programs run.
///
/// Only the last-level entry is cleared. The tables above it are left in place
/// even when they empty out -- reclaiming those means knowing whether any of
/// their 512 entries is still live, and getting that wrong frees a table
/// something else is still walking. Real kernels keep a count per table; this
/// one keeps the four pages.
///
/// # Safety
/// The address must not be in use. Nothing here checks whether a task is still
/// standing on it.
pub unsafe fn unmap_page(virtual_address: u64) -> Option<u64> {
    let indexes = indexes(virtual_address);

    // Remember the whole path down, because pruning is a walk back up it.
    let mut tables = [0u64; 4];
    let mut table = read_cr3();
    for level in 0..4 {
        tables[level] = table;
        let entry = unsafe { *(physical_to_virtual(table) as *const u64).add(indexes[level]) };
        if entry & PRESENT == 0 {
            return None;
        }
        if level < 3 {
            table = entry & ADDRESS_MASK;
        }
    }

    let leaf_ptr = (physical_to_virtual(tables[3]) as *mut u64).add(indexes[3]);
    let entry = unsafe { *leaf_ptr };
    if entry & PRESENT == 0 {
        return None;
    }

    unsafe {
        *leaf_ptr = 0;
        // Without this the CPU keeps serving the old translation out of the
        // TLB, and the page stays readable at the old address after the frame
        // belongs to somebody else -- which presents as memory corruption in an
        // unrelated subsystem.
        asm!("invlpg [{}]", in(reg) virtual_address, options(nostack, preserves_flags));
    }

    // Now give back the tables that this was the last entry in.
    //
    // The first version of this did not, and said so in a comment: reclaiming a
    // table means proving all 512 of its entries are dead, and freeing one that
    // something else is still walking is catastrophic. The proof turns out to be
    // cheap -- read 512 words and check they are zero -- and the emptiness test
    // is what makes it safe in general. A table the bootloader also has
    // mappings in is, by definition, not empty, so it is never touched.
    //
    // Deepest first, and stop at the first table that still has something in
    // it. The top-level table is never freed: it is what CR3 points at.
    for level in (1..4).rev() {
        if !table_is_empty(tables[level]) {
            break;
        }
        let parent = (physical_to_virtual(tables[level - 1]) as *mut u64).add(indexes[level - 1]);
        unsafe {
            *parent = 0;
            allocator().deallocate(tables[level]);
            asm!("invlpg [{}]", in(reg) virtual_address, options(nostack, preserves_flags));
        }
    }

    Some(entry & ADDRESS_MASK)
}

/// Does this table have no present entries left?
fn table_is_empty(table: u64) -> bool {
    let base = physical_to_virtual(table) as *const u64;
    for index in 0..512 {
        if unsafe { *base.add(index) } & PRESENT != 0 {
            return false;
        }
    }
    true
}

/// Unmap a range and return every frame to the allocator.
///
/// # Safety
/// As `unmap_page`, for every page in the range.
pub unsafe fn release(pages: &[u64]) -> usize {
    let mut freed = 0;
    for &page in pages {
        if let Some(frame) = unsafe { unmap_page(page) } {
            unsafe { allocator().deallocate(frame) };
            freed += 1;
        }
    }
    freed
}

/// Which top-level slots are in use, and what address range each one covers.
///
/// The level-4 table has 512 entries and each one owns 512 GiB of address
/// space. Printing which are present is the clearest picture of the address
/// space a kernel can give you -- and it is the measurement that decides
/// whether per-process address spaces are cheap or hard, because a slot the
/// kernel and userspace both need cannot simply be swapped per process.
pub fn top_level_map() -> [(usize, u64, u64); 512] {
    let mut out = [(0usize, 0u64, 0u64); 512];
    let table = read_cr3();
    for index in 0..512 {
        let entry = unsafe { *(physical_to_virtual(table) as *const u64).add(index) };
        out[index] = (index, entry, (index as u64) << 39);
    }
    out
}

pub const FLAG_WRITABLE: u64 = WRITABLE;
pub const FLAG_USER: u64 = USER;

/// Change the permissions on a page that is already mapped, keeping its frame.
///
/// This is how a loader seals what it has just written: map a page writable,
/// copy code into it, then take the write permission away. After that even the
/// kernel cannot modify it -- CR0.WP means ring 0 obeys the read-only bit too,
/// which is precisely why that bit gets set.
///
/// # Safety
/// Editing a live mapping. Removing a permission something still relies on
/// surfaces as a fault at a completely unrelated instruction.
pub unsafe fn set_flags(virtual_address: u64, flags: u64) -> Result<(), &'static str> {
    let indexes = indexes(virtual_address);
    let mut table = read_cr3();

    for level in 0..3 {
        let entry = unsafe { *(physical_to_virtual(table) as *const u64).add(indexes[level]) };
        if entry & PRESENT == 0 {
            return Err("not mapped");
        }
        table = entry & ADDRESS_MASK;
    }

    let entry_ptr = (physical_to_virtual(table) as *mut u64).add(indexes[3]);
    let entry = unsafe { *entry_ptr };
    if entry & PRESENT == 0 {
        return Err("not mapped");
    }
    unsafe {
        *entry_ptr = (entry & ADDRESS_MASK) | flags | PRESENT;
        asm!("invlpg [{}]", in(reg) virtual_address, options(nostack, preserves_flags));
    }
    Ok(())
}

/// Is this address one ring 3 is allowed to touch?
///
/// The kernel asks this about every pointer a syscall hands it. A user program
/// that passes a kernel address is not necessarily malicious -- it is more often
/// simply wrong -- but a kernel that dereferences it either way has no isolation
/// at all, only the appearance of some.
pub fn is_user_accessible(address: u64, len: u64) -> bool {
    let Some(end) = address.checked_add(len) else {
        return false;
    };
    let mut page = address & !(PAGE_SIZE as u64 - 1);
    while page < end {
        let indexes = indexes(page);
        let mut table = read_cr3();
        for level in 0..4 {
            let entry = unsafe { *(physical_to_virtual(table) as *const u64).add(indexes[level]) };
            if entry & PRESENT == 0 || entry & USER == 0 {
                return false;
            }
            if level < 3 && entry & (1 << 7) != 0 {
                break;
            }
            table = entry & ADDRESS_MASK;
        }
        page += PAGE_SIZE as u64;
    }
    true
}

// --- no-execute ----------------------------------------------------------

/// Bit 63 of a page table entry: fetching an instruction from this page faults.
///
/// The bit exists in every long-mode entry, and the CPU ignores it completely
/// until `EFER.NXE` is set. That is why a kernel can mark pages non-executable,
/// see no faults, and believe it worked -- the permission was written down and
/// never enforced. This kernel spent its whole ELF-loader life in exactly that
/// state, printing `rw-` for a segment that was fully executable.
const NO_EXECUTE: u64 = 1 << 63;

pub const FLAG_NO_EXECUTE: u64 = NO_EXECUTE;

/// Enable the no-execute bit, if this CPU has it.
///
/// EFER is a Model Specific Register: a control register reached by number
/// through `rdmsr`/`wrmsr` rather than by name. NXE is bit 11.
///
/// Support is not guaranteed, so it is asked for rather than assumed --
/// CPUID leaf 0x8000_0001 reports it in EDX bit 20. Setting NXE on a CPU
/// without it raises #GP, which would be a poor way to end the boot.
///
/// # Safety
/// Changes how every page table entry in the system is interpreted. Must run
/// before any entry sets bit 63.
pub unsafe fn enable_no_execute() -> bool {
    let supported: u32;
    unsafe {
        asm!(
            "push rbx",
            "mov eax, 0x80000001",
            "cpuid",
            "pop rbx",
            out("eax") _,
            out("edx") supported,
            out("ecx") _,
            options(nostack),
        );
    }
    if supported & (1 << 20) == 0 {
        return false;
    }

    const EFER: u32 = 0xC000_0080;
    unsafe {
        let (low, high): (u32, u32);
        asm!("rdmsr", in("ecx") EFER, out("eax") low, out("edx") high, options(nostack, preserves_flags));
        let value = ((high as u64) << 32 | low as u64) | (1 << 11); // NXE
        asm!(
            "wrmsr",
            in("ecx") EFER,
            in("eax") value as u32,
            in("edx") (value >> 32) as u32,
            options(nostack, preserves_flags),
        );
    }
    true
}

/// Whether `enable_no_execute` succeeded, so callers can avoid setting a bit
/// the CPU would treat as part of an address.
static mut NO_EXECUTE_ACTIVE: bool = false;

pub fn no_execute_active() -> bool {
    unsafe { core::ptr::read_volatile(core::ptr::addr_of!(NO_EXECUTE_ACTIVE)) }
}

/// # Safety
/// Call once, from `enable_no_execute`'s caller, with the value it returned.
pub unsafe fn set_no_execute_active(active: bool) {
    unsafe { core::ptr::write_volatile(core::ptr::addr_of_mut!(NO_EXECUTE_ACTIVE), active) };
}

// --- address spaces ------------------------------------------------------

/// Where userspace ends. Everything below this is per-process; everything
/// above belongs to the kernel and is shared by every address space.
pub const USER_SPACE_END: u64 = 1 << 39;

/// A private view of memory.
///
/// Up to now every task shared one set of page tables, so "ring 3" fenced a
/// program off from the *kernel* and not from other programs. Two user programs
/// could read each other's memory completely. That is the difference between a
/// task and a process, and this is the whole of it: a different level-4 table.
///
/// ## What has to be shared anyway
///
/// The kernel must stay mapped in every address space, at the same addresses.
/// An interrupt can arrive at any moment, and the handler is kernel code that
/// runs on whatever address space happened to be active — if the kernel were
/// not mapped there, the first timer tick after a switch would be a triple
/// fault with nothing printed.
///
/// On this machine that is easy, and `pagemap` is how I checked rather than
/// assumed: the kernel occupies top-level slots 2 through 7 and 136, and
/// userspace lives entirely in slot 0. Copying every slot except 0 gives a
/// process a private lower half and a shared kernel, which is the same
/// arrangement Linux uses — for the same reason, and at the same granularity.
pub struct AddressSpace {
    root: u64,
}

impl AddressSpace {
    /// Build a new address space: empty user half, kernel half shared.
    pub fn new() -> Option<Self> {
        let root = allocator().allocate()?;
        unsafe {
            core::ptr::write_bytes(physical_to_virtual(root) as *mut u8, 0, PAGE_SIZE);

            // Share every kernel slot by copying the *entry*, not the table.
            // Both address spaces then point at the same lower tables, so a
            // change the kernel makes to its own mappings is visible in every
            // process without any synchronisation at all.
            let current = physical_to_virtual(read_cr3()) as *const u64;
            let new = physical_to_virtual(root) as *mut u64;
            for slot in 1..512 {
                *new.add(slot) = *current.add(slot);
            }
        }
        Some(Self { root })
    }

    /// Wrap a level-4 table this code did not allocate, without taking
    /// ownership of it. Used to fork the address space that is already active.
    pub fn adopt(root: u64) -> Self {
        Self { root }
    }

    pub fn root(&self) -> u64 {
        self.root
    }

    /// Make this the address space the CPU is using.
    ///
    /// # Safety
    /// Every address the current code depends on must be mapped here too --
    /// which for kernel code it is, by construction. Writing CR3 also flushes
    /// the entire TLB, which is why switching processes is not free.
    pub unsafe fn activate(&self) {
        unsafe { asm!("mov cr3, {}", in(reg) self.root, options(nostack, preserves_flags)) };
    }

    /// Map a page into this space rather than the live one.
    ///
    /// # Safety
    /// As `map_page`.
    pub unsafe fn map(&self, virtual_address: u64, frame: u64, flags: u64) -> Result<(), &'static str> {
        if virtual_address >= USER_SPACE_END {
            return Err("that address belongs to the kernel");
        }
        unsafe { map_page_in(self.root, virtual_address, frame, flags) }
    }

    pub fn translate(&self, virtual_address: u64) -> Option<(u64, [u64; 4])> {
        translate_in(self.root, virtual_address)
    }

    /// Tear the user half down and return every frame, including the tables.
    ///
    /// Only slot 0 is walked. Touching any other slot would free memory the
    /// kernel and every other process are still using -- the shared entries are
    /// copies, and freeing what they point at is exactly the bug that makes
    /// sharing dangerous.
    ///
    /// # Safety
    /// This space must not be active, and nothing may still be using it.
    pub unsafe fn destroy(self) -> usize {
        let mut freed = 0;
        unsafe {
            let root_table = physical_to_virtual(self.root) as *mut u64;
            let slot0 = *root_table;
            if slot0 & PRESENT != 0 {
                freed += free_subtree(slot0 & ADDRESS_MASK, 3);
                *root_table = 0;
            }
            allocator().deallocate(self.root);
        }
        freed + 1
    }
}

/// Free a table and everything under it.
///
/// `level` is the level of *this* table: 3 for a PDPT, 2 for a page directory,
/// 1 for a page table. Only a level-1 table's entries name data frames;
/// everything above names another table.
///
/// Getting that off by one is not a subtle failure. The first version recursed
/// one level too far, treated 4 KiB of user data as 512 page table entries, and
/// handed the resulting nonsense to the frame allocator -- which faulted trying
/// to write a free-list pointer to a non-canonical address. The oracle called
/// it correctly and the disassembly named the store, but the mistake was three
/// frames up the stack from where it landed.
///
/// # Safety
/// Nothing may reference any of it.
unsafe fn free_subtree(table: u64, level: usize) -> usize {
    let mut freed = 0;
    let base = physical_to_virtual(table) as *mut u64;
    for index in 0..512 {
        let entry = unsafe { *base.add(index) };
        if entry & PRESENT == 0 {
            continue;
        }
        // A huge page names data directly even above level 1.
        let huge = level > 1 && entry & (1 << 7) != 0;
        if level == 1 || huge {
            let frame = entry & ADDRESS_MASK;
            // A shared frame belongs to somebody else too. Dropping our
            // reference is all we may do -- freeing it would hand a page
            // another address space is still reading to the next allocation.
            //
            // Note the test is "was it shared", not "is it still shared after
            // dropping". Unsharing down to one owner means the *other* space
            // now has it exclusively, which is precisely when freeing it here
            // would be a disaster.
            if share_count(frame) > 1 {
                unshare(frame);
                continue;
            }
            unsafe { allocator().deallocate(frame) };
            freed += 1;
        } else {
            freed += unsafe { free_subtree(entry & ADDRESS_MASK, level - 1) };
        }
    }
    unsafe { allocator().deallocate(table) };
    freed + 1
}

/// The kernel's own level-4 table: the one the bootloader handed over.
static mut KERNEL_ROOT: u64 = 0;

/// Remember the address space the kernel started in.
///
/// Tasks that have no address space of their own must run in *this* one, not in
/// whichever user space happened to be active when they were scheduled. Letting
/// them inherit was fine for execution -- the kernel is mapped everywhere -- but
/// it made `active_root()` mean "wherever we happen to be", and the shell then
/// went on to destroy an address space it was standing in.
pub fn remember_kernel_root() {
    unsafe { core::ptr::write_volatile(core::ptr::addr_of_mut!(KERNEL_ROOT), read_cr3()) };
}

pub fn kernel_root() -> u64 {
    unsafe { core::ptr::read_volatile(core::ptr::addr_of!(KERNEL_ROOT)) }
}

/// The address space the CPU is using right now.
pub fn active_root() -> u64 {
    read_cr3()
}

/// # Safety
/// `root` must be a level-4 table in which the currently executing code is
/// mapped.
pub unsafe fn activate_root(root: u64) {
    unsafe { asm!("mov cr3, {}", in(reg) root, options(nostack, preserves_flags)) };
}

// --- copy on write -------------------------------------------------------

/// Bit 9 is one of three bits the CPU ignores entirely and leaves to the OS.
///
/// This one means "read-only because it is shared, not because it is meant to
/// be read-only". Without a distinction like it, the fault handler cannot tell
/// a page it should silently copy from a page a program has no business writing
/// to — and would happily make `.text` writable on demand.
const COW: u64 = 1 << 9;

/// Frames referenced by more than one address space, and by how many.
///
/// Only shared frames appear here, which keeps it tiny: a forked program shares
/// a dozen pages, not a million. A production kernel keeps a counter per frame
/// in a flat array, because it shares at a very different scale.
static mut SHARED: Option<Vec<(u64, u32)>> = None;

fn shared() -> &'static mut Vec<(u64, u32)> {
    unsafe {
        let slot = &mut *core::ptr::addr_of_mut!(SHARED);
        slot.get_or_insert_with(Vec::new)
    }
}

/// Record one more owner of `frame`.
fn share(frame: u64) {
    let table = shared();
    match table.iter_mut().find(|(f, _)| *f == frame) {
        // Already shared: one more owner.
        Some((_, count)) => *count += 1,
        // First share: two owners, the one that had it and the new one.
        None => table.push((frame, 2)),
    }
}

/// Drop one owner. Returns how many are left.
fn unshare(frame: u64) -> u32 {
    let table = shared();
    if let Some(index) = table.iter().position(|(f, _)| *f == frame) {
        table[index].1 -= 1;
        let left = table[index].1;
        if left <= 1 {
            table.swap_remove(index);
        }
        return left;
    }
    1
}

fn share_count(frame: u64) -> u32 {
    shared()
        .iter()
        .find(|(f, _)| *f == frame)
        .map(|(_, c)| *c)
        .unwrap_or(1)
}

/// Every present page in the user half of `root`, with a pointer to its entry.
fn user_pages(root: u64) -> Vec<(u64, u64, *mut u64)> {
    let mut out = Vec::new();
    let level4 = physical_to_virtual(root) as *mut u64;
    let slot0 = unsafe { *level4 };
    if slot0 & PRESENT == 0 {
        return out;
    }

    let pdpt = physical_to_virtual(slot0 & ADDRESS_MASK) as *mut u64;
    for i3 in 0..512 {
        let e3 = unsafe { *pdpt.add(i3) };
        if e3 & PRESENT == 0 || e3 & (1 << 7) != 0 {
            continue;
        }
        let directory = physical_to_virtual(e3 & ADDRESS_MASK) as *mut u64;
        for i2 in 0..512 {
            let e2 = unsafe { *directory.add(i2) };
            if e2 & PRESENT == 0 || e2 & (1 << 7) != 0 {
                continue;
            }
            let table = physical_to_virtual(e2 & ADDRESS_MASK) as *mut u64;
            for i1 in 0..512 {
                let pointer = unsafe { table.add(i1) };
                let entry = unsafe { *pointer };
                if entry & PRESENT == 0 {
                    continue;
                }
                let address = ((i3 as u64) << 30) | ((i2 as u64) << 21) | ((i1 as u64) << 12);
                out.push((address, entry, pointer));
            }
        }
    }
    out
}

impl AddressSpace {
    /// Duplicate this address space without duplicating its memory.
    ///
    /// The obvious implementation copies every page, and the obvious
    /// implementation is what makes `fork` expensive enough that people avoid
    /// it. Almost every forked program immediately replaces itself with another
    /// one, so nearly all of that copying is thrown away unread.
    ///
    /// So copy nothing. Point both address spaces at the same frames, take the
    /// write permission away from **both**, and mark the entries copy-on-write.
    /// Whoever writes first takes a fault, gets a private copy, and neither
    /// program can tell the difference. Memory is only spent on pages that are
    /// actually modified.
    ///
    /// The tables themselves *are* copied. Sharing those instead would be
    /// possible and is what makes a real implementation faster still, but then
    /// the first write has to un-share a whole table before it can un-share a
    /// page, and that is a second mechanism to get right.
    ///
    /// # Safety
    /// `self` must be a valid address space; the caller owns the result.
    pub unsafe fn fork(&self) -> Option<AddressSpace> {
        let child = AddressSpace::new()?;

        for (address, entry, pointer) in user_pages(self.root) {
            let frame = entry & ADDRESS_MASK;
            let flags = entry & !ADDRESS_MASK;

            // A page that was writable becomes read-only and copy-on-write in
            // both. One that was already read-only stays exactly as it is --
            // sharing immutable pages needs no mechanism at all, and marking
            // `.text` copy-on-write would mean silently permitting a write to
            // it later.
            let child_flags = if flags & WRITABLE != 0 {
                let shared_flags = (flags & !WRITABLE) | COW;
                unsafe {
                    *pointer = frame | shared_flags;
                    asm!("invlpg [{}]", in(reg) address, options(nostack, preserves_flags));
                }
                shared_flags
            } else {
                flags
            };

            if unsafe { map_page_in(child.root, address, frame, child_flags & !PRESENT) }.is_err() {
                return None;
            }
            share(frame);
        }

        Some(child)
    }
}

/// Handle a write to a copy-on-write page. Returns false if this fault is not
/// one of ours, in which case it is a real fault and the caller should say so.
///
/// # Safety
/// Called from the page fault handler with the faulting address from CR2.
pub unsafe fn cow_fault(address: u64) -> bool {
    let page = address & !(PAGE_SIZE as u64 - 1);
    if page >= USER_SPACE_END {
        return false;
    }

    let root = read_cr3();
    let indexes = indexes(page);
    let mut table = root;
    for level in 0..3 {
        let entry = unsafe { *(physical_to_virtual(table) as *const u64).add(indexes[level]) };
        if entry & PRESENT == 0 || entry & (1 << 7) != 0 {
            return false;
        }
        table = entry & ADDRESS_MASK;
    }
    let pointer = (physical_to_virtual(table) as *mut u64).add(indexes[3]);
    let entry = unsafe { *pointer };

    // Only a page we marked. A read-only page without the COW bit is one the
    // program genuinely may not write, and turning this into "make it writable"
    // would quietly grant that.
    if entry & PRESENT == 0 || entry & COW == 0 || entry & WRITABLE != 0 {
        return false;
    }

    let frame = entry & ADDRESS_MASK;

    // Ask how many owners there are *before* dropping ours. Decrementing first
    // and then testing was my first version, and it gets the two-owner case --
    // the only case fork ever produces -- exactly backwards: two owners minus
    // one reads as "sole owner", so the faulting process keeps the shared page
    // and simply grants itself write access to memory the other one is still
    // using. Both then free it, which is a double free on top of the sharing
    // bug. It passes every test that does not compare physical frames.
    let owners = share_count(frame);

    if owners > 1 {
        // Somebody else still has it: take a private copy and stop being an
        // owner of the original.
        unshare(frame);
        let Some(fresh) = allocator().allocate() else {
            return false;
        };
        unsafe {
            core::ptr::copy_nonoverlapping(
                physical_to_virtual(frame) as *const u8,
                physical_to_virtual(fresh) as *mut u8,
                PAGE_SIZE,
            );
            *pointer = fresh | ((entry & !ADDRESS_MASK) | WRITABLE) & !COW;
        }
    } else {
        // Last owner. There is nothing to copy -- the page is already private,
        // it was only being kept read-only in case somebody else needed it.
        // Real kernels get this wrong and copy anyway, which is a page of
        // memory and a memcpy spent proving something to nobody.
        unsafe { *pointer = (entry | WRITABLE) & !COW };
    }

    unsafe { asm!("invlpg [{}]", in(reg) page, options(nostack, preserves_flags)) };
    true
}

/// Is this frame shared with another address space?
pub fn is_shared(frame: u64) -> bool {
    share_count(frame) > 1
}
