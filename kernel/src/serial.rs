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

#[doc(hidden)]
pub fn _print(args: fmt::Arguments) {
    use fmt::Write;
    // SAFETY: single-threaded, interrupts still disabled. See CONSOLE above.
    unsafe {
        let console = &mut *core::ptr::addr_of_mut!(CONSOLE);
        let _ = console.write_fmt(args);
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
