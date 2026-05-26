//! Waking the other processors.
//!
//! Everything in this kernel so far has been true of one core. Several comments
//! say so out loud: `without_interrupts` is a complete critical section, a
//! spinlock can never actually spin, one address space is active at a time.
//! Every one of those stops being true the moment a second core is running
//! kernel code.
//!
//! So this module does the first half only, and deliberately stops there. It
//! starts the other processors and proves they are alive; it does **not** let
//! them into the scheduler, the frame allocator or the heap, because none of
//! those are safe for two callers yet. That is the honest order for a port: get
//! the cores running, then make the kernel worthy of them.
//!
//! ## How a processor is started
//!
//! Only one core runs at power-on. The others sit halted until the first one
//! sends them two interrupts through the local APIC — INIT, then STARTUP — and
//! the STARTUP carries a page number. The woken core begins executing at that
//! physical address **in 16-bit real mode**, exactly as an 8086 would in 1978,
//! because the startup protocol has never changed.
//!
//! So the trampoline below has to walk the whole history: real mode, then
//! protected mode, then long mode, in about forty instructions. It must live in
//! the first megabyte (the STARTUP vector is a byte), and it must be identity
//! mapped, because paging is switched on halfway through and the instruction
//! after that has to still be at the address the processor thinks it is at.

extern crate alloc;

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

/// Where the local APIC's registers appear in physical memory. Every core sees
/// its *own* APIC at this address, which is how a core asks who it is.
const APIC_BASE: u64 = 0xFEE0_0000;
const APIC_ID: u64 = 0x020;
const APIC_SPURIOUS: u64 = 0x0F0;
const APIC_ICR_LOW: u64 = 0x300;
const APIC_ICR_HIGH: u64 = 0x310;

/// The page the other cores start executing at. Must be below 1 MiB and page
/// aligned: the STARTUP message carries a page number in one byte.
const TRAMPOLINE: u64 = 0x8000;

pub const MAX_CPUS: usize = 8;

static STARTED: AtomicU32 = AtomicU32::new(1);
static AP_STACK: AtomicU64 = AtomicU64::new(0);

/// Read one of this core's own APIC registers.
///
/// # Safety
/// The APIC must be mapped.
unsafe fn apic_read(register: u64) -> u32 {
    unsafe { core::ptr::read_volatile(crate::memory::physical_to_virtual(APIC_BASE + register) as *const u32) }
}

unsafe fn apic_write(register: u64, value: u32) {
    unsafe {
        core::ptr::write_volatile(
            crate::memory::physical_to_virtual(APIC_BASE + register) as *mut u32,
            value,
        )
    }
}

/// Which core is this? Every core reads a different answer from the same
/// address, which is the whole trick of a *local* APIC.
pub fn cpu_id() -> u32 {
    unsafe { apic_read(APIC_ID) >> 24 }
}

pub fn started() -> u32 {
    STARTED.load(Ordering::Acquire)
}

