//! A text console drawn pixel by pixel.
//!
//! Until this existed, 5AM-OS was invisible without a serial cable. Everything
//! it printed went out a UART, which QEMU conveniently wires to your terminal —
//! but boot it in VirtualBox, or off a USB stick on a real laptop, and you got
//! a black screen and no way to tell whether it had crashed or was working
//! perfectly.
//!
//! The bootloader hands us a framebuffer: a slice of memory where each group of
//! bytes is one pixel on the display. Nothing draws to it on its own. There is
//! no text mode here, no cursor, no scrolling, no notion of a character — the
//! hardware knows only pixels, and everything above that is written here:
//!
//!   * a glyph is 16 bytes of bitmap, blitted one pixel at a time
//!   * a line wraps because we compare a column counter against the width
//!   * the screen scrolls because we copy the framebuffer up over itself
//!
//! That is the whole of what a terminal is, once you are below the terminal.

use crate::font;
use bootloader_api::info::{FrameBufferInfo, PixelFormat};

struct Console {
    buffer: &'static mut [u8],
    info: FrameBufferInfo,
    /// Cursor position, in characters rather than pixels.
    column: usize,
    row: usize,
}

static mut CONSOLE: Option<Console> = None;

fn console() -> Option<&'static mut Console> {
    unsafe { (*core::ptr::addr_of_mut!(CONSOLE)).as_mut() }
}

pub fn is_active() -> bool {
    unsafe { (*core::ptr::addr_of!(CONSOLE)).is_some() }
}

/// Take ownership of the bootloader's framebuffer.
///
/// # Safety
/// `buffer` must be the framebuffer the bootloader reported, and nothing else
/// may write to it afterwards.
pub unsafe fn init(buffer: &'static mut [u8], info: FrameBufferInfo) {
    unsafe {
        CONSOLE = Some(Console {
            buffer,
            info,
            column: 0,
            row: 0,
        });
    }
    clear();
}

pub fn clear() {
    if let Some(console) = console() {
        console.buffer.fill(0);
        console.column = 0;
        console.row = 0;
    }
}

/// Describe the display, for the shell.
pub fn info() -> Option<(usize, usize, usize, usize)> {
    console().map(|c| {
        (
            c.info.width,
            c.info.height,
            c.info.width / font::WIDTH,
            c.info.height / font::HEIGHT,
        )
    })
}

pub fn write_str(text: &str) {
    let Some(console) = console() else { return };
    // Iterate characters, not bytes. The kernel's own source is full of typo-
    // graphic punctuation, and walking the bytes of a multi-byte character
    // renders each one separately: an em dash came out as "???" until this
    // decoded properly.
    for ch in text.chars() {
        console.write_byte(to_ascii(ch));
    }
}

/// Fold a character onto the ASCII the font can actually draw.
///
/// A serial terminal handles UTF-8 fine, so this lives here rather than in the
/// strings themselves — the screen is the only thing that needs the compromise.
fn to_ascii(ch: char) -> u8 {
    match ch {
        '\u{2014}' | '\u{2013}' => b'-',       // em dash, en dash
        '\u{2018}' | '\u{2019}' => b'\'',      // curly single quotes
        '\u{201C}' | '\u{201D}' => b'"',       // curly double quotes
        '\u{2026}' => b'.',                    // ellipsis
        '\u{00A0}' => b' ',                    // non-breaking space
        c if c.is_ascii() => c as u8,
        _ => b'?',
    }
}

impl Console {
    fn columns(&self) -> usize {
        self.info.width / font::WIDTH
    }
    fn rows(&self) -> usize {
        self.info.height / font::HEIGHT
    }

    fn write_byte(&mut self, byte: u8) {
        match byte {
            b'\n' => self.newline(),
            b'\r' => self.column = 0,
            // Backspace has to erase, not just move: the caller draws a space
            // over the character and steps back again.
            0x08 => {
                if self.column > 0 {
                    self.column -= 1;
                }
            }
            _ => {
                if self.column >= self.columns() {
                    self.newline();
                }
                let (x, y) = (self.column * font::WIDTH, self.row * font::HEIGHT);
                self.draw_glyph(byte, x, y);
                self.column += 1;
            }
        }
    }

    fn newline(&mut self) {
        self.column = 0;
        self.row += 1;
        if self.row >= self.rows() {
            self.scroll();
            self.row = self.rows() - 1;
        }
    }

    /// Move everything up one text row and blank the last one.
    ///
    /// This is a straight memmove of nearly the whole framebuffer — on a
    /// 1280x720 display, about 2.7MB every time a line is added. Real drivers
    /// avoid it with a hardware pan or a ring buffer of rows; doing it the
    /// naive way here is honest about the cost.
    fn scroll(&mut self) {
        let stride_bytes = self.info.stride * self.info.bytes_per_pixel;
        let row_bytes = stride_bytes * font::HEIGHT;
        let total = stride_bytes * self.info.height;

        self.buffer.copy_within(row_bytes..total, 0);
        self.buffer[total - row_bytes..total].fill(0);
    }

    fn draw_glyph(&mut self, byte: u8, x: usize, y: usize) {
        let glyph = font::glyph(byte);
        for (row, bits) in glyph.iter().enumerate() {
            for column in 0..font::WIDTH {
                // Bit 7 is the leftmost pixel.
                let lit = bits & (0x80 >> column) != 0;
                if lit {
                    self.set_pixel(x + column, y + row);
                }
            }
        }
    }

    fn set_pixel(&mut self, x: usize, y: usize) {
        if x >= self.info.width || y >= self.info.height {
            return;
        }
        let offset = (y * self.info.stride + x) * self.info.bytes_per_pixel;
        if offset + self.info.bytes_per_pixel > self.buffer.len() {
            return;
        }

        // A soft green-white, so it reads as a console rather than a bug.
        const R: u8 = 0xC8;
        const G: u8 = 0xE8;
        const B: u8 = 0xC8;

        // The bootloader tells us the channel order; guessing it is how you end
        // up with a blue-tinted display on half of all hardware.
        let pixel = &mut self.buffer[offset..offset + self.info.bytes_per_pixel];
        match self.info.pixel_format {
            PixelFormat::Rgb => {
                pixel[0] = R;
                pixel[1] = G;
                pixel[2] = B;
            }
            PixelFormat::Bgr => {
                pixel[0] = B;
                pixel[1] = G;
                pixel[2] = R;
            }
            PixelFormat::U8 => {
                // Greyscale: use luminance rather than an arbitrary channel.
                pixel[0] = ((R as u16 * 30 + G as u16 * 59 + B as u16 * 11) / 100) as u8;
            }
            _ => {
                pixel.fill(0xC0);
            }
        }
    }
}
