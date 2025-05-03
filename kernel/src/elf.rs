//! Reading an ELF file, which is the last thing standing between this kernel
//! and running programs it did not compile.
//!
//! The previous version of `user` copied a fixed number of bytes from a naked
//! function into a page and jumped at it. That works exactly once, for exactly
//! one program, written under a rule the compiler never agreed to. A loader
//! removes all three limits: the file says where it wants to live, how much
//! space it needs, and what permissions each part should have, and the kernel
//! obeys or refuses.
//!
//! ## The part that surprises people
//!
//! An ELF file has two completely separate tables of contents. **Sections**
//! (`.text`, `.rodata`, `.bss`) are for the linker, and a loader may ignore
//! them entirely — a stripped binary has none and still runs. **Segments**,
//! described by program headers, are for the loader, and they are all that
//! matters here. Loading a program is: walk the program headers, and for each
//! one that says PT_LOAD, put those bytes at that address with those
//! permissions.
//!
//! That is the whole algorithm. Everything else in this file is checking.
//!
//! ## memsz vs filesz
//!
//! A segment may ask for more memory than the file provides. The difference is
//! `.bss` — variables that are zero at startup, which would be a waste of disk
//! to store as actual zeroes. The loader owes the program that memory, zeroed.
//!
//! "Zeroed" is not a nicety. The frames being handed out held something before,
//! and skipping the clear would leak whatever it was into a program that is not
//! supposed to see it, while looking like it worked.

use crate::memory::{self, PAGE_SIZE};
use alloc::vec::Vec;

const MAGIC: [u8; 4] = [0x7F, b'E', b'L', b'F'];
const CLASS_64: u8 = 2;
const LITTLE_ENDIAN: u8 = 1;
const TYPE_EXECUTABLE: u16 = 2;
const MACHINE_X86_64: u16 = 0x3E;

const PT_LOAD: u32 = 1;
const PF_X: u32 = 1;
const PF_W: u32 = 2;
const PF_R: u32 = 4;

/// What a loaded program needs in order to be started.
pub struct Loaded {
    pub entry: u64,
    pub segments: usize,
    pub bytes_zeroed: u64,
    /// Every page this load mapped, so the caller can give them back.
    ///
    /// Somebody has to remember. A loader that maps and forgets leaves the
    /// frames unreachable and still allocated for as long as the machine runs.
    pub pages: Vec<u64>,
}

/// One program header, exactly as it appears on disk.
#[repr(C)]
#[derive(Clone, Copy)]
struct ProgramHeader {
    kind: u32,
    flags: u32,
    offset: u64,
    virtual_address: u64,
    physical_address: u64,
    file_size: u64,
    memory_size: u64,
    align: u64,
}

/// Read a little-endian value out of the file without assuming alignment.
///
/// The bytes are a file, not a struct. Casting a pointer into the middle of a
/// buffer and dereferencing it happens to work on x86 and is undefined
/// behaviour everywhere, including here — `read_unaligned` costs nothing and
/// is simply correct.
fn read_u16(data: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([data[at], data[at + 1]])
}

fn read_u32(data: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([data[at], data[at + 1], data[at + 2], data[at + 3]])
}

fn read_u64(data: &[u8], at: usize) -> u64 {
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&data[at..at + 8]);
    u64::from_le_bytes(bytes)
}