core::arch::global_asm!(
    r#"
.section .rodata
.globl ap_trampoline_start
.globl ap_trampoline_end
.align 4096
ap_trampoline_start:

// Every address below is absolute, because the processor executes this at
// 0x8000 with no paging and no relocation. `.set` collapses each label into a
// single value first -- an assembler will not take a subtraction of two symbols
// inside a memory operand, which is the same rule that bit the ELF loader.
.set AP_BASE,     0x8000
.set AP_PROT,     AP_BASE + (ap_protected      - ap_trampoline_start)
.set AP_LONG,     AP_BASE + (ap_long           - ap_trampoline_start)
.set AP_GDTP,     AP_BASE + (ap_gdt_pointer    - ap_trampoline_start)
.set AP_GDT,      AP_BASE + (ap_gdt            - ap_trampoline_start)
.set AP_CR3,      AP_BASE + (ap_cr3            - ap_trampoline_start)
.set AP_STACK,    AP_BASE + (ap_stack          - ap_trampoline_start)
.set AP_ENTRY,    AP_BASE + (ap_entry          - ap_trampoline_start)

.code16
    cli
    cld
    xor ax, ax
    mov ds, ax
    mov es, ax
    mov ss, ax

    // Stage 1: executing at all, in 16-bit real mode. Physical address, no
    // paging -- if this byte never changes, the processor never woke.
    mov byte ptr [0x9000], 1

    lgdt [AP_GDTP]

    // Protected mode: one bit in CR0, and a far jump to make it take effect.
    mov eax, cr0
    or eax, 1
    mov cr0, eax
    // Far jump, written as opcodes because no assembler syntax for this is
    // portable between the AT&T and Intel parsers. 0x66 makes the operand
    // 32-bit while still in 16-bit mode; 0xEA is jump-far-absolute, followed by
    // a 32-bit offset and a 16-bit selector.
    .byte 0x66, 0xEA
    .long AP_PROT
    .word 0x08

.code32
ap_protected:
    mov ax, 0x10
    mov ds, ax
    mov es, ax
    mov ss, ax

    // Stage 2: protected mode, still no paging.
    mov byte ptr [0x9000], 2

    // PAE, without which there is no long mode -- plus the two bits that make
    // SSE usable. This kernel is compiled with SSE enabled, so the first
    // floating point or vector instruction on a core without OSFXSR faults.
    mov eax, cr4
    or eax, (1 << 5) | (1 << 9) | (1 << 10)
    mov cr4, eax

    // The page tables the first processor is already using.
    mov eax, [AP_CR3]
    mov cr3, eax

    // EFER: long mode enabled, and no-execute *understood*.
    //
    // NXE is the one that cost a debugging session. The first processor turned
    // it on, so the kernel's page tables have bit 63 set on every page that is
    // not executable. On a core where NXE is clear, bit 63 is a **reserved**
    // bit -- so walking those same tables raises a page fault with the reserved
    // bit set in the error code, at the fourth instruction, before anything can
    // report it.
    //
    // Page tables are shared between processors. The flags that decide how to
    // *read* them are not.
    mov ecx, 0xC0000080
    rdmsr
    or eax, (1 << 8) | (1 << 11)
    wrmsr

    // Paging on. Long mode activates at this instruction, which is exactly why
    // the trampoline must be identity mapped: the *next* instruction is fetched
    // through the page tables, from the same address it was already at.
    // Paging on, and the FPU made usable in the same write: clear EM so SSE
    // instructions are not treated as emulated, set MP alongside it.
    mov eax, cr0
    and eax, 0xFFFFFFFB
    or eax, (1 << 1) | (1 << 31)
    mov cr0, eax

    // Same instruction, now in 32-bit mode, so no size prefix is needed.
    .byte 0xEA
    .long AP_LONG
    .word 0x18

.code64
ap_long:
    // Stage 3: long mode, paging on, and this instruction was fetched through
    // the page tables -- which proves the identity mapping survived.
    mov byte ptr [0x9000], 3

    mov rsp, [AP_STACK]
    mov rax, [AP_ENTRY]
    jmp rax

.align 16
ap_gdt:
    .quad 0
    .quad 0x00CF9A000000FFFF   // 32-bit code
    .quad 0x00CF92000000FFFF   // 32-bit data
    .quad 0x00209A0000000000   // 64-bit code
ap_gdt_pointer:
    .word 31
    .long AP_GDT
.align 8
ap_cr3:
    .quad 0
ap_stack:
    .quad 0
ap_entry:
    .quad 0
ap_trampoline_end:

.section .text
.code64
"#
);

extern "C" {
    static ap_trampoline_start: u8;
    static ap_trampoline_end: u8;
}

/// Offsets of the three values the trampoline reads, from its own start.
fn parameter_offsets() -> (usize, usize, usize) {
    // cr3, stack, entry are the last three quadwords before the end.
    let length = unsafe {
        core::ptr::addr_of!(ap_trampoline_end) as usize
            - core::ptr::addr_of!(ap_trampoline_start) as usize
    };
    (length - 24, length - 16, length - 8)
}

