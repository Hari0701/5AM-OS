//! Preemptive multitasking: making one CPU look like several.
//!
//! Until now this kernel did exactly one thing at a time. Running the
//! transformer froze the shell for fifteen seconds — you could press keys, the
//! interrupt still fired and the bytes piled up in a buffer, but nothing
//! happened, because the shell *was* the thing running the transformer.
//!
//! ## What a task actually is
//!
//! Nothing but **register values plus a stack**. That is the whole idea. To
//! switch from one job to another you save the registers onto the current
//! stack, remember where that stack pointer was, load a different stack
//! pointer, and pop registers off *that* stack. Then you return — into a
//! completely different execution stream.
//!
//! The strange part is that the return does not come back to you. Something
//! else continues, and later something returns to you as though no time had
//! passed.
//!
//! ## Why the timer entry is written in assembly
//!
//! Every other interrupt handler here uses Rust's `extern "x86-interrupt"`,
//! which generates its own prologue and epilogue. That is fine when a handler
//! returns to the code it interrupted — and useless here, because we need to
//! return somewhere *else*, which means we need to know the exact layout of
//! what was pushed. So this one handler pushes the registers itself, in a known
//! order, hands the stack pointer to Rust, and uses whatever pointer Rust hands
//! back.
//!
//! A new task's stack is built to *look* like a task that was interrupted: a
//! fake interrupt frame with the entry point as its return address, and zeros
//! where the saved registers go. The first switch to it pops those zeros and
//! `iretq`s into the entry point, and the task never knows it was never
//! actually running.

use crate::interrupts::without_interrupts;
use crate::{gdt, println};
use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::arch::naked_asm;

pub const MAX_TASKS: usize = 8;
const STACK_SIZE: usize = 64 * 1024;

#[derive(Clone, Copy, PartialEq)]
pub enum State {
    Free,
    Ready,
    Finished,
}

pub struct Task {
    pub state: State,
    pub name: [u8; 24],
    pub name_len: usize,
    /// Where this task's saved registers are. Meaningless while it is running.
    pub stack_pointer: u64,
    /// Kept alive because the task is standing on it.
    _stack: Option<Box<[u8]>>,
    pub switches: u64,
    /// Work for the task to do, read by `task_entry`.
    pub prompt: Option<String>,
}

impl Task {
    const fn empty() -> Self {
        Self {
            state: State::Free,
            name: [0; 24],
            name_len: 0,
            stack_pointer: 0,
            _stack: None,
            switches: 0,
            prompt: None,
        }
    }

    pub fn name(&self) -> &str {
        core::str::from_utf8(&self.name[..self.name_len]).unwrap_or("?")
    }
}

static mut TASKS: [Task; MAX_TASKS] = [const { Task::empty() }; MAX_TASKS];
static mut CURRENT: usize = 0;
static mut ENABLED: bool = false;

fn tasks() -> &'static mut [Task; MAX_TASKS] {
    unsafe { &mut *core::ptr::addr_of_mut!(TASKS) }
}

pub fn current_id() -> usize {
    unsafe { core::ptr::read_volatile(core::ptr::addr_of!(CURRENT)) }
}

/// Register task 0 as the code that is already running: the shell.
///
/// It needs no stack allocated — it is standing on one. Its saved stack pointer
/// gets filled in the first time it is switched away from.
pub fn init() {
    let tasks = tasks();
    tasks[0].state = State::Ready;
    let name = b"shell";
    tasks[0].name[..name.len()].copy_from_slice(name);
    tasks[0].name_len = name.len();
    unsafe {
        CURRENT = 0;
        ENABLED = true;
    }
}

/// Create a task that will run `prompt` through the transformer.
pub fn spawn(name: &str, prompt: String) -> Result<usize, &'static str> {
    without_interrupts(|| {
        let tasks = tasks();
        let id = (1..MAX_TASKS)
            .find(|&i| tasks[i].state == State::Free)
            .ok_or("no free task slots")?;

        // Every task needs its own stack, and this is the first thing in the
        // kernel that could not have existed before the allocator did.
        let stack: Box<[u8]> = vec![0u8; STACK_SIZE].into_boxed_slice();
        let top = stack.as_ptr() as u64 + STACK_SIZE as u64;
        let top = top & !0xF;

        // Build a stack that looks like a task which was interrupted.
        let stack_pointer = unsafe { build_frame(top, task_entry as u64) };

        let task = &mut tasks[id];
        task.state = State::Ready;
        task.stack_pointer = stack_pointer;
        task._stack = Some(stack);
        task.switches = 0;
        task.prompt = Some(prompt);
        let bytes = name.as_bytes();
        let len = bytes.len().min(task.name.len());
        task.name[..len].copy_from_slice(&bytes[..len]);
        task.name_len = len;

        Ok(id)
    })
}