/// Load an ELF executable into user-accessible pages.
///
/// Every failure here is a refusal to run something, which is the only correct
/// response to a file the kernel does not fully understand. A loader that
/// guesses is a loader that executes attacker-chosen bytes.
///
/// # Safety
/// Maps pages and writes to them. The addresses come from the file, so this
/// must only be handed files the kernel is willing to trust that far.
pub unsafe fn load(data: &[u8], verbose: bool) -> Result<Loaded, &'static str> {
    if data.len() < 64 {
        return Err("too small to be an ELF header");
    }
    if data[0..4] != MAGIC {
        return Err("not an ELF file (bad magic)");
    }
    if data[4] != CLASS_64 {
        return Err("not a 64-bit ELF");
    }
    if data[5] != LITTLE_ENDIAN {
        return Err("not little-endian");
    }

    let kind = read_u16(data, 16);
    if kind != TYPE_EXECUTABLE {
        // A position-independent executable is type 3, and running one means
        // choosing a base address and applying relocations. This kernel does
        // neither, so the honest answer is no.
        return Err("not a fixed-address executable (ET_EXEC)");
    }
    if read_u16(data, 18) != MACHINE_X86_64 {
        return Err("not compiled for x86_64");
    }

    let entry = read_u64(data, 24);
    let header_offset = read_u64(data, 32) as usize;
    let header_size = read_u16(data, 54) as usize;
    let header_count = read_u16(data, 56) as usize;

    if header_size < core::mem::size_of::<ProgramHeader>() {
        return Err("program headers are too small");
    }
    let table_end = header_offset
        .checked_add(header_size * header_count)
        .ok_or("program header table overflows")?;
    if table_end > data.len() {
        return Err("program header table runs past the end of the file");
    }

    if verbose {
        crate::println!("    entry point {entry:#x}, {header_count} program headers");
    }

    let mut segments = 0;
    let mut bytes_zeroed = 0u64;
    let mut pages = Vec::new();

    for index in 0..header_count {
        let at = header_offset + index * header_size;
        let header = ProgramHeader {
            kind: read_u32(data, at),
            flags: read_u32(data, at + 4),
            offset: read_u64(data, at + 8),
            virtual_address: read_u64(data, at + 16),
            physical_address: read_u64(data, at + 24),
            file_size: read_u64(data, at + 32),
            memory_size: read_u64(data, at + 40),
            align: read_u64(data, at + 48),
        };
        let _ = (header.physical_address, header.align);

        // Everything that is not PT_LOAD describes something this kernel does
        // not implement -- dynamic linking, the stack's desired permissions,
        // build notes. Ignoring them is correct, not lazy.
        if header.kind != PT_LOAD || header.memory_size == 0 {
            continue;
        }

        if header.file_size > header.memory_size {
            return Err("segment claims more file bytes than memory");
        }
        let file_end = header
            .offset
            .checked_add(header.file_size)
            .ok_or("segment offset overflows")?;
        if file_end > data.len() as u64 {
            return Err("segment runs past the end of the file");
        }
        // A program does not get to ask for a page in kernel territory.
        let memory_end = header
            .virtual_address
            .checked_add(header.memory_size)
            .ok_or("segment address overflows")?;
        if memory_end > USER_ADDRESS_LIMIT {
            return Err("segment wants an address outside userspace");
        }

        if verbose {
            crate::println!(
                "    {:#010x}  {:>6} bytes  {}{}{}{}",
                header.virtual_address,
                header.memory_size,
                if header.flags & PF_R != 0 { "r" } else { "-" },
                if header.flags & PF_W != 0 { "w" } else { "-" },
                if header.flags & PF_X != 0 { "x" } else { "-" },
                if header.memory_size > header.file_size {
                    " (.bss: more memory than file)"
                } else {
                    ""
                },
            );
        }

        let first_page = header.virtual_address & !(PAGE_SIZE as u64 - 1);
        let last_page = (memory_end - 1) & !(PAGE_SIZE as u64 - 1);

        // Map writable regardless of what the segment asked for -- we are about
        // to write the program into it. Permissions are applied afterwards.
        let mut page = first_page;
        while page <= last_page {
            if memory::translate(page).is_none() {
                let frame = memory::allocator()
                    .allocate()
                    .ok_or("out of physical memory")?;
                unsafe {
                    memory::map_page(page, frame, memory::FLAG_USER | memory::FLAG_WRITABLE)?;
                    // Zero the whole page before anything is copied into it.
                    // This is what makes the .bss guarantee true, and it is also
                    // the only thing stopping a fresh program from reading
                    // whatever the last owner of this frame left behind.
                    core::ptr::write_bytes(page as *mut u8, 0, PAGE_SIZE);
                }
                bytes_zeroed += PAGE_SIZE as u64;
                pages.push(page);
            }
            page += PAGE_SIZE as u64;
        }

        // Copy the part the file actually contains. Whatever the segment asked
        // for beyond that is already zero.
        if header.file_size > 0 {
            let source = &data[header.offset as usize..file_end as usize];
            unsafe {
                core::ptr::copy_nonoverlapping(
                    source.as_ptr(),
                    header.virtual_address as *mut u8,
                    source.len(),
                );
            }
        }

        segments += 1;
    }

    if segments == 0 {
        return Err("nothing to load: no PT_LOAD segments");
    }

    // Second pass, once every byte is in place: give each segment the
    // permissions it asked for. Doing this as we went would mean sealing a page
    // a later segment still needs to write to.
    for index in 0..header_count {
        let at = header_offset + index * header_size;
        let kind = read_u32(data, at);
        let flags = read_u32(data, at + 4);
        let virtual_address = read_u64(data, at + 16);
        let memory_size = read_u64(data, at + 40);
        if kind != PT_LOAD || memory_size == 0 {
            continue;
        }

        let mut page_flags = memory::FLAG_USER;
        if flags & PF_W != 0 {
            page_flags |= memory::FLAG_WRITABLE;
        }
        // Nothing is done with PF_X. Marking a page non-executable needs the NX
        // bit and EFER.NXE, which this kernel does not enable -- so every page
        // it maps is executable, and a segment marked rw- is a lie we are
        // currently telling. Worth knowing about rather than hiding.

        let mut page = virtual_address & !(PAGE_SIZE as u64 - 1);
        let last = (virtual_address + memory_size - 1) & !(PAGE_SIZE as u64 - 1);
        while page <= last {
            unsafe { memory::set_flags(page, page_flags)? };
            page += PAGE_SIZE as u64;
        }
    }

    if entry > USER_ADDRESS_LIMIT || memory::translate(entry).is_none() {
        return Err("entry point is not inside anything we loaded");
    }

    Ok(Loaded {
        entry,
        segments,
        bytes_zeroed,
        pages,
    })
}

/// Everything at or above this belongs to the kernel. The kernel itself lives
/// far higher, at 0x1000_0000_0000; this is simply a line a user program has no
/// business crossing.
const USER_ADDRESS_LIMIT: u64 = 0x0000_8000_0000;