/// Where a woken core begins running Rust.
///
/// It does almost nothing on purpose. Printing is safe because the console
/// lock is a real atomic compare-exchange rather than merely interrupts-off --
/// which is exactly the distinction that looked like ceremony when there was
/// one core. Allocating, scheduling or touching the task table would not be
/// safe, so this does none of it.
extern "C" fn ap_main() -> ! {
    // Stage 4: Rust, on its own stack.
    unsafe { core::ptr::write_volatile(crate::memory::physical_to_virtual(0x9000) as *mut u8, 4) };

    // Descriptor tables first, before anything that could possibly fault.
    //
    // `lgdt` and `lidt` load *per-processor* registers. This core has never
    // executed either, so it is running on the trampoline's temporary GDT with
    // a null IDT -- and a core with a null IDT cannot dispatch its first
    // exception, so it double faults, cannot dispatch that either, and triple
    // faults. That is precisely what happened here, and the QEMU dump said so
    // in one line: `IDT= 0000000000000000`.
    unsafe {
        crate::gdt::load_on_this_processor();
        crate::interrupts::load_idt_on_this_processor();
        core::ptr::write_volatile(crate::memory::physical_to_virtual(0x9000) as *mut u8, 5);
    }
    let id = cpu_id();
    STARTED.fetch_add(1, Ordering::AcqRel);
    unsafe {
        core::ptr::write_volatile(crate::memory::physical_to_virtual(0x9000) as *mut u8, 6)
    };

    // Printing from here is the first genuinely shared thing this core touches,
    // and it is safe for one reason: the console lock is a real atomic
    // compare-exchange, not merely interrupts-off. That distinction looked like
    // ceremony when there was one processor.
    crate::println!("[smp ] processor {id} is awake and running kernel code");

    // Park. Joining the scheduler needs a kernel that is safe for more than one
    // caller, and this one is not yet.
    loop {
        unsafe { core::arch::asm!("hlt", options(nomem, nostack)) };
    }
}

/// Copy the trampoline into low memory, fill in its parameters, and send the
/// wake-up sequence to every other core.
///
/// # Safety
/// Runs once, from the first core, after paging and the heap are up.
/// How far the woken processor got: 0 never ran, 1 real mode, 2 protected,
/// 3 long mode, 4 Rust.
pub fn progress() -> u8 {
    unsafe { core::ptr::read_volatile(crate::memory::physical_to_virtual(0x9000) as *const u8) }
}

pub fn clear_progress() {
    unsafe { core::ptr::write_volatile(crate::memory::physical_to_virtual(0x9000) as *mut u8, 0) };
}

/// Is the APIC reachable at all? Reading its ID register is the cheapest
/// possible test, and the first thing that can fault.
pub fn probe_apic() -> Result<u32, &'static str> {
    let virt = crate::memory::physical_to_virtual(APIC_BASE);
    if crate::memory::translate(virt).is_none() {
        return Err("the APIC's physical address is not mapped");
    }
    Ok(unsafe { apic_read(APIC_ID) >> 24 })
}

/// Put the trampoline in place without waking anything.
pub unsafe fn install_trampoline() -> Result<usize, &'static str> {
    if crate::memory::translate(TRAMPOLINE).is_none() {
        let flags = crate::memory::FLAG_WRITABLE;
        unsafe { crate::memory::map_page(TRAMPOLINE, TRAMPOLINE, flags) }?;
    }
    if crate::memory::translate(0x9000).is_none() {
        let flags = crate::memory::FLAG_WRITABLE;
        unsafe { crate::memory::map_page(0x9000, 0x9000, flags) }?;
    }

    let start = core::ptr::addr_of!(ap_trampoline_start) as *const u8;
    let length = unsafe {
        core::ptr::addr_of!(ap_trampoline_end) as usize
            - core::ptr::addr_of!(ap_trampoline_start) as usize
    };
    unsafe { core::ptr::copy_nonoverlapping(start, TRAMPOLINE as *mut u8, length) };

    let (cr3_offset, stack_offset, entry_offset) = parameter_offsets();
    let cr3 = crate::memory::active_root();
    let stack = alloc::vec![0u8; 16 * 1024].into_boxed_slice();
    let top = (stack.as_ptr() as u64 + 16 * 1024) & !0xF;
    core::mem::forget(stack);
    unsafe {
        *((TRAMPOLINE + cr3_offset as u64) as *mut u64) = cr3;
        *((TRAMPOLINE + stack_offset as u64) as *mut u64) = top;
        *((TRAMPOLINE + entry_offset as u64) as *mut u64) = ap_main as usize as u64;
    }
    clear_progress();
    Ok(length)
}

