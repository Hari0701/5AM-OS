//! Processes: the table, and whose turn it is.
//!
//! `fork` created a second process and then had nowhere to put it, so it went
//! in a single slot and ran when the parent exited. That works for exactly one
//! child and makes `wait` impossible — a parent cannot wait for something that
//! only runs after it is gone.
//!
//! So there is a table now, and with it the three things that make `fork`
//! useful rather than a curiosity:
//!
//!   * **fork** — a second process, sharing memory copy-on-write.
//!   * **exec** — a process replacing its own program with another one.
//!   * **wait** — a parent stopping until a child finishes, and collecting the
//!     answer it left behind.
//!
//! That triple is how every Unix shell has started every program for fifty
//! years. `fork` alone gives you a copy of yourself, which is rarely what you
//! wanted; `exec` alone would mean a program can only ever be replaced, never
//! accompanied. Together they let one process become two and one of the two
//! become something else entirely.
//!
//! ## Taking turns
//!
//! Only one process runs at a time here, and switching happens only when one of
//! them makes a syscall that cannot be answered immediately. That is
//! *cooperative* scheduling, and it is a real design that real systems shipped
//! for years — not a placeholder. What makes it a limitation rather than a
//! choice is that a process which never calls anything can never be taken off
//! the CPU, and fixing that means letting the timer fire in ring 3.
//!
//! The mechanism is the same one `fork` already needed: a saved register frame
//! and an address space. Switching is restoring one instead of another.

use crate::memory::AddressSpace;
use crate::println;
use alloc::vec::Vec;

/// How many qwords `syscall_entry` leaves on the stack: nine saved registers,
/// then the five the CPU pushed.
pub const CONTEXT_WORDS: usize = 14;

pub const MAX_PROCESSES: usize = 8;

#[derive(Clone, Copy, PartialEq)]
pub enum State {
    Free,
    /// Has a saved context and is willing to run.
    Ready,
    /// Blocked in `wait`, and will be given the child's exit code.
    Waiting,
    /// Done. Kept in the table until a parent collects the exit code, which is
    /// exactly what a zombie is: a process that has stopped running and cannot
    /// be forgotten yet, because nobody has read its answer.
    Zombie(u64),
}

pub struct Process {
    pub state: State,
    pub root: u64,
    pub context: [u64; CONTEXT_WORDS],
    pub parent: Option<usize>,
    /// The process currently on the CPU has no saved context -- its registers
    /// are the CPU's registers.
    pub running: bool,
}

impl Process {
    const fn empty() -> Self {
        Self {
            state: State::Free,
            root: 0,
            context: [0; CONTEXT_WORDS],
            parent: None,
            running: false,
        }
    }
}

static mut TABLE: [Process; MAX_PROCESSES] = [const { Process::empty() }; MAX_PROCESSES];
static mut CURRENT: usize = 0;

fn table() -> &'static mut [Process; MAX_PROCESSES] {
    unsafe { &mut *core::ptr::addr_of_mut!(TABLE) }
}

pub fn current() -> usize {
    unsafe { core::ptr::read_volatile(core::ptr::addr_of!(CURRENT)) }
}

fn set_current(id: usize) {
    unsafe { core::ptr::write_volatile(core::ptr::addr_of_mut!(CURRENT), id) };
}

/// Forget every process. The shell calls this before starting a program.
pub fn reset() {
    for process in table().iter_mut() {
        *process = Process::empty();
    }
    set_current(0);
}

/// Register the first process, the one `exec` from the shell creates.
pub fn install_first(root: u64) -> usize {
    reset();
    let process = &mut table()[0];
    process.state = State::Ready;
    process.root = root;
    process.running = true;
    set_current(0);
    0
}

fn free_slot() -> Option<usize> {
    table().iter().position(|p| p.state == State::Free)
}

/// Record a forked child: same saved context, its own address space.
pub fn add_child(parent: usize, root: u64, context: [u64; CONTEXT_WORDS]) -> Option<usize> {
    let id = free_slot()?;
    let process = &mut table()[id];
    process.state = State::Ready;
    process.root = root;
    process.context = context;
    process.parent = Some(parent);
    process.running = false;
    Some(id)
}

/// Swap the address space under the current process, for `exec`.
pub fn replace_root(id: usize, root: u64) {
    table()[id].root = root;
}

pub fn root_of(id: usize) -> u64 {
    table()[id].root
}

/// Save the running process's registers so it can be resumed later.
pub fn save_context(id: usize, context: *const u64) {
    let process = &mut table()[id];
    for (index, slot) in process.context.iter_mut().enumerate() {
        *slot = unsafe { *context.add(index) };
    }
    process.running = false;
}

/// Mark the current process finished. Returns its parent, if it has one that
/// is waiting for exactly this.
pub fn exit_current(code: u64) -> Option<usize> {
    let id = current();
    let parent = table()[id].parent;
    table()[id].state = State::Zombie(code);
    table()[id].running = false;

    match parent {
        Some(parent) if table()[parent].state == State::Waiting => Some(parent),
        _ => None,
    }
}

/// Collect a finished child, if this process has one. Frees the slot: this is
/// where a zombie stops being one.
pub fn reap_child(parent: usize) -> Option<u64> {
    let table = table();
    for id in 0..MAX_PROCESSES {
        if table[id].parent != Some(parent) {
            continue;
        }
        if let State::Zombie(code) = table[id].state {
            table[id] = Process::empty();
            return Some(code);
        }
    }
    None
}

pub fn has_child(parent: usize) -> bool {
    table()
        .iter()
        .any(|p| p.parent == Some(parent) && p.state != State::Free)
}

/// Mark the current process as blocked in `wait`.
pub fn begin_wait(id: usize) {
    table()[id].state = State::Waiting;
}

/// The next process that could run, other than `except`.
pub fn next_ready(except: usize) -> Option<usize> {
    let table = table();
    (0..MAX_PROCESSES).find(|&id| id != except && table[id].state == State::Ready && !table[id].running)
}

/// Make `id` the running process and hand back what is needed to resume it.
pub fn take_over(id: usize) -> (u64, *const u64) {
    set_current(id);
    let process = &mut table()[id];
    process.running = true;
    process.state = State::Ready;
    (process.root, core::ptr::addr_of!(process.context) as *const u64)
}

/// Every address space still owned by a process, for cleanup when the last one
/// exits and the shell takes the machine back.
pub fn all_roots() -> Vec<u64> {
    table()
        .iter()
        .filter(|p| p.state != State::Free && p.root != 0)
        .map(|p| p.root)
        .collect()
}

/// Release every process's address space. Called once nothing is left to run.
///
/// # Safety
/// No process may still be running.
pub unsafe fn destroy_all(keep: u64) -> usize {
    let mut freed = 0;
    for root in all_roots() {
        if root == keep {
            continue;
        }
        freed += unsafe { AddressSpace::adopt(root).destroy() };
    }
    reset();
    freed
}

pub fn report() {
    println!("  pid  state      address space");
    for (id, process) in table().iter().enumerate() {
        let state = match process.state {
            State::Free => continue,
            State::Ready if process.running => "running",
            State::Ready => "ready",
            State::Waiting => "waiting",
            State::Zombie(code) => {
                println!("  {id:<4} zombie({code}) {:#x}", process.root);
                continue;
            }
        };
        println!("  {id:<4} {state:<10} {:#x}", process.root);
    }
}
