//! The Global Descriptor Table — the CPU's list of memory segments.
//!
//! The bootloader already built a GDT to get us into long mode. We build our
//! own anyway, for two reasons: theirs will be reclaimed as free memory once we
//! start allocating, and building it is how you learn what it is.
//!
//! In 64-bit mode segmentation is *mostly* dead — the base and limit fields are
//! ignored and every segment covers all of memory. What survives matters a lot
//! though: the privilege level (ring 0 vs ring 3), and the TSS, which tells the
//! CPU which stack to switch to when something goes catastrophically wrong.

use core::arch::asm;
use core::mem::size_of;

/// The value you load into a segment register: an index into the GDT plus the
/// requested privilege level in the low two bits.
pub const KERNEL_CODE: u16 = 1 << 3; // entry 1, ring 0
pub const KERNEL_DATA: u16 = 2 << 3; // entry 2, ring 0
pub const TSS_SELECTOR: u16 = 3 << 3; // entry 3-4 (a TSS descriptor is 16 bytes)

/// Ring 3 selectors. The low two bits are the *requested* privilege level, and
/// they are not decoration — load a selector with RPL 0 while in user mode and
/// the CPU refuses. `| 3` is what makes these user selectors.
pub const USER_DATA: u16 = (5 << 3) | 3; // entry 5, ring 3
pub const USER_CODE: u16 = (6 << 3) | 3; // entry 6, ring 3

/// Interrupt Stack Table slot we reserve for double faults.
pub const DOUBLE_FAULT_IST_INDEX: u16 = 0;

/// A stack used *only* for double faults.
///
/// This is the whole reason we bother with a TSS. If the kernel stack overflows,
/// the CPU tries to push a page-fault frame onto the broken stack, fails, and
/// escalates to a double fault — which tries to push again onto the same broken
/// stack, fails again, and triple faults. A triple fault is not an exception;
/// it is the CPU giving up and resetting the machine, with nothing printed.
///
/// The IST breaks that chain by giving the double fault handler a *known good*
/// stack, so it can run and tell us what happened.
const STACK_SIZE: usize = 4096 * 5;
static mut DOUBLE_FAULT_STACK: [u8; STACK_SIZE] = [0; STACK_SIZE];

/// The Task State Segment.
///
/// A leftover from 1985, when the CPU could switch tasks in hardware. Long mode
/// dropped all of that; the struct survives because two of its fields are still
/// useful — the privilege stack table and the interrupt stack table.
#[repr(C, packed)]
#[derive(Clone, Copy)]
struct Tss {
    reserved_1: u32,
    /// Stacks to switch to when entering rings 0-2.
    ///
    /// Slot 0 is the one that matters: when ring 3 code takes an interrupt or
    /// makes a syscall, the CPU loads RSP from here *before* pushing anything.
    /// It has to — the user's stack pointer cannot be trusted, and a kernel that
    /// pushed a trap frame onto a user-controlled address would be handing the
    /// machine away. Leave it zero and the first syscall from ring 3 triple
    /// faults trying to push onto address 0.
    privilege_stack_table: [u64; 3],
    reserved_2: u64,
    /// Seven known-good stacks an interrupt can be forced onto.
    interrupt_stack_table: [u64; 7],
    reserved_3: u64,
    reserved_4: u16,
    iomap_base: u16,
}

impl Tss {
    const fn new() -> Self {
        Self {
            reserved_1: 0,
            privilege_stack_table: [0; 3],
            reserved_2: 0,
            interrupt_stack_table: [0; 7],
            reserved_3: 0,
            reserved_4: 0,
            // No I/O permission bitmap: point past the end of the struct.
            iomap_base: size_of::<Tss>() as u16,
        }
    }
}

static mut TSS: Tss = Tss::new();

/// A kernel stack for privilege transitions -- see `privilege_stack_table`.
static mut SYSCALL_STACK: [u8; STACK_SIZE] = [0; STACK_SIZE];

/// The table: null, kernel code/data, a 16-byte TSS descriptor, user data/code.
static mut GDT: [u64; 7] = [0; 7];

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
        // Point IST slot 0 at the top of our double-fault stack. Stacks on x86
        // grow *downwards*, so the usable pointer is the END of the array.
        let stack_start = core::ptr::addr_of!(DOUBLE_FAULT_STACK) as u64;
        let stack_end = stack_start + STACK_SIZE as u64;
        let tss = &mut *core::ptr::addr_of_mut!(TSS);
        tss.interrupt_stack_table[DOUBLE_FAULT_IST_INDEX as usize] = stack_end;

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

        // [3..4] The TSS descriptor is *sixteen* bytes — a 64-bit base does not
        //        fit in the old eight-byte layout, so it occupies two entries.
        let tss_addr = core::ptr::addr_of!(TSS) as u64;
        let limit = (size_of::<Tss>() - 1) as u64;
        let mut low: u64 = 0;
        low |= limit & 0xFFFF; // limit 0:15
        low |= (tss_addr & 0xFF_FFFF) << 16; // base 0:23
        low |= 0b1001 << 40; // type: available 64-bit TSS
        low |= 1 << 47; // present
        low |= ((limit >> 16) & 0xF) << 48; // limit 16:19
        low |= ((tss_addr >> 24) & 0xFF) << 56; // base 24:31
        gdt[3] = low;
        gdt[4] = tss_addr >> 32; // base 32:63

        // [5] User data and [6] user code. Identical to the kernel pair except
        //     for bits 45-46, the Descriptor Privilege Level. DPL 3 is the
        //     entire difference between code the CPU trusts and code it does
        //     not -- there is no other flag, no other table, no other check.
        gdt[5] = (1 << 44) | (1 << 47) | (1 << 41) | (3 << 45);
        gdt[6] = (1 << 43) | (1 << 44) | (1 << 47) | (1 << 53) | (3 << 45);

        // The stack the CPU switches to on any entry from ring 3.
        let syscall_start = core::ptr::addr_of!(SYSCALL_STACK) as u64;
        tss.privilege_stack_table[0] = (syscall_start + STACK_SIZE as u64) & !0xF;

        // Tell the CPU where the table is.
        let pointer = DescriptorTablePointer {
            limit: (size_of::<[u64; 7]>() - 1) as u16,
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

        // Finally, tell the CPU where the TSS is so IST lookups work.
        asm!("ltr {0:x}", in(reg) TSS_SELECTOR, options(nostack, preserves_flags));
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
pub fn entries() -> [u64; 7] {
    unsafe { *core::ptr::addr_of!(GDT) }
}
