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

use crate::keyboard;
use crate::serial::{inb, outb};
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

    /// Point this vector at `handler` and let ring 3 raise it deliberately.
    ///
    /// The DPL in an IDT entry answers a different question from the DPL in a
    /// GDT entry: not "who may run this code" but "who may reach it with an
    /// `int` instruction". Leave it at 0 and hardware can still deliver the
    /// vector, but a user program executing `int 0x80` gets a general
    /// protection fault -- which is exactly what you want for every vector
    /// except the one that is meant to be a front door.
    fn set_user_callable(&mut self, handler: u64) {
        self.set(handler, 0);
        // 0xEE00 = present, DPL 3, 64-bit interrupt gate.
        self.options = 0xEE00;
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
/// IRQ4 is COM1. Typing into the serial console arrives here.
pub const SERIAL_VECTOR: u8 = 36;

/// Ticks since boot.
///
/// Incremented by the assembly in task.rs rather than by Rust, now that the
/// timer entry is written by hand — hence no_mangle, so `inc qword ptr` can
/// name it.
#[unsafe(no_mangle)]
pub static mut TICKS: u64 = 0;

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

        // The timer is the one handler not written in Rust: preemption means
        // returning onto a different stack, which needs an exactly known
        // register layout. See task.rs.
        idt[TIMER_VECTOR as usize].set(crate::task::timer_entry as *const () as u64, 0);
        idt[KEYBOARD_VECTOR as usize].set(keyboard_irq as *const () as u64, 0);
        idt[SERIAL_VECTOR as usize].set(serial_irq as *const () as u64, 0);

        // The syscall gate: the single vector ring 3 is permitted to raise.
        idt[crate::syscall::SYSCALL_VECTOR as usize]
            .set_user_callable(crate::syscall::syscall_entry as *const () as u64);

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

        // Unmask the timer (IRQ0), keyboard (IRQ1) and COM1 (IRQ4). Everything
        // else stays masked: a device we have no handler for would otherwise
        // interrupt us into a fault.
        outb(PIC1_DATA, 0b1110_1100);
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

/// Is the interrupt flag currently set?
///
/// Read from RFLAGS rather than tracked in a variable, because the CPU changes
/// it behind our back: entering an interrupt gate clears it, `iretq` restores
/// it. Anything we recorded ourselves would be wrong the moment hardware
/// disagreed.
pub fn are_enabled() -> bool {
    let flags: u64;
    unsafe { asm!("pushfq; pop {}", out(reg) flags, options(nomem, preserves_flags)) };
    flags & (1 << 9) != 0
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
    let was_enabled = are_enabled();

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
    // We are about to stop the machine. Take the console by force rather than
    // waiting on a lock whose holder may be the code that just faulted.
    unsafe { crate::serial::force_unlock_console() };
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
    // We are about to stop the machine. Take the console by force rather than
    // waiting on a lock whose holder may be the code that just faulted.
    unsafe { crate::serial::force_unlock_console() };
    println!("\n[trap] INVALID OPCODE at {:#x}", frame.rip);
    crate::halt_forever();
}

extern "x86-interrupt" fn general_protection(frame: InterruptStackFrame, error: u64) {
    // We are about to stop the machine. Take the console by force rather than
    // waiting on a lock whose holder may be the code that just faulted.
    unsafe { crate::serial::force_unlock_console() };
    println!("\n[trap] GENERAL PROTECTION FAULT at {:#x}", frame.rip);
    println!("       error code {error:#x}");
    println!();
    crate::oracle::explain_fault("general_protection", frame.rip, error, 0);
    crate::halt_forever();
}

extern "x86-interrupt" fn page_fault(frame: InterruptStackFrame, error: u64) {
    // CR2 holds the address that could not be translated.
    let address: u64;
    unsafe { asm!("mov {}, cr2", out(reg) address, options(nomem, nostack)) };

    // Not every page fault is an error. This is the one that is routine: a
    // write to a page that fork made read-only on purpose. Handle it, and
    // return -- `iretq` re-runs the instruction that faulted, which now
    // succeeds and never knows anything happened.
    //
    // This is the moment paging stops being a lookup table and becomes a
    // mechanism: the kernel gets control at the exact instant a program
    // touches memory, and can decide what that memory is.
    // Note what is *not* checked here: which ring the write came from. The
    // copy-on-write bit in the page table is the authority, not the privilege
    // level of whoever touched it -- the kernel writes into a forked process's
    // memory too, and that write deserves the same private copy. Gating this
    // on a ring 3 fault was my first version, and it made every kernel-side
    // write to a shared page fatal instead of routine.
    const PRESENT: u64 = 1 << 0;
    const WRITE: u64 = 1 << 1;
    if error & PRESENT != 0 && error & WRITE != 0 {
        if unsafe { crate::memory::cow_fault(address) } {
            return;
        }
    }

    // Everything past here is fatal, so take the console by force rather than
    // waiting on a lock whose holder may be the code that just faulted.
    unsafe { crate::serial::force_unlock_console() };

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

    // Diagnose it here, inside the machine that broke. No network, no host,
    // no waiting -- and no chance of a confident wrong answer, because this
    // decodes the real error bits rather than generating prose about them.
    println!();
    crate::oracle::explain_fault("page_fault", frame.rip, error, address);

    crate::halt_forever();
}

extern "x86-interrupt" fn double_fault(frame: InterruptStackFrame, _error: u64) -> ! {
    // We are about to stop the machine. Take the console by force rather than
    // waiting on a lock whose holder may be the code that just faulted.
    unsafe { crate::serial::force_unlock_console() };
    // We are running on the IST stack right now. If we were not, this message
    // would never appear — the machine would have reset instead.
    println!("\n[trap] DOUBLE FAULT at {:#x}", frame.rip);
    println!("       A fault occurred while handling a fault.");
    println!("       Reached this handler on the IST stack, which is the only");
    println!("       reason you are reading this instead of watching a reboot.");
    println!();
    crate::oracle::explain_fault("double_fault", frame.rip, 0, 0);
    crate::halt_forever();
}

extern "x86-interrupt" fn keyboard_irq(_frame: InterruptStackFrame) {
    // Port 0x60 is the keyboard controller's data register. The byte must be
    // read even if we do nothing with it, or the controller will not send
    // another interrupt.
    let scancode = unsafe { inb(0x60) };
    keyboard::push_scancode(scancode);
    crate::task::wake_all(keyboard::INPUT_CHANNEL);
    unsafe { end_of_interrupt(KEYBOARD_VECTOR) };
}

extern "x86-interrupt" fn serial_irq(_frame: InterruptStackFrame) {
    // Drain the FIFO rather than taking one byte: the UART raises a single
    // interrupt for a burst, so reading once would leave the rest sitting there
    // until the next keystroke happened to arrive.
    unsafe {
        let console = &mut *core::ptr::addr_of_mut!(crate::serial::CONSOLE);
        while let Some(byte) = console.try_recv() {
            // Ctrl-C is handled here, as it arrives, and never becomes data.
            //
            // The first version checked for it where a program *reads* the
            // console, which cannot work: the program you most want to
            // interrupt is the one not reading anything. Nobody drained the
            // buffer, so the byte sat in it while the program spun. A terminal
            // driver has always done this on receipt, and this is why.
            if byte == 0x03 {
                if let Some(target) = crate::task::foreground_user_task() {
                    crate::task::signal(target, crate::signal::SIGINT);
                    crate::println!();
                    crate::println!("  [console] ^C -- SIGINT to task {target}");
                }
                continue;
            }
            crate::serial::push_input(byte);
        }
        crate::task::wake_all(keyboard::INPUT_CHANNEL);
        end_of_interrupt(SERIAL_VECTOR);
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
