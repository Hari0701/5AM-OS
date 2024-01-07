//! Turning on the floating-point unit.
//!
//! A freshly-booted x86_64 CPU will not execute SSE instructions. It comes up
//! with the FPU *emulated* — a setting inherited from machines where the FPU
//! was a separate chip you might not have bought. Execute an XMM instruction
//! in that state and you do not get a wrong answer; you get an exception.
//!
//! This is the reason kernels usually compile with `+soft-float`: if you never
//! enable the FPU, you must never emit a float instruction, and the compiler
//! guarantees that for you. 5AM-OS gives that guarantee up on purpose, because
//! a neural network made of software-emulated floats is not worth running.
//!
//! Four bits, and the machine can do arithmetic.

use core::arch::asm;

/// # Safety
/// Must run once, early, before any floating-point code executes.
pub unsafe fn init() {
    unsafe {
        let mut cr0: u64;
        asm!("mov {}, cr0", out(reg) cr0, options(nomem, nostack));

        // EM (bit 2) — "no FPU present, trap and emulate it in software".
        // It is set at reset. Leave it set and every SSE instruction raises
        // #UD, which for us would mean a fault instead of a multiply.
        cr0 &= !(1 << 2);

        // MP (bit 1) — monitor coprocessor. Pairs with TS to make lazy FPU
        // context switching work: with both set, the first FP instruction after
        // a task switch traps so the kernel can swap FPU state. We have one
        // thread and no task switching, but the bit is expected to be set.
        cr0 |= 1 << 1;

        asm!("mov cr0, {}", in(reg) cr0, options(nomem, nostack));

        let mut cr4: u64;
        asm!("mov {}, cr4", out(reg) cr4, options(nomem, nostack));

        // OSFXSR (bit 9) — "this OS knows how to save SSE state with fxsave".
        // The CPU refuses to execute SSE instructions until you claim this,
        // because otherwise a context switch would silently corrupt registers
        // the kernel does not know exist.
        cr4 |= 1 << 9;

        // OSXMMEXCPT (bit 10) — "route SSE numeric errors to vector 19 rather
        // than raising #UD". Without it, a floating-point error arrives as an
        // invalid-opcode fault and sends you hunting for the wrong bug.
        cr4 |= 1 << 10;

        asm!("mov cr4, {}", in(reg) cr4, options(nomem, nostack));
    }
}

/// Read back what actually got set, for `explain fpu` and the boot narration.
pub fn state() -> (u64, u64) {
    let (cr0, cr4);
    unsafe {
        asm!(
            "mov {cr0}, cr0",
            "mov {cr4}, cr4",
            cr0 = out(reg) cr0,
            cr4 = out(reg) cr4,
            options(nomem, nostack, preserves_flags),
        );
    }
    (cr0, cr4)
}

/// Is the FPU actually usable right now?
pub fn enabled() -> bool {
    let (cr0, cr4) = state();
    cr0 & (1 << 2) == 0 && cr4 & (1 << 9) != 0
}
