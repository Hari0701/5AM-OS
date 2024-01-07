//! The kernel's channel to a language model.
//!
//! What is actually happening here, stated plainly: **the model does not run
//! inside 5AM-OS.** A kernel with no filesystem, no allocator and no network
//! stack cannot load a multi-gigabyte model, and nothing in this repository
//! pretends otherwise.
//!
//! What the kernel does is talk. It writes a question — plus the machine state
//! that makes the question answerable — out of COM2, and reads the reply back
//! in. On the other end of that wire is `bridge/bridge.py`, running on your
//! machine, which calls the Claude API and writes the answer back.
//!
//! That split is the honest version of an "AI OS" at this stage, and it is not
//! a toy: the serial port is the only I/O the kernel has, so this is genuinely
//! the whole of its ability to reach the outside world. When 5AM-OS eventually
//! grows a virtio-net driver and a TCP stack, the bridge disappears and this
//! module talks to the API directly. The protocol below is deliberately shaped
//! so that change touches nothing above it.
//!
//! ## Protocol
//!
//! Faults are NOT sent here — `oracle.rs` decodes those inside the kernel,
//! because a diagnosis you can trust beats one that needs a network.
//!
//! Request (kernel -> bridge), plain text, LF-terminated lines:
//!
//! ```text
//! 5AMOS/1 ASK
//! state: <live machine registers>
//! q: <the question>
//! END
//! ```
//!
//! Reply (bridge -> kernel): any number of lines, terminated by a line `END`.
//! The kernel prints each byte as it arrives, so the answer streams.

use crate::interrupts;
use crate::serial::{self, SerialPort};
use crate::{print, println};
use core::arch::asm;
use core::fmt::Write;

/// How long to wait for the bridge, in PIT ticks (~18.2 per second).
///
/// A real API call takes seconds, so the timeout has to be generous.
const REPLY_TIMEOUT_TICKS: u64 = 18 * 90;

/// Fallback timeout for when the tick counter is not advancing.
///
/// Inside a fault handler interrupts are disabled, so no timer tick will ever
/// arrive and a tick deadline would hang forever. A spin count is the crude but
/// universal alternative — it measures host speed rather than time, which is
/// exactly why it is the fallback and not the primary.
const REPLY_TIMEOUT_SPINS: u64 = 60_000_000;

/// Are interrupts enabled? Decides which of the two clocks above we can trust.
fn interrupts_enabled() -> bool {
    let flags: u64;
    unsafe { asm!("pushfq; pop {}", out(reg) flags, options(nomem, preserves_flags)) };
    flags & (1 << 9) != 0
}

/// # Safety
/// Configures COM2. Call once at boot.
pub unsafe fn init() {
    unsafe {
        let bridge = &mut *core::ptr::addr_of_mut!(serial::BRIDGE);
        bridge.init();
    }
}

fn bridge() -> &'static mut SerialPort {
    // SAFETY: single-threaded, and every caller holds the CPU for the duration
    // of one request/response exchange.
    unsafe { &mut *core::ptr::addr_of_mut!(serial::BRIDGE) }
}

fn write_line(port: &mut SerialPort, text: &str) {
    for byte in text.bytes() {
        port.send_raw(byte);
    }
    port.send_raw(b'\n');
}

/// Ask a question, with the live machine state attached.
pub fn ask(question: &str) {
    let state = StateLine::capture();
    exchange("5AMOS/1 ASK", state.as_str(), question);
}

fn exchange(verb: &str, state: &str, question: &str) {
    let port = bridge();

    println!();
    println!("[ai  ] asking the bridge ...");

    // Drain anything stale so a previous timed-out reply cannot be mistaken for
    // the answer to this question.
    while port.try_recv().is_some() {}

    write_line(port, verb);
    write_line(port, state);
    for chunk in ["q: ", question] {
        for byte in chunk.bytes() {
            port.send_raw(byte);
        }
    }
    port.send_raw(b'\n');
    write_line(port, "END");

    read_reply(port);
}

/// Read the reply, printing it as it arrives, until a lone `END` line.
fn read_reply(port: &mut SerialPort) {
    // Two clocks, because the two callers run in different worlds: the shell
    // has interrupts on and a ticking timer, a fault handler has neither.
    let use_ticks = interrupts_enabled();
    let deadline = interrupts::ticks() + REPLY_TIMEOUT_TICKS;
    let mut spins = 0u64;
    // Tracks how much of a bare "END\n" line we have seen, so the terminator is
    // recognised without buffering the whole reply.
    let mut line_start = true;
    let mut matched = 0usize;
    const TERMINATOR: &[u8] = b"END";

    loop {
        match port.try_recv() {
            Some(byte) => {
                spins = 0;

                if line_start && matched < TERMINATOR.len() && byte == TERMINATOR[matched] {
                    matched += 1;
                    continue; // hold it back until we know it is not the terminator
                }
                if matched == TERMINATOR.len() && (byte == b'\n' || byte == b'\r') {
                    println!();
                    return;
                }

                // Not the terminator after all — emit what we held back.
                for held in &TERMINATOR[..matched] {
                    print!("{}", *held as char);
                }
                matched = 0;

                if byte == b'\r' {
                    continue;
                }
                line_start = byte == b'\n';
                print!("{}", byte as char);
            }
            None => {
                spins += 1;
                let expired = if use_ticks {
                    interrupts::ticks() > deadline
                } else {
                    spins > REPLY_TIMEOUT_SPINS
                };
                if expired {
                    println!();
                    println!("[ai  ] no answer from the bridge.");
                    println!("       Start it with:  python3 bridge/bridge.py");
                    println!("       It needs ANTHROPIC_API_KEY set in its environment.");
                    return;
                }
                core::hint::spin_loop();
            }
        }
    }
}

/// A fixed-size line buffer for the machine state.
///
/// No allocator exists, so this is a stack array with a cursor — the no_std
/// substitute for `String`, and the reason `write!` works here at all.
struct StateLine {
    buffer: [u8; 320],
    len: usize,
}

impl StateLine {
    fn capture() -> Self {
        let mut line = Self {
            buffer: [0; 320],
            len: 0,
        };

        let (cr0, cr2, cr3, cr4, cs, rsp): (u64, u64, u64, u64, u16, u64);
        unsafe {
            asm!(
                "mov {cr0}, cr0",
                "mov {cr2}, cr2",
                "mov {cr3}, cr3",
                "mov {cr4}, cr4",
                "mov {cs:x}, cs",
                "mov {rsp}, rsp",
                cr0 = out(reg) cr0,
                cr2 = out(reg) cr2,
                cr3 = out(reg) cr3,
                cr4 = out(reg) cr4,
                cs = out(reg) cs,
                rsp = out(reg) rsp,
                options(nomem, nostack, preserves_flags),
            );
        }

        let _ = write!(
            line,
            "state: cr0={cr0:#x} cr2={cr2:#x} cr3={cr3:#x} cr4={cr4:#x} cs={cs:#x} rsp={rsp:#x} ring={} ticks={}",
            cs & 0b11,
            interrupts::ticks(),
        );
        line
    }

    fn as_str(&self) -> &str {
        core::str::from_utf8(&self.buffer[..self.len]).unwrap_or("state: unavailable")
    }
}

impl Write for StateLine {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        for byte in s.bytes() {
            if self.len < self.buffer.len() {
                self.buffer[self.len] = byte;
                self.len += 1;
            }
        }
        Ok(())
    }
}
