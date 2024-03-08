//! The 16550 UART — the oldest and most reliable way for a kernel to speak.
//!
//! Why start here instead of the screen? A screen needs a framebuffer, a font,
//! glyph rasterising and a cursor before it can say one word. A serial port
//! needs one byte written to one port. When something goes wrong at boot, this
//! is the channel that still works, which is exactly why real kernels keep a
//! serial console long after they have graphics.
//!
//! QEMU wires COM1 straight to our terminal with `-serial stdio`, so anything
//! written here appears in the shell that launched the VM.

use core::arch::asm;
use core::fmt;

/// COM1. The PC's four serial ports live at fixed I/O addresses that have not
/// moved since the IBM PC in 1981.
const COM1: u16 = 0x3F8;

/// COM2. A second, independent UART — the kernel's channel to the AI bridge.
///
/// Using a separate port rather than multiplexing COM1 means the protocol never
/// has to be untangled from console output, and a bridge that is not attached
/// simply times out instead of corrupting what you are reading.
pub const COM2: u16 = 0x2F8;

// Register offsets from the port base. Several of these registers change
// meaning depending on the DLAB bit below, which is a piece of 1970s hardware
// frugality we still live with.
const DATA: u16 = 0; // read/write a byte (when DLAB=0)
const INT_ENABLE: u16 = 1; // interrupt enable  (when DLAB=0)
const DIVISOR_LO: u16 = 0; // baud divisor low  (when DLAB=1)
const DIVISOR_HI: u16 = 1; // baud divisor high (when DLAB=1)
const FIFO_CTRL: u16 = 2;
const LINE_CTRL: u16 = 3;
const MODEM_CTRL: u16 = 4;
const LINE_STATUS: u16 = 5;

/// Write one byte to an I/O port.
///
/// x86 has a second address space, separate from memory, reached only by the
/// `in`/`out` instructions. `outb` is one machine instruction; there is no way
/// to express it in safe Rust, which is exactly the kind of place `unsafe`
/// exists for.
///
/// # Safety
/// Writing to an arbitrary port can reconfigure or wedge hardware. The caller
/// must know what device answers at `port`.
pub unsafe fn outb(port: u16, value: u8) {
    unsafe {
        asm!("out dx, al", in("dx") port, in("al") value, options(nomem, nostack, preserves_flags));
    }
}

/// Read one byte from an I/O port.
///
/// # Safety
/// Reading a port can have side effects on the device. Same contract as [`outb`].
pub unsafe fn inb(port: u16) -> u8 {
    let value: u8;
    unsafe {
        asm!("in al, dx", out("al") value, in("dx") port, options(nomem, nostack, preserves_flags));
    }
    value
}

pub struct SerialPort {
    base: u16,
}

impl SerialPort {
    pub const fn new(base: u16) -> Self {
        Self { base }
    }

    /// Bring the UART up. Every line here is a register write to a real chip.
    ///
    /// # Safety
    /// Must only be called for a base address that is actually a 16550 UART.
    pub unsafe fn init(&mut self) {
        unsafe {
            // 1. Silence the chip while we reconfigure it. An interrupt fired
            //    mid-setup would arrive before we have an interrupt handler.
            outb(self.base + INT_ENABLE, 0x00);

            // 2. Set DLAB (Divisor Latch Access Bit). This *remaps* the first
            //    two registers: they stop being data/interrupt and become the
            //    two halves of the baud divisor. One bit, two register layouts.
            outb(self.base + LINE_CTRL, 0x80);

            // 3. Divisor 3: 115200 / 3 = 38400 baud. The UART's clock is fixed,
            //    so you pick a speed by dividing it.
            outb(self.base + DIVISOR_LO, 0x03);
            outb(self.base + DIVISOR_HI, 0x00);

            // 4. Clear DLAB and set the line format in the same write:
            //    0x03 = 8 data bits, no parity, 1 stop bit — "8N1", the default
            //    everyone has assumed for forty years.
            outb(self.base + LINE_CTRL, 0x03);

            // 5. Enable and clear the FIFOs, interrupt at 14 bytes queued.
            outb(self.base + FIFO_CTRL, 0xC7);

            // 6. Assert DTR and RTS: tell the other end we are here and ready.
            outb(self.base + MODEM_CTRL, 0x0B);
        }
    }

    /// True when the transmit holding register is empty and will accept a byte.
    fn can_send(&self) -> bool {
        // Bit 5 of the line status register. We poll it because we have no
        // interrupt handling yet — this is a busy-wait, and it is fine at boot.
        unsafe { inb(self.base + LINE_STATUS) & 0x20 != 0 }
    }

    /// Ask the UART to raise an interrupt whenever a byte arrives.
    ///
    /// Without this the port is write-only in practice: input would have to be
    /// polled, and anything typed between polls sits in a 16-byte FIFO that
    /// silently discards the overflow. Paste a line into the terminal and you
    /// would lose most of it.
    ///
    /// # Safety
    /// The caller must have installed a handler for this port's IRQ first.
    pub unsafe fn enable_receive_interrupt(&mut self) {
        // Bit 0 of the interrupt-enable register: "data available".
        unsafe { outb(self.base + INT_ENABLE, 0x01) };
    }

    /// True when a byte has arrived and is waiting to be read.
    fn has_data(&self) -> bool {
        // Bit 0 of the line status register: "data ready".
        unsafe { inb(self.base + LINE_STATUS) & 1 != 0 }
    }

