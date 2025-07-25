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
use crate::serial::{inb, outb};

/// PS/2 controller ports.
const DATA: u16 = 0x60;
const STATUS: u16 = 0x64; // read
const COMMAND: u16 = 0x64; // write

/// Wake the keyboard up and empty its output buffer.
///
/// This is the step whose absence looks like broken hardware. The controller
/// raises IRQ1 only when a byte lands in an *empty* output buffer. If the
/// firmware left one sitting there, the buffer is already full, no interrupt is
/// ever raised, and every key you press vanishes with no error anywhere.
///
/// # Safety
/// Talks directly to the PS/2 controller. Call once, before enabling IRQ1.
pub unsafe fn init() {
    unsafe {
        // Enable the first PS/2 port. Usually already on after BIOS, but the
        // bootloader is entitled to have turned it off.
        outb(COMMAND, 0xAE);

        // Drain whatever is stale. Status bit 0 = output buffer full.
        let mut drained = 0;
        while inb(STATUS) & 1 != 0 && drained < 16 {
            let _ = inb(DATA);
            drained += 1;
        }

        // Now the part that unmasking the PIC does not cover.
        //
        // The controller has its own configuration byte, and bit 0 of it is
        // "raise IRQ1 when port 1 has data". If that bit is clear the keyboard
        // is silent no matter what the PIC thinks, which looks exactly like
        // broken hardware: keys go in, nothing comes out, no fault, no clue.
        outb(COMMAND, 0x20); // "give me the configuration byte"
        let mut config = read_data();
        config |= 1 << 0; // port 1 interrupt enable
        config &= !(1 << 4); // clear "port 1 clock disabled"
        outb(COMMAND, 0x60); // "here is a new configuration byte"
        write_data(config);

        // Tell the keyboard itself to start scanning.
        write_data(0xF4);
        let _ = read_data();
    }
}

/// Wait for the controller to have a byte for us, then take it.
unsafe fn read_data() -> u8 {
    unsafe {
        let mut spins = 0;
        while inb(STATUS) & 1 == 0 && spins < 100_000 {
            spins += 1;
            core::hint::spin_loop();
        }
        inb(DATA)
    }
}

/// Wait for the controller's input buffer to drain, then write.
///
/// Status bit 1 means "input buffer full — do not write yet". Writing anyway
/// silently loses the byte.
unsafe fn write_data(value: u8) {
    unsafe {
        let mut spins = 0;
        while inb(STATUS) & (1 << 1) != 0 && spins < 100_000 {
            spins += 1;
            core::hint::spin_loop();
        }
        outb(DATA, value);
    }
}

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
        let queue = core::ptr::addr_of_mut!(QUEUE);
        let write = core::ptr::read_volatile(&raw const (*queue).write);
        let read = core::ptr::read_volatile(&raw const (*queue).read);
        let next = (write + 1) % CAPACITY;
        if next != read {
            core::ptr::write_volatile(&raw mut (*queue).buffer[write], scancode);
            core::ptr::write_volatile(&raw mut (*queue).write, next);
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

        // Volatile is not optional here, and the reason is worth understanding.
        //
        // The compiler analyses this loop and sees nothing that writes to the
        // queue, so it is entitled to read `write` once and reuse that value
        // forever. It cannot see the interrupt handler, because nothing in the
        // program *calls* it — the hardware does. The result is a shell that
        // hangs with a full input buffer, having decided in advance that no
        // input would ever arrive.
        //
        // `read_volatile` says: this memory changes for reasons you cannot see,
        // load it every single time.
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
/// The channel tasks sleep on while waiting for a keystroke.
///
/// A fixed number rather than an address, because both the IRQ handler and the
/// shell need it and neither owns the other.
pub const INPUT_CHANNEL: u64 = 0x5A4D_0001;

/// Wait for a key, sleeping rather than spinning.
///
/// The old shell loop polled and halted, which parks the CPU but keeps the task
/// runnable -- so with anything else running, the scheduler kept handing it
/// slices to discover, again, that nothing had been typed. Blocking takes it
/// out of the rotation entirely until a key actually arrives.
pub fn wait_key() -> Key {
    // Test and block as one uninterruptible step. A keystroke arriving between
    // them would wake nobody, and the shell would then sleep until the *next*
    // key -- one keypress permanently behind, forever.
    crate::task::block_until(INPUT_CHANNEL, next_key)
}

/// A key from either console: the serial line or the PS/2 keyboard.
///
/// Both are real inputs and the shell must not care which one you used. Serial
/// is checked first only because it is the one a remote user is on, and a
/// dropped keystroke there is harder to notice.
pub fn next_key() -> Option<Key> {
    if let Some(byte) = crate::serial::read_input() {
        return match byte {
            b'\r' | b'\n' => Some(Key::Enter),
            // Terminals disagree: most send DEL for backspace, some send BS.
            0x08 | 0x7F => Some(Key::Backspace),
            // Printable ASCII. Escape sequences (arrow keys and the like) begin
            // with 0x1B and are dropped rather than pasted in as garbage.
            0x20..=0x7E => Some(Key::Char(byte as char)),
            _ => None,
        };
    }
    read_key()
}

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
