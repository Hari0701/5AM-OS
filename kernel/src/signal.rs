//! Signals: making a program run something it did not call.
//!
//! Every other way the kernel affects a process is a reply. The process asks,
//! the kernel answers. A signal is the other direction — the kernel forcing a
//! detour through code the program never called, at a moment the program did
//! not choose.
//!
//! ## The trick, which is entirely about stacks
//!
//! There is no way to "call" a function in a process that is not running. So
//! the kernel does not call anything. It waits until the process is *about* to
//! resume — returning from a syscall, or being picked by the scheduler — and
//! then rewrites the trap frame it is going to resume through.
//!
//! Before rewriting it, the whole frame is pushed onto the program's own stack,
//! and a return address is pushed below it. Then RIP is set to the handler and
//! RDI to the signal number. The program resumes, finds itself inside a
//! function it never called, and when that function returns it lands on the
//! `restorer` — a few instructions that ask the kernel to put everything back.
//!
//! So a signal handler is a genuine function call, made by writing a stack
//! frame from outside. The interrupted code has no idea; it resumes with every
//! register exactly as it was, because every register was on the stack the
//! whole time.
//!
//! ## Why the restorer belongs to the program
//!
//! Linux keeps that stub in the vDSO, a page the kernel maps into everyone. We
//! have no vDSO, so the program supplies the address when it registers a
//! handler. That is more honest for a teaching kernel anyway: the trampoline is
//! visible in `userland/`, and you can read the two instructions that make
//! returning from a handler possible.
//!
//! ## What is deliberately blunt
//!
//! No masking, no queueing, no `sigaction` flags. A pending signal is one bit,
//! so two of the same kind collapse into one — which is true of real signals
//! too, and the reason they carry no data.

use crate::task;

pub const SIGINT: usize = 2;
pub const SIGKILL: usize = 9;
pub const SIGTERM: usize = 15;
pub const MAX_SIGNALS: usize = 16;

/// Where things live in the twenty-word frame both entry paths save.
///
/// Pushed rax first and popped r15 first, so the register block runs backwards:
/// index 0 is r15 and index 14 is rax.
const RDI: usize = 8;
const RIP: usize = 15;
const CS: usize = 16;
const RSP: usize = 18;
pub const FRAME_WORDS: usize = 20;

/// Can this signal be caught at all?
///
/// SIGKILL cannot, and that is its entire purpose: every other signal is a
/// request the program may decline, and one has to be an instruction.
pub fn catchable(signal: usize) -> bool {
    signal != SIGKILL
}

/// Deliver at most one pending signal by rewriting `frame`.
///
/// Returns true if the task should be terminated instead — the caller decides
/// how, because "stop running" means something different inside a syscall than
/// it does inside the scheduler.
///
/// # Safety
/// `frame` must be a twenty-word frame belonging to `id`, and that task's
/// address space must be the active one: this writes to the *user's* stack.
pub unsafe fn deliver(id: usize, frame: *mut u64) -> bool {
    let Some(signal) = task::take_pending_signal(id) else {
        return false;
    };

    // Only interrupt a program that is actually in ring 3. A task stopped
    // inside the kernel has kernel state on that stack, and dropping a handler
    // frame on top of it would resume the handler with the kernel's registers.
    let in_user_mode = unsafe { *frame.add(CS) } & 3 == 3;
    if !in_user_mode {
        // Put it back and deliver later, at a moment when it is safe.
        task::set_pending_signal(id, signal);
        return false;
    }

    let handler = task::signal_handler(id, signal);
    if handler == 0 || !catchable(signal) {
        // The default action, and the only one this kernel implements.
        return true;
    }

    let restorer = task::signal_restorer(id);
    if restorer == 0 {
        return true;
    }

    // Push the interrupted frame onto the program's own stack, then a return
    // address below it. Sixteen-byte aligned first, because the handler is an
    // ordinary function and the compiler will assume the ABI holds.
    let user_stack = unsafe { *frame.add(RSP) };
    let saved = (user_stack - (FRAME_WORDS as u64 * 8)) & !0xF;
    for index in 0..FRAME_WORDS {
        unsafe { *((saved as *mut u64).add(index)) = *frame.add(index) };
    }

    // The return address the handler will `ret` to. A function starts life with
    // rsp % 16 == 8 because a call pushed eight bytes -- so this both provides
    // the return address and fixes the alignment, which is the same rule that
    // cost a day in task.rs.
    let entry_stack = saved - 8;
    unsafe { *(entry_stack as *mut u64) = restorer };

    unsafe {
        *frame.add(RIP) = handler;
        *frame.add(RDI) = signal as u64;
        *frame.add(RSP) = entry_stack;
    }

    false
}

/// Undo a delivery: copy the frame the handler was standing on back.
///
/// The program's stack pointer is, at this moment, exactly where the saved
/// frame begins — the handler's `ret` popped the restorer address and left it
/// there. Nothing has to be remembered on the kernel side at all, which is why
/// signal handlers can nest.
///
/// # Safety
/// Only valid from the `sigreturn` syscall, with the user's frame at `frame`.
pub unsafe fn restore(frame: *mut u64) {
    let saved = unsafe { *frame.add(RSP) };
    for index in 0..FRAME_WORDS {
        unsafe { *frame.add(index) = *((saved as *const u64).add(index)) };
    }
}