/// Wake exactly one processor, so a failure names one thing.
///
/// # Safety
/// The trampoline must already be installed.
pub unsafe fn wake_one(target: u32) {
    unsafe {
        let spurious = apic_read(APIC_SPURIOUS);
        apic_write(APIC_SPURIOUS, spurious | 0x100 | 0xFF);

        apic_write(APIC_ICR_HIGH, target << 24);
        apic_write(APIC_ICR_LOW, 0x0000_4500); // INIT, assert
        delay(200_000);
        apic_write(APIC_ICR_HIGH, target << 24);
        apic_write(APIC_ICR_LOW, 0x0000_4600 | (TRAMPOLINE >> 12) as u32); // STARTUP
        delay(400_000);
    }
}

#[allow(dead_code)]
pub unsafe fn start_others() {
    // The trampoline must be readable at the physical address the other cores
    // will jump to, *and* at the same virtual address once they turn paging on.
    if crate::memory::translate(TRAMPOLINE).is_none() {
        let flags = crate::memory::FLAG_WRITABLE;
        if unsafe { crate::memory::map_page(TRAMPOLINE, TRAMPOLINE, flags) }.is_err() {
            crate::println!("[smp ] could not identity map the trampoline");
            return;
        }
    }

    let start = core::ptr::addr_of!(ap_trampoline_start) as *const u8;
    let length = unsafe {
        core::ptr::addr_of!(ap_trampoline_end) as usize
            - core::ptr::addr_of!(ap_trampoline_start) as usize
    };
    unsafe { core::ptr::copy_nonoverlapping(start, TRAMPOLINE as *mut u8, length) };

    let (cr3_offset, stack_offset, entry_offset) = parameter_offsets();
    let cr3 = crate::memory::active_root();
    unsafe {
        *((TRAMPOLINE + cr3_offset as u64) as *mut u64) = cr3;
        *((TRAMPOLINE + entry_offset as u64) as *mut u64) = ap_main as usize as u64;
    }

    // The APIC has to be switched on before it will send anything. Bit 8 of the
    // spurious register is the enable, and the low byte is a vector that must
    // be set even though nothing here will ever raise it.
    unsafe {
        let spurious = apic_read(APIC_SPURIOUS);
        apic_write(APIC_SPURIOUS, spurious | 0x100 | 0xFF);
    }

    let self_id = cpu_id();
    for target in 0..MAX_CPUS as u32 {
        if target == self_id {
            continue;
        }

        // Each core needs its own stack before it runs a single instruction of
        // Rust, because the first thing any function does is push.
        let stack = alloc::vec![0u8; 16 * 1024].into_boxed_slice();
        let top = (stack.as_ptr() as u64 + 16 * 1024) & !0xF;
        core::mem::forget(stack);
        AP_STACK.store(top, Ordering::Release);
        unsafe { *((TRAMPOLINE + stack_offset as u64) as *mut u64) = top };

        unsafe {
            // INIT, then STARTUP twice -- the sequence the manual specifies,
            // and the second STARTUP is ignored by a core that already woke.
            apic_write(APIC_ICR_HIGH, target << 24);
            apic_write(APIC_ICR_LOW, 0x0000_4500); // INIT
            delay(100_000);
            apic_write(APIC_ICR_HIGH, target << 24);
            apic_write(APIC_ICR_LOW, 0x0000_4600 | (TRAMPOLINE >> 12) as u32);
            delay(200_000);
            apic_write(APIC_ICR_HIGH, target << 24);
            apic_write(APIC_ICR_LOW, 0x0000_4600 | (TRAMPOLINE >> 12) as u32);
            delay(200_000);
        }
    }
}

/// Spin for roughly a while. There is no timer available to a core that has not
/// finished starting, so this is what the startup sequence gets.
fn delay(iterations: u64) {
    for _ in 0..iterations {
        core::hint::spin_loop();
    }
}
