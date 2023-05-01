//! The Interrupt Descriptor Table, and the handlers it points at.
//!
//! Until this module runs, any mistake the kernel makes is fatal and silent:
//! divide by zero, dereference a bad pointer, overflow the stack — the CPU
//! looks for a handler, finds none, and triple faults. The machine resets with
//! nothing printed.
//!
//! An IDT is 256 slots, one per interrupt vector. The first 32 are defined by
//! Intel (faults the CPU raises itself). The rest are ours, and we point the
//! next 16 at hardware devices via the PIC.

use crate::serial::outb;
use crate::{gdt, println};
use core::arch::asm;
use core::mem::size_of;

/// One IDT entry: a handler address, split into three pieces for backwards
/// compatibility with a 16-bit design from 1982.
#[repr(C, packed)]
#[derive(Clone, Copy)]
struct Entry {
    ptr_low: u16,
    gdt_selector: u16,
    options: u16,
    ptr_mid: u16,
    ptr_high: u32,
    reserved: u32,
}

impl Entry {
    const fn empty() -> Self {
        Self {
            ptr_low: 0,
            gdt_selector: 0,
            options: 0,
            ptr_mid: 0,
            ptr_high: 0,
            reserved: 0,
        }
    }

    /// Point this vector at `handler`.
    ///
    /// `ist` selects an Interrupt Stack Table slot: 0 means "keep using the
    /// current stack", 1-7 mean "switch to this known-good stack first".
    fn set(&mut self, handler: u64, ist: u16) {
        self.ptr_low = handler as u16;
        self.ptr_mid = (handler >> 16) as u16;
        self.ptr_high = (handler >> 32) as u32;
        self.gdt_selector = gdt::KERNEL_CODE;
        // 0x8E00 = present, ring 0, 64-bit interrupt gate.
        //
        // "Interrupt gate" rather than "trap gate" means the CPU clears the
        // interrupt flag on entry, so a handler is not itself interrupted
        // before it has saved anything.
        self.options = 0x8E00 | ist;
    }
}

/// What the CPU pushes before entering a handler. Layout is fixed by hardware.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct InterruptStackFrame {
    pub rip: u64,
    pub cs: u64,
    pub rflags: u64,
    pub rsp: u64,
    pub ss: u64,
}

#[repr(C, packed)]
struct DescriptorTablePointer {
    limit: u16,
    base: u64,
}

static mut IDT: [Entry; 256] = [Entry::empty(); 256];

// The 8259 PIC pair. Vectors 0-31 are Intel's, so the PIC's default of 8-15
// collides with them — remapping is not optional.
const PIC1_COMMAND: u16 = 0x20;
const PIC1_DATA: u16 = 0x21;
const PIC2_COMMAND: u16 = 0xA0;
const PIC2_DATA: u16 = 0xA1;
const PIC_EOI: u8 = 0x20;

pub const TIMER_VECTOR: u8 = 32;
pub const KEYBOARD_VECTOR: u8 = 33;

/// Ticks since boot. Written by the timer interrupt, read by everyone else.
static mut TICKS: u64 = 0;

pub fn ticks() -> u64 {
    unsafe { core::ptr::read_volatile(core::ptr::addr_of!(TICKS)) }
}

/// # Safety
/// Installs the IDT and reprograms the interrupt controller. Call once, after
/// the GDT is loaded, with interrupts still disabled.
pub unsafe fn init() {
    unsafe {
        let idt = &mut *core::ptr::addr_of_mut!(IDT);

        idt[0].set(divide_by_zero as *const () as u64, 0);
        idt[3].set(breakpoint as *const () as u64, 0);
        idt[6].set(invalid_opcode as *const () as u64, 0);
        // The double fault runs on its own stack — see gdt.rs for why.
        idt[8].set(double_fault as *const () as u64, gdt::DOUBLE_FAULT_IST_INDEX + 1);
        idt[13].set(general_protection as *const () as u64, 0);
        idt[14].set(page_fault as *const () as u64, 0);

        idt[TIMER_VECTOR as usize].set(timer as *const () as u64, 0);

        let pointer = DescriptorTablePointer {
            limit: (size_of::<[Entry; 256]>() - 1) as u16,
            base: core::ptr::addr_of!(IDT) as u64,
        };
        asm!("lidt [{}]", in(reg) &pointer, options(readonly, nostack, preserves_flags));

        remap_pic();
    }
}

/// Move the PIC's interrupts from vectors 8-15 to 32-47.
///
/// The 8259 is configured by writing a fixed sequence of "initialisation
/// command words" in order. There is no way to read the current state back;
/// you simply have to do it right.
unsafe fn remap_pic() {
    unsafe {
        // ICW1: begin initialisation, expect ICW4.
        outb(PIC1_COMMAND, 0x11);
        outb(PIC2_COMMAND, 0x11);
        // ICW2: the new vector offsets.
        outb(PIC1_DATA, TIMER_VECTOR);
        outb(PIC2_DATA, TIMER_VECTOR + 8);
        // ICW3: how the two chips are wired to each other.
        outb(PIC1_DATA, 4); // secondary is on primary's IRQ2
        outb(PIC2_DATA, 2); // secondary's identity is 2
        // ICW4: 8086 mode.
        outb(PIC1_DATA, 0x01);
        outb(PIC2_DATA, 0x01);

        // Mask everything except the timer (IRQ0). A device
        // we have no handler for would otherwise interrupt us into a fault.
        outb(PIC1_DATA, 0b1111_1110);
        outb(PIC2_DATA, 0b1111_1111);
    }
}

