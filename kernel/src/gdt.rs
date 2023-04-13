//! The Global Descriptor Table — the CPU's list of memory segments.
//!
//! The bootloader already built a GDT to get us into long mode. We build our
//! own anyway, for two reasons: theirs will be reclaimed as free memory once we
//! start allocating, and building it is how you learn what it is.
//!
//! In 64-bit mode segmentation is *mostly* dead — the base and limit fields are
//! ignored and every segment covers all of memory. What survives matters a lot
//! though: the privilege level, ring 0 versus ring 3.

use core::arch::asm;
use core::mem::size_of;

/// The value you load into a segment register: an index into the GDT plus the
/// requested privilege level in the low two bits.
pub const KERNEL_CODE: u16 = 1 << 3; // entry 1, ring 0
pub const KERNEL_DATA: u16 = 2 << 3; // entry 2, ring 0

/// The table itself: null, code, data.
static mut GDT: [u64; 3] = [0; 3];

/// What `lgdt` actually consumes: a limit and a base address.
#[repr(C, packed)]
struct DescriptorTablePointer {
    limit: u16,
    base: u64,
}

/// Build and load our GDT, then reload every segment register from it.
///
/// # Safety
/// Replaces the CPU's segment configuration. Must run exactly once, early,
/// with interrupts disabled.
pub unsafe fn init() {
    unsafe {
        let gdt = &mut *core::ptr::addr_of_mut!(GDT);

        // [0] The null descriptor. Required, and required to be zero: a segment
        //     register loaded with 0 is how you say "no segment".
        gdt[0] = 0;

        // [1] Kernel code. In long mode the CPU only reads three bits here:
        //     Present, Descriptor-type=code, and Long-mode.
        //       bit 43 executable, bit 44 descriptor type (1 = code/data),
        //       bit 47 present, bit 53 long mode
        gdt[1] = (1 << 43) | (1 << 44) | (1 << 47) | (1 << 53);

        // [2] Kernel data. Same idea, without the executable bit. Long mode
        //     ignores this for addressing, but SS must still reference a valid
        //     writable descriptor.
        gdt[2] = (1 << 44) | (1 << 47) | (1 << 41);

        // Tell the CPU where the table is.
        let pointer = DescriptorTablePointer {
            limit: (size_of::<[u64; 3]>() - 1) as u16,
            base: core::ptr::addr_of!(GDT) as u64,
        };
        asm!("lgdt [{}]", in(reg) &pointer, options(readonly, nostack, preserves_flags));

        // Loading the GDT does not change which descriptors are *in use*. CS
        // still holds the bootloader's selector, and CS cannot be assigned with
        // `mov` — the only way to change it is to jump. A far return does it:
        // push the new selector and a target address, then `retfq` pops both.
        asm!(
            "push {sel}",
            "lea {tmp}, [2f + rip]",
            "push {tmp}",
            "retfq",
            "2:",
            sel = in(reg) KERNEL_CODE as u64,
            tmp = lateout(reg) _,
            options(preserves_flags),
        );

        // The data segment registers are plain writes.
        asm!(
            "mov ss, {0:x}",
            "mov ds, {0:x}",
            "mov es, {0:x}",
            in(reg) KERNEL_DATA,
            options(nostack, preserves_flags),
        );

    }
}

/// Read back what the CPU thinks its GDT is — used by `explain gdt`.
pub fn current() -> (u64, u16) {
    let mut pointer = DescriptorTablePointer { limit: 0, base: 0 };
    unsafe {
        asm!("sgdt [{}]", in(reg) &mut pointer, options(nostack, preserves_flags));
    }
    (pointer.base, pointer.limit)
}

/// The raw entries, so the shell can decode them in front of you.
pub fn entries() -> [u64; 3] {
    unsafe { *core::ptr::addr_of!(GDT) }
}