/// Lay out a fake interrupt frame plus zeroed registers.
///
/// The order here must be the exact reverse of what `timer_entry` pops.
///
/// ## The alignment trap
///
/// `top` is 16-byte aligned, and handing that straight to the task is wrong in
/// a way that takes a fault to discover. The ABI does not say "aligned at entry"
/// — it says the stack was 16-aligned *before the `call`*, and the call pushed
/// eight bytes of return address. So a function's first instruction sees
/// `rsp % 16 == 8`, and the compiler emits `movaps` against that assumption.
/// Start a task on a 16-aligned stack and every spill is off by eight, which the
/// CPU reports as a #GP with error code 0 — not the alignment complaint you
/// would hope for.
///
/// # Safety
/// `top` must be the 16-aligned top of a stack big enough for the frame.
unsafe fn build_frame(top: u64, entry: u64) -> u64 {
    // Where the task's own RSP starts: one slot down, standing where a return
    // address would be if anyone had called it. Nothing reads that slot —
    // `task_entry` never returns — it exists purely to fix the alignment.
    let task_rsp = top - 8;
    let mut sp = top;
    let mut push = |value: u64| {
        sp -= 8;
        unsafe { *(sp as *mut u64) = value };
    };

    // What the CPU pushes on an interrupt, in the order it pushes it.
    push(0);                       // SS: 0 is legal for a ring-0 iretq
    push(task_rsp);                // RSP the task resumes with
    push(0x202);                   // RFLAGS: interrupts enabled, reserved bit
    push(gdt::KERNEL_CODE as u64); // CS
    push(entry);                   // RIP — where the task begins

    // The fifteen registers timer_entry pushes, all zero to start with.
    for _ in 0..15 {
        push(0);
    }
    sp
}

/// Where every spawned task begins.
extern "C" fn task_entry() -> ! {
    let id = current_id();
    let prompt = without_interrupts(|| tasks()[id].prompt.take());

    if let Some(prompt) = prompt {
        crate::llm::generate(&prompt, 96);
    }

    without_interrupts(|| {
        tasks()[id].state = State::Finished;
    });
    println!();
    println!("[task] {id} finished. Press enter for a prompt.");

    // The scheduler will not pick a Finished task again, so this only spins
    // until the next tick.
    loop {
        unsafe { core::arch::asm!("hlt", options(nomem, nostack)) };
    }
}

/// Pick the next runnable task and hand back its stack pointer.
///
/// Called from `timer_entry` with the interrupted task's stack pointer. Whatever
/// this returns becomes the stack the CPU resumes on — which is the entire
/// mechanism of multitasking, in one return value.
#[unsafe(no_mangle)]
extern "C" fn schedule(stack_pointer: u64) -> u64 {
    let enabled = unsafe { core::ptr::read_volatile(core::ptr::addr_of!(ENABLED)) };
    if !enabled {
        return stack_pointer;
    }

    let tasks = tasks();
    let current = current_id();
    tasks[current].stack_pointer = stack_pointer;

    // Round robin: start after the current task and take the next Ready one.
    let mut next = current;
    for offset in 1..=MAX_TASKS {
        let candidate = (current + offset) % MAX_TASKS;
        if tasks[candidate].state == State::Ready {
            next = candidate;
            break;
        }
    }

    if next != current {
        unsafe { CURRENT = next };
        tasks[next].switches += 1;
    }
    tasks[next].stack_pointer
}

/// The timer interrupt, with the register handling written out by hand.
///
/// # Safety
/// Installed directly in the IDT as vector 32. Never called from Rust.
#[unsafe(naked)]
pub unsafe extern "C" fn timer_entry() {
    naked_asm!(
        // Save every general-purpose register. The order matters only in that
        // build_frame must mirror it exactly.
        "push rax", "push rcx", "push rdx", "push rbx",
        "push rbp", "push rsi", "push rdi",
        "push r8",  "push r9",  "push r10", "push r11",
        "push r12", "push r13", "push r14", "push r15",

        // Tell the PIC we are done before switching away. Do it after and the
        // controller waits forever for an EOI from a task that is no longer
        // running, and no further timer interrupts ever arrive.
        "mov al, 0x20",
        "out 0x20, al",

        // Count the tick.
        "inc qword ptr [rip + {ticks}]",

        // schedule(current_rsp) -> rsp to resume on.
        "mov rdi, rsp",
        "call {schedule}",
        "mov rsp, rax",

        // Restore, in exact reverse.
        "pop r15", "pop r14", "pop r13", "pop r12",
        "pop r11", "pop r10", "pop r9",  "pop r8",
        "pop rdi", "pop rsi", "pop rbp",
        "pop rbx", "pop rdx", "pop rcx", "pop rax",
        "iretq",
        ticks = sym crate::interrupts::TICKS,
        schedule = sym schedule,
    )
}

/// Report every task, for the shell.
pub fn report() {
    let tasks = tasks();
    let current = current_id();
    if crate::llm::busy() {
        println!("  the model is claimed by one task (only one may run it)");
    }
    println!("  id  name      state     switches");
    for (id, task) in tasks.iter().enumerate() {
        let state = match task.state {
            State::Free => continue,
            State::Ready if id == current => "running",
            State::Ready => "ready",
            State::Finished => "finished",
        };
        println!("  {id:<3} {:<9} {state:<9} {}", task.name(), task.switches);
    }
}

/// Give up the rest of this time slice.
///
/// Not required for preemption — the timer takes the CPU away regardless — but
/// it makes waiting politely cheap instead of burning a slice spinning.
pub fn yield_now() {
    unsafe { core::arch::asm!("hlt", options(nomem, nostack)) };
}

/// So the shell can tell whether anything is still working.
pub fn busy() -> bool {
    tasks()
        .iter()
        .enumerate()
        .any(|(id, t)| id != 0 && t.state == State::Ready)
}

pub fn reap_finished() {
    without_interrupts(|| {
        for task in tasks().iter_mut().skip(1) {
            if task.state == State::Finished {
                *task = Task::empty();
            }
        }
    })
}

/// Keeps `Vec` in scope for the stack allocation above.
const _: fn() -> Vec<u8> = || vec![0u8; 0];
