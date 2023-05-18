//! PS/2 keyboard: raw scancodes in, characters out.
//!
//! The keyboard does not send you letters. It sends a number per *physical key
//! event* — one when a key goes down, a different one when it comes up. What
//! that key "means" is entirely the operating system's opinion, which is why
//! layouts are software and why this file has to exist.
//!
//! This is scancode set 1, the original IBM PC encoding, which the keyboard
//! controller still emulates by default forty years later.

use crate::interrupts::without_interrupts;

/// A ring buffer between the interrupt handler and the shell.
///
/// The handler must be fast and must never block, so it does the least possible
/// work: drop the byte in here and leave. The shell picks it up later. This
/// split — minimal work in the handler, real work outside it — is one of the
/// central patterns of kernel design.
const CAPACITY: usize = 128;

struct ScancodeQueue {
    buffer: [u8; CAPACITY],
    read: usize,
    write: usize,
}

static mut QUEUE: ScancodeQueue = ScancodeQueue {
    buffer: [0; CAPACITY],
    read: 0,
    write: 0,
};

/// Called from the keyboard interrupt handler.
///
/// If the buffer is full we drop the byte rather than overwrite unread input:
/// losing the newest keystroke is less confusing than losing an older one.
pub fn push_scancode(scancode: u8) {
    unsafe {
        let queue = &mut *core::ptr::addr_of_mut!(QUEUE);
        let next = (queue.write + 1) % CAPACITY;
        if next != queue.read {
            queue.buffer[queue.write] = scancode;
            queue.write = next;
        }
    }
}

/// Called from normal kernel code.
///
/// Wrapped in `without_interrupts` because the handler can fire between our
/// read of `read` and our write of it, which would corrupt the queue. This is
/// the smallest real example of why kernels need critical sections.
fn pop_scancode() -> Option<u8> {
    without_interrupts(|| unsafe {
        let queue = core::ptr::addr_of_mut!(QUEUE);

        let read = core::ptr::read_volatile(&raw const (*queue).read);
        let write = core::ptr::read_volatile(&raw const (*queue).write);
        if read == write {
            return None;
        }
        let value = core::ptr::read_volatile(&raw const (*queue).buffer[read]);
        core::ptr::write_volatile(&raw mut (*queue).read, (read + 1) % CAPACITY);
        Some(value)
    })
}

/// Modifier state. Shift is not a character; it changes what other keys mean.
static mut SHIFT: bool = false;

/// What a key event turned into.
pub enum Key {
    Char(char),
    Enter,
    Backspace,
}

/// Pull one decoded key, if any input is waiting.
pub fn read_key() -> Option<Key> {
    loop {
        let scancode = pop_scancode()?;

        // Bit 7 set means "key released". Most of the time we ignore releases —
        // except for shift, where the release is exactly the interesting part.
        let released = scancode & 0x80 != 0;
        let code = scancode & 0x7F;

        if code == 0x2A || code == 0x36 {
            unsafe { SHIFT = !released };
            continue;
        }
        if released {
            continue;
        }

        let shift = unsafe { SHIFT };
        match code {
            0x1C => return Some(Key::Enter),
            0x0E => return Some(Key::Backspace),
            _ => {
                if let Some(c) = translate(code, shift) {
                    return Some(Key::Char(c));
                }
            }
        }
    }
}

/// Scancode set 1 -> US QWERTY.
///
/// This table *is* the keyboard layout. Swap it and you have a different one;
/// there is nothing more to it than this at the kernel level.
fn translate(code: u8, shift: bool) -> Option<char> {
    let unshifted = b"\0\x1b1234567890-=\x08\tqwertyuiop[]\n\0asdfghjkl;'`\0\\zxcvbnm,./";
    let shifted = b"\0\x1b!@#$%^&*()_+\x08\tQWERTYUIOP{}\n\0ASDFGHJKL:\"~\0|ZXCVBNM<>?";

    let table = if shift { shifted } else { unshifted };
    let index = code as usize;

    if code == 0x39 {
        return Some(' ');
    }
    if index >= table.len() {
        return None;
    }
    match table[index] {
        0 => None,
        byte => Some(byte as char),
    }
}
