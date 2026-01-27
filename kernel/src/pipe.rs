//! Pipes: a bounded buffer with a reader at one end and a writer at the other.
//!
//! Every mechanism this kernel has for one program to affect another so far has
//! been an accident of sharing — copy-on-write memory that both sides can read,
//! an exit code the parent collects. A pipe is the first one built on purpose.
//!
//! ## Why the interesting part is the emptiness
//!
//! The buffer is trivial: bytes in one end, out the other, wrapping around a
//! fixed array. What makes it a pipe rather than an array is what happens at the
//! edges.
//!
//! Read an empty pipe and you **block** until somebody writes — unless no writer
//! exists any more, in which case you get zero bytes and that means end of file,
//! forever. Write a full pipe and you block until somebody reads. Those two
//! rules are the whole of flow control: a fast producer is slowed to the speed
//! of its consumer without either of them knowing the other exists, and neither
//! has to agree on a size in advance.
//!
//! ## End-of-file is a reference count
//!
//! A reader cannot ask "will there be more?" — it can only be told, and the only
//! honest way to know is that every write end has been closed. So a pipe counts
//! its ends. That is why `fork` has to bump the count and why a process that
//! forgets to close the write end it inherited hangs the reader forever: the
//! reader is waiting on a writer that exists only as a number.
//!
//! It is the most common pipe bug there is, and it is not a bug in the pipe.

use crate::task;

pub const CAPACITY: usize = 512;
pub const MAX_PIPES: usize = 8;

pub struct Pipe {
    buffer: [u8; CAPACITY],
    /// Where the next byte will be written, and where the next will be read.
    /// The gap between them is the contents; equal means empty.
    head: usize,
    tail: usize,
    len: usize,
    pub readers: u32,
    pub writers: u32,
    used: bool,
}

impl Pipe {
    const fn empty() -> Self {
        Self {
            buffer: [0; CAPACITY],
            head: 0,
            tail: 0,
            len: 0,
            readers: 0,
            writers: 0,
            used: false,
        }
    }
}

static mut PIPES: [Pipe; MAX_PIPES] = [const { Pipe::empty() }; MAX_PIPES];

fn pipes() -> &'static mut [Pipe; MAX_PIPES] {
    unsafe { &mut *core::ptr::addr_of_mut!(PIPES) }
}

/// Wait channels. A reader sleeps on one, a writer on the other, and each wakes
/// the opposite side. Two channels rather than one so that waking a reader does
/// not also wake every blocked writer to discover nothing changed for them.
fn readable_channel(id: usize) -> u64 {
    0x9000_0000 + id as u64
}

fn writable_channel(id: usize) -> u64 {
    0x9100_0000 + id as u64
}

/// Make a pipe. It starts with one reader and one writer: the process calling.
pub fn create() -> Option<usize> {
    crate::interrupts::without_interrupts(|| {
        let pipes = pipes();
        let id = pipes.iter().position(|p| !p.used)?;
        pipes[id] = Pipe::empty();
        pipes[id].used = true;
        pipes[id].readers = 1;
        pipes[id].writers = 1;
        Some(id)
    })
}

pub fn add_reader(id: usize) {
    crate::interrupts::without_interrupts(|| pipes()[id].readers += 1)
}

pub fn add_writer(id: usize) {
    crate::interrupts::without_interrupts(|| pipes()[id].writers += 1)
}

/// Close one read end. When the last one goes, writers are woken so they can
/// discover there is nobody left to write to.
pub fn close_reader(id: usize) {
    crate::interrupts::without_interrupts(|| {
        let pipe = &mut pipes()[id];
        pipe.readers = pipe.readers.saturating_sub(1);
        if pipe.readers == 0 && pipe.writers == 0 {
            pipe.used = false;
        }
    });
    task::wake_all(writable_channel(id));
}

/// Close one write end. When the last one goes, any blocked reader must be
/// woken -- not because there is data, but because there never will be, and
/// waiting forever is the alternative.
pub fn close_writer(id: usize) {
    crate::interrupts::without_interrupts(|| {
        let pipe = &mut pipes()[id];
        pipe.writers = pipe.writers.saturating_sub(1);
        if pipe.readers == 0 && pipe.writers == 0 {
            pipe.used = false;
        }
    });
    task::wake_all(readable_channel(id));
}

/// Read up to `out.len()` bytes. Blocks while the pipe is empty and a writer
/// still exists. Returns 0 only at end of file.
pub fn read(id: usize, out: &mut [u8]) -> usize {
    loop {
        let taken = crate::interrupts::without_interrupts(|| {
            let pipe = &mut pipes()[id];
            if pipe.len == 0 {
                // Nothing now. Is there any prospect of more?
                return if pipe.writers == 0 { Some(0) } else { None };
            }
            let count = pipe.len.min(out.len());
            for slot in out.iter_mut().take(count) {
                *slot = pipe.buffer[pipe.tail];
                pipe.tail = (pipe.tail + 1) % CAPACITY;
            }
            pipe.len -= count;
            Some(count)
        });

        if let Some(count) = taken {
            if count > 0 {
                // Somebody may be blocked waiting for room.
                task::wake_all(writable_channel(id));
            }
            return count;
        }

        // Empty, but a writer exists. Test-and-block as one step, or a write
        // landing in between is a wakeup nobody is awake to receive.
        task::block_until(readable_channel(id), || {
            let pipe = &pipes()[id];
            if pipe.len > 0 || pipe.writers == 0 {
                Some(())
            } else {
                None
            }
        });
    }
}

/// Write bytes, blocking while the pipe is full. Returns how many were taken,
/// or 0 if there is nobody left to read them.
pub fn write(id: usize, data: &[u8]) -> usize {
    let mut written = 0;

    while written < data.len() {
        let placed = crate::interrupts::without_interrupts(|| {
            let pipe = &mut pipes()[id];
            // Writing into a pipe nobody will ever read is pointless, and on a
            // real system it kills the writer with SIGPIPE. There are no
            // signals here, so it is reported as a short write.
            if pipe.readers == 0 {
                return None;
            }
            let room = CAPACITY - pipe.len;
            if room == 0 {
                return Some(0);
            }
            let count = room.min(data.len() - written);
            for &byte in &data[written..written + count] {
                pipe.buffer[pipe.head] = byte;
                pipe.head = (pipe.head + 1) % CAPACITY;
            }
            pipe.len += count;
            Some(count)
        });

        match placed {
            None => break, // no readers left
            Some(0) => {
                // Full. Wake any reader, then wait for room.
                task::wake_all(readable_channel(id));
                task::block_until(writable_channel(id), || {
                    let pipe = &pipes()[id];
                    if pipe.len < CAPACITY || pipe.readers == 0 {
                        Some(())
                    } else {
                        None
                    }
                });
            }
            Some(count) => written += count,
        }
    }

    if written > 0 {
        task::wake_all(readable_channel(id));
    }
    written
}

/// For the shell and the tests.
pub fn stats(id: usize) -> (usize, u32, u32) {
    let pipe = &pipes()[id];
    (pipe.len, pipe.readers, pipe.writers)
}

pub fn in_use(id: usize) -> bool {
    pipes()[id].used
}