/// Tell the PIC we are done, or it will never send that IRQ again.
unsafe fn end_of_interrupt(vector: u8) {
    unsafe {
        if vector >= TIMER_VECTOR + 8 {
            outb(PIC2_COMMAND, PIC_EOI);
        }
        outb(PIC1_COMMAND, PIC_EOI);
    }
}

pub fn enable() {
    unsafe { asm!("sti", options(nomem, nostack)) };
}

pub fn disable() {
    unsafe { asm!("cli", options(nomem, nostack)) };
}

/// Run `f` with interrupts off, restoring them afterwards if they were on.
///
/// This is the kernel's most basic critical section: it is how you touch data
/// that an interrupt handler also touches without being interrupted halfway.
pub fn without_interrupts<F, R>(f: F) -> R
where
    F: FnOnce() -> R,
{
    let flags: u64;
    unsafe { asm!("pushfq; pop {}", out(reg) flags, options(nomem, preserves_flags)) };
    let was_enabled = flags & (1 << 9) != 0; // IF, the interrupt flag

    disable();
    let result = f();
    if was_enabled {
        enable();
    }
    result
}

// --- handlers ------------------------------------------------------------
//
// `extern "x86-interrupt"` tells rustc this is entered by hardware, not called:
// every register must be preserved, and it returns with `iretq` rather than
// `ret`. Writing this correctly by hand in assembly is an afternoon; the ABI
// makes it a keyword.

extern "x86-interrupt" fn divide_by_zero(frame: InterruptStackFrame) {
    println!("\n[trap] DIVIDE BY ZERO at {:#x}", frame.rip);
    println!("       The CPU cannot represent the result, so it asked us.");
    crate::halt_forever();
}

extern "x86-interrupt" fn breakpoint(frame: InterruptStackFrame) {
    println!("\n[trap] BREAKPOINT at {:#x}", frame.rip);
    println!("       int3 — what a debugger patches into your code.");
    println!("       We handled it and are returning to the next instruction.\n");
}

extern "x86-interrupt" fn invalid_opcode(frame: InterruptStackFrame) {
    println!("\n[trap] INVALID OPCODE at {:#x}", frame.rip);
    crate::halt_forever();
}

extern "x86-interrupt" fn general_protection(frame: InterruptStackFrame, error: u64) {
    println!("\n[trap] GENERAL PROTECTION FAULT at {:#x}", frame.rip);
    println!("       error code {error:#x}");
    println!("       Something was attempted that the current ring forbids.");
    crate::halt_forever();
}

extern "x86-interrupt" fn page_fault(frame: InterruptStackFrame, error: u64) {
    // CR2 holds the address that could not be translated.
    let address: u64;
    unsafe { asm!("mov {}, cr2", out(reg) address, options(nomem, nostack)) };

    println!("\n[trap] PAGE FAULT");
    println!("       tried to touch : {address:#018x}");
    println!("       from           : {:#018x}", frame.rip);
    println!("       error code     : {error:#b}");
    println!(
        "         present    {}  {}",
        error & 1,
        if error & 1 == 0 { "page not mapped" } else { "protection violation" }
    );
    println!(
        "         write      {}  {}",
        (error >> 1) & 1,
        if (error >> 1) & 1 == 1 { "was a write" } else { "was a read" }
    );
    println!("         user       {}", (error >> 2) & 1);
    println!();
    println!("       This is the fault that makes virtual memory possible: a");
    println!("       real kernel would map a page here and return.");
    crate::halt_forever();
}

extern "x86-interrupt" fn double_fault(frame: InterruptStackFrame, _error: u64) -> ! {
    // We are running on the IST stack right now. If we were not, this message
    // would never appear — the machine would have reset instead.
    println!("\n[trap] DOUBLE FAULT at {:#x}", frame.rip);
    println!("       A fault occurred while handling a fault.");
    println!("       Reached this handler on the IST stack, which is the only");
    println!("       reason you are reading this instead of watching a reboot.");
    crate::halt_forever();
}

extern "x86-interrupt" fn timer(_frame: InterruptStackFrame) {
    unsafe {
        let ticks = core::ptr::addr_of_mut!(TICKS);
        core::ptr::write_volatile(ticks, core::ptr::read_volatile(ticks) + 1);
        end_of_interrupt(TIMER_VECTOR);
    }
}

/// Where the CPU thinks the IDT is — for `explain idt`.
pub fn current() -> (u64, u16) {
    let mut pointer = DescriptorTablePointer { limit: 0, base: 0 };
    unsafe {
        asm!("sidt [{}]", in(reg) &mut pointer, options(nostack, preserves_flags));
    }
    (pointer.base, pointer.limit)
}

/// Is a given vector wired up? Used by the shell to show the live table.
pub fn is_present(vector: usize) -> bool {
    unsafe {
        let idt = &*core::ptr::addr_of!(IDT);
        idt[vector].options & (1 << 15) != 0
    }
}
