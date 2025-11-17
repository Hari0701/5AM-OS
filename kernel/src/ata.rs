//! Reading sectors off an IDE disk, one at a time, through the CPU.
//!
//! This is the oldest way to talk to a hard disk that still works, and it is
//! deliberately the one implemented here: no DMA, no interrupts, no queueing.
//! Ask for a sector, then sit in a loop reading a status port until the drive
//! says the data is ready, then pull 256 words through a single 16-bit port.
//! The CPU personally carries every byte. That is what "programmed I/O" means,
//! and why every real driver stopped doing it.
//!
//! What it costs is visible: 512 bytes per round trip, with the processor doing
//! nothing else in between. What it buys is that the whole driver is one page of
//! code with no allocation, no setup and no shared state — you can read it and
//! know exactly what the machine does.
//!
//! ## Which disk
//!
//! The IDE primary bus can carry two drives, master and slave, sharing one set
//! of ports. `run.sh` gives QEMU the boot image as the master and the
//! filesystem image as the slave, so everything here talks to drive 1.

use crate::serial::{inb, outb};
use core::arch::asm;

const DATA: u16 = 0x1F0;
const SECTOR_COUNT: u16 = 0x1F2;
const LBA_LOW: u16 = 0x1F3;
const LBA_MID: u16 = 0x1F4;
const LBA_HIGH: u16 = 0x1F5;
const DRIVE: u16 = 0x1F6;
const STATUS: u16 = 0x1F7;
const COMMAND: u16 = 0x1F7;
/// A second, read-only view of the status register. Reading the real one
/// acknowledges an interrupt as a side effect; this one does not.
const ALT_STATUS: u16 = 0x3F6;

const STATUS_BUSY: u8 = 1 << 7;
const STATUS_READY: u8 = 1 << 6;
const STATUS_DRQ: u8 = 1 << 3;
const STATUS_ERROR: u8 = 1 << 0;

const COMMAND_READ_SECTORS: u8 = 0x20;

pub const SECTOR_SIZE: usize = 512;

/// The drive we keep the filesystem on: primary bus, slave.
const DISK: u8 = 1;

/// Read one 16-bit word from a port. The data register is the only place in
/// this kernel that is not byte-wide.
unsafe fn inw(port: u16) -> u16 {
    let value: u16;
    unsafe { asm!("in ax, dx", out("ax") value, in("dx") port, options(nomem, nostack, preserves_flags)) };
    value
}

/// Waste roughly 400ns.
///
/// After selecting a drive the controller needs time before its status register
/// means anything. There is no "ready to be asked whether you are ready" bit,
/// so the documented fix is to read the alternate status four times and throw
/// the answers away — each read takes about 100ns on the ISA bus.
unsafe fn settle() {
    for _ in 0..4 {
        unsafe { inb(ALT_STATUS) };
    }
}

/// Wait for BSY to clear, then report whether the drive is willing to talk.
unsafe fn wait_ready() -> Result<u8, &'static str> {
    // Bounded, because a missing drive floats the bus high and BSY never
    // clears -- an unbounded loop here hangs the machine at boot, which is a
    // miserable way to discover you forgot a -drive flag.
    for _ in 0..1_000_000 {
        let status = unsafe { inb(STATUS) };
        if status == 0xFF {
            return Err("no drive on this bus");
        }
        if status & STATUS_BUSY == 0 {
            if status & STATUS_ERROR != 0 {
                return Err("the drive reported an error");
            }
            return Ok(status);
        }
    }
    Err("the drive never became ready")
}