    /// Take one byte if the other end has sent one. Never blocks.
    pub fn try_recv(&mut self) -> Option<u8> {
        if self.has_data() {
            Some(unsafe { inb(self.base + DATA) })
        } else {
            None
        }
    }

    /// Write a byte without the CRLF translation `send` does.
    ///
    /// The console translates newlines for the benefit of a terminal. A protocol
    /// must not: an extra carriage return would corrupt every framed message.
    pub fn send_raw(&mut self, byte: u8) {
        while !self.can_send() {
            core::hint::spin_loop();
        }
        unsafe { outb(self.base + DATA, byte) };
    }

    pub fn send(&mut self, byte: u8) {
        // A terminal expects CRLF; a kernel emits LF. Translate, or every line
        // after the first starts wherever the previous one ended.
        if byte == b'\n' {
            while !self.can_send() {
                core::hint::spin_loop();
            }
            unsafe { outb(self.base + DATA, b'\r') };
        }
        while !self.can_send() {
            core::hint::spin_loop();
        }
        unsafe { outb(self.base + DATA, byte) };
    }
}

/// Lets us use `write!` and friends against the serial port.
impl fmt::Write for SerialPort {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for byte in s.bytes() {
            self.send(byte);
        }
        Ok(())
    }
}

/// The one console the kernel owns.
///
/// A `static mut` is the crudest possible answer to "who owns this device", and
/// it is deliberate: at this stage there is exactly one CPU running exactly one
/// thread, so there is nobody to race with. The moment we enable interrupts and
/// a second core, this becomes a spinlock — and that change is the whole lesson
/// about why kernels need locks at all.
pub static mut CONSOLE: SerialPort = SerialPort::new(COM1);

/// The AI bridge channel. Silent unless something is listening on the other end.
pub static mut BRIDGE: SerialPort = SerialPort::new(COM2);

/// Bytes that arrived on the console port, filled by the IRQ4 handler.
///
/// Same shape as the keyboard queue in keyboard.rs, and volatile for the same
/// reason: the compiler cannot see that an interrupt writes to it, so without
/// `read_volatile` it caches the indices and the shell decides, permanently,
/// that no input ever arrived.
const INPUT_CAPACITY: usize = 256;

struct InputQueue {
    buffer: [u8; INPUT_CAPACITY],
    read: usize,
    write: usize,
}

static mut INPUT: InputQueue = InputQueue {
    buffer: [0; INPUT_CAPACITY],
    read: 0,
    write: 0,
};

/// Called from the serial interrupt handler.
pub fn push_input(byte: u8) {
    unsafe {
        let queue = core::ptr::addr_of_mut!(INPUT);
        let write = core::ptr::read_volatile(&raw const (*queue).write);
        let read = core::ptr::read_volatile(&raw const (*queue).read);
        let next = (write + 1) % INPUT_CAPACITY;
        if next != read {
            core::ptr::write_volatile(&raw mut (*queue).buffer[write], byte);
            core::ptr::write_volatile(&raw mut (*queue).write, next);
        }
    }
}

/// Take one byte typed on the console, if any is waiting.
pub fn read_input() -> Option<u8> {
    crate::interrupts::without_interrupts(|| unsafe {
        let queue = core::ptr::addr_of_mut!(INPUT);
        let read = core::ptr::read_volatile(&raw const (*queue).read);
        let write = core::ptr::read_volatile(&raw const (*queue).write);
        if read == write {
            return None;
        }
        let value = core::ptr::read_volatile(&raw const (*queue).buffer[read]);
        core::ptr::write_volatile(&raw mut (*queue).read, (read + 1) % INPUT_CAPACITY);
        Some(value)
    })
}

/// Drain and discard whatever the UART is holding.
///
/// The FIFO can contain bytes from before the handler existed; without this
/// the controller may also never raise the first interrupt, exactly as with
/// the keyboard controller.
///
/// # Safety
/// Talks directly to COM1.
pub unsafe fn drain_console() {
    unsafe {
        let console = &mut *core::ptr::addr_of_mut!(CONSOLE);
        let mut drained = 0;
        while console.try_recv().is_some() && drained < 64 {
            drained += 1;
        }
    }
}

#[doc(hidden)]
pub fn _print(args: fmt::Arguments) {
    use fmt::Write;
    // SAFETY: single-threaded, interrupts still disabled. See CONSOLE above.
    unsafe {
        let console = &mut *core::ptr::addr_of_mut!(CONSOLE);
        let _ = console.write_fmt(args);
    }

    // Everything also goes to the screen, when there is one. Serial stays the
    // primary channel because it works before the framebuffer is mapped and
    // keeps working when the display does not -- but a machine booted off a USB
    // stick with no serial cable now shows its output.
    if crate::framebuffer::is_active() {
        let mut screen = ScreenWriter;
        let _ = screen.write_fmt(args);
    }
}

/// Adapter so the same `format_args!` can be replayed to the framebuffer.
struct ScreenWriter;

impl fmt::Write for ScreenWriter {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        crate::framebuffer::write_str(s);
        Ok(())
    }
}

#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => ($crate::serial::_print(format_args!($($arg)*)));
}

#[macro_export]
macro_rules! println {
    () => ($crate::print!("\n"));
    ($($arg:tt)*) => ($crate::print!("{}\n", format_args!($($arg)*)));
}