/// Read `buffer.len() / 512` sectors starting at logical block `lba`.
///
/// LBA is the abstraction that killed cylinders, heads and sectors: the disk is
/// an array of 512-byte blocks numbered from zero, and where they physically
/// live is the drive's problem. This uses 28-bit LBA, which tops out at 128 GiB
/// and is more than enough for a 16 MiB image.
pub fn read(lba: u32, buffer: &mut [u8]) -> Result<(), &'static str> {
    if buffer.len() % SECTOR_SIZE != 0 {
        return Err("buffer is not a whole number of sectors");
    }
    let count = buffer.len() / SECTOR_SIZE;
    if count == 0 {
        return Ok(());
    }
    if count > 256 {
        return Err("too many sectors for one request");
    }

    unsafe {
        // Drive select carries the top four bits of the address, which is the
        // kind of detail that tells you this interface grew rather than being
        // designed.
        outb(DRIVE, 0xE0 | (DISK << 4) | ((lba >> 24) & 0x0F) as u8);
        settle();

        outb(SECTOR_COUNT, if count == 256 { 0 } else { count as u8 });
        outb(LBA_LOW, lba as u8);
        outb(LBA_MID, (lba >> 8) as u8);
        outb(LBA_HIGH, (lba >> 16) as u8);
        outb(COMMAND, COMMAND_READ_SECTORS);

        for sector in 0..count {
            let status = wait_ready()?;
            if status & STATUS_DRQ == 0 {
                return Err("the drive has no data to give");
            }

            // 256 words, not 512 bytes: the data port is 16 bits wide, and
            // reading it byte-wise would return the same low half twice.
            let base = sector * SECTOR_SIZE;
            for word in 0..SECTOR_SIZE / 2 {
                let value = inw(DATA);
                buffer[base + word * 2] = value as u8;
                buffer[base + word * 2 + 1] = (value >> 8) as u8;
            }
        }
    }

    Ok(())
}

/// Is there anything on the slave slot of the primary bus?
///
/// Cheap enough to call before every filesystem operation, and it means a
/// kernel booted without the second image says so instead of hanging.
pub fn present() -> bool {
    unsafe {
        outb(DRIVE, 0xE0 | (DISK << 4));
        settle();
        let status = inb(STATUS);
        status != 0 && status != 0xFF && status & STATUS_READY != 0
    }
}

/// Write one 16-bit word to a port.
unsafe fn outw(port: u16, value: u16) {
    unsafe {
        asm!("out dx, ax", in("dx") port, in("ax") value, options(nomem, nostack, preserves_flags))
    };
}

const COMMAND_WRITE_SECTORS: u8 = 0x30;
const COMMAND_FLUSH_CACHE: u8 = 0xE7;

/// Write whole sectors starting at logical block `lba`.
///
/// The mirror image of `read`, with one addition that is easy to leave out and
/// impossible to notice in an emulator: the cache flush at the end.
///
/// A drive is allowed to report a write complete the moment the bytes reach its
/// own buffer, and to reorder what it does with them afterwards. Without the
/// flush, "the file is written" means "the drive has agreed to write it", and a
/// power cut in between loses data the kernel already promised was safe. QEMU
/// will never show you this. A real disk will, once.
pub fn write(lba: u32, buffer: &[u8]) -> Result<(), &'static str> {
    if buffer.len() % SECTOR_SIZE != 0 {
        return Err("buffer is not a whole number of sectors");
    }
    let count = buffer.len() / SECTOR_SIZE;
    if count == 0 {
        return Ok(());
    }
    if count > 256 {
        return Err("too many sectors for one request");
    }

    unsafe {
        outb(DRIVE, 0xE0 | (DISK << 4) | ((lba >> 24) & 0x0F) as u8);
        settle();

        outb(SECTOR_COUNT, if count == 256 { 0 } else { count as u8 });
        outb(LBA_LOW, lba as u8);
        outb(LBA_MID, (lba >> 8) as u8);
        outb(LBA_HIGH, (lba >> 16) as u8);
        outb(COMMAND, COMMAND_WRITE_SECTORS);

        for sector in 0..count {
            let status = wait_ready()?;
            if status & STATUS_DRQ == 0 {
                return Err("the drive will not accept data");
            }

            let base = sector * SECTOR_SIZE;
            for word in 0..SECTOR_SIZE / 2 {
                let low = buffer[base + word * 2] as u16;
                let high = buffer[base + word * 2 + 1] as u16;
                outw(DATA, low | (high << 8));
            }
        }

        // Tell the drive to actually commit what it just accepted.
        outb(COMMAND, COMMAND_FLUSH_CACHE);
        wait_ready()?;
    }

    Ok(())
}
