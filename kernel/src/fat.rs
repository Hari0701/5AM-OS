//! FAT16.
//!
//! The last thing this kernel was missing. Until now a program had to be
//! compiled into the kernel to exist at all — `include_bytes!` is not a
//! filesystem, it is a promise made at build time. With this, `exec hello.elf`
//! reads a file off a disk the kernel has never seen before and runs it.
//!
//! ## Why FAT and not something nicer
//!
//! Because it can be checked. The image `mkfs` produces mounts on macOS,
//! Windows and Linux, so when this reader disagrees with the disk there is an
//! authority to consult that is not more of my own code. A format I invented
//! would have been simpler, more elegant, and only ever verifiable against the
//! thing under test.
//!
//! It is also genuinely everywhere — SD cards, USB sticks, the UEFI system
//! partition on the machine you are reading this on.
//!
//! ## The idea
//!
//! One array, called the File Allocation Table, with one entry per cluster.
//! Each entry holds the number of the *next* cluster in the same file, or a
//! marker meaning "this is the last one". A file is therefore a linked list,
//! and the list lives in a table at the front of the disk rather than in the
//! data itself.
//!
//! Everything good and bad about FAT follows from that. Growing a file is
//! trivial. Finding byte 40,000 of a file means walking the chain from the
//! start, because nothing indexes it. And if the table is damaged, the data is
//! all still there, in an order nothing records.
//!
//! ## Writing
//!
//! Reading a filesystem is following instructions somebody else wrote. Writing
//! one is taking responsibility for a structure that must stay consistent
//! whatever happens next — and that difference is most of what a filesystem is.
//!
//! See the `writing` section below for the ordering rules.
//!
//! ## What is deliberately missing
//!
//! Subdirectories and long file names. This uses the root directory only, in
//! 8.3 form. Long names are stored as a chain of hidden entries masquerading as
//! volume labels, which is a fascinating piece of backwards compatibility and
//! not one worth implementing to read `hello.elf`.

use crate::ata::{self, SECTOR_SIZE};
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

const DIRECTORY_ENTRY_SIZE: usize = 32;
const ATTRIBUTE_VOLUME_LABEL: u8 = 0x08;
const ATTRIBUTE_DIRECTORY: u8 = 0x10;
/// The first byte of an unused directory slot.
const ENTRY_FREE: u8 = 0x00;
const ENTRY_DELETED: u8 = 0xE5;

const END_OF_CHAIN: u16 = 0xFFF8;

/// Everything needed to find anything, read once at mount time.
#[derive(Clone, Copy)]
pub struct Volume {
    pub bytes_per_sector: u32,
    pub fat_count: u32,
    pub sectors_per_cluster: u32,
    pub fat_start: u32,
    pub sectors_per_fat: u32,
    pub root_start: u32,
    pub root_entries: u32,
    pub data_start: u32,
    pub clusters: u32,
}

pub struct Entry {
    pub name: String,
    pub size: u32,
    pub first_cluster: u16,
}

fn read_u16(data: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([data[at], data[at + 1]])
}

fn read_u32(data: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([data[at], data[at + 1], data[at + 2], data[at + 3]])
}

/// Read the boot sector and work out where everything lives.
///
/// Every number here is derived, not stored. The BPB says how many of each
/// region there are; where they begin is arithmetic, and getting the arithmetic
/// wrong produces a filesystem that reads plausible garbage rather than
/// failing — which is much harder to debug than a refusal.
pub fn mount() -> Result<Volume, &'static str> {
    if !ata::present() {
        return Err("no disk on the primary slave (did run.sh attach fs.img?)");
    }

    let mut sector = [0u8; SECTOR_SIZE];
    ata::read(0, &mut sector)?;

    if sector[510] != 0x55 || sector[511] != 0xAA {
        return Err("sector 0 is not a boot sector");
    }

    let bytes_per_sector = read_u16(&sector, 11) as u32;
    if bytes_per_sector as usize != SECTOR_SIZE {
        return Err("this reader only handles 512-byte sectors");
    }

    let sectors_per_cluster = sector[13] as u32;
    if sectors_per_cluster == 0 || !sectors_per_cluster.is_power_of_two() {
        return Err("sectors per cluster must be a power of two");
    }

    let reserved = read_u16(&sector, 14) as u32;
    let fat_count = sector[16] as u32;
    let root_entries = read_u16(&sector, 17) as u32;
    let sectors_per_fat = read_u16(&sector, 22) as u32;

    let total_sectors = match read_u16(&sector, 19) as u32 {
        0 => read_u32(&sector, 32), // volumes over 32 MiB use the 32-bit field
        small => small,
    };
    if reserved == 0 || fat_count == 0 || sectors_per_fat == 0 || total_sectors == 0 {
        return Err("the BPB has a zero where it must not");
    }

    let fat_start = reserved;
    let root_start = fat_start + fat_count * sectors_per_fat;
    let root_sectors = (root_entries * DIRECTORY_ENTRY_SIZE as u32).div_ceil(bytes_per_sector);
    let data_start = root_start + root_sectors;
    if data_start >= total_sectors {
        return Err("the data region starts past the end of the volume");
    }
    let clusters = (total_sectors - data_start) / sectors_per_cluster;

    // The cluster count is what decides FAT12 vs 16 vs 32. There is no field
    // that says which one a volume is -- readers are expected to do this
    // division and take the answer, which is why a volume can silently become
    // a different filesystem if it is resized.
    if !(4085..65525).contains(&clusters) {
        return Err("cluster count is not in the FAT16 range");
    }

    Ok(Volume {
        bytes_per_sector,
        fat_count,
        sectors_per_cluster,
        fat_start,
        sectors_per_fat,
        root_start,
        root_entries,
        data_start,
        clusters,
    })
}

impl Volume {
    fn cluster_to_sector(&self, cluster: u16) -> u32 {
        // Minus two, because entries 0 and 1 of the table describe no cluster.
        self.data_start + (cluster as u32 - 2) * self.sectors_per_cluster
    }

    fn cluster_bytes(&self) -> usize {
        (self.sectors_per_cluster * self.bytes_per_sector) as usize
    }

    /// What comes after this cluster.
    fn next_cluster(&self, cluster: u16) -> Result<u16, &'static str> {
        let offset = cluster as u32 * 2;
        let sector = self.fat_start + offset / self.bytes_per_sector;
        let within = (offset % self.bytes_per_sector) as usize;

        let mut buffer = [0u8; SECTOR_SIZE];
        ata::read(sector, &mut buffer)?;
        Ok(read_u16(&buffer, within))
    }

    /// Every usable entry in the root directory.
    pub fn list(&self) -> Result<Vec<Entry>, &'static str> {
        let root_sectors =
            (self.root_entries * DIRECTORY_ENTRY_SIZE as u32).div_ceil(self.bytes_per_sector);
        let mut entries = Vec::new();
        let mut buffer = [0u8; SECTOR_SIZE];

        for index in 0..root_sectors {
            ata::read(self.root_start + index, &mut buffer)?;

            for slot in buffer.chunks_exact(DIRECTORY_ENTRY_SIZE) {
                match slot[0] {
                    // A zero here means not just "this slot is empty" but "every
                    // slot after this one is too" -- the directory is scanned in
                    // order and stops at the first never-used entry.
                    ENTRY_FREE => return Ok(entries),
                    ENTRY_DELETED => continue,
                    _ => {}
                }

                let attributes = slot[11];
                if attributes & ATTRIBUTE_VOLUME_LABEL != 0 {
                    // Also skips long-file-name entries, which disguise
                    // themselves as volume labels precisely so that readers
                    // like this one ignore them.
                    continue;
                }
                if attributes & ATTRIBUTE_DIRECTORY != 0 {
                    continue;
                }

                entries.push(Entry {
                    name: decode_name(slot),
                    first_cluster: read_u16(slot, 26),
                    size: read_u32(slot, 28),
                });
            }
        }

        Ok(entries)
    }

    pub fn find(&self, name: &str) -> Result<Entry, &'static str> {
        self.list()?
            .into_iter()
            .find(|entry| entry.name.eq_ignore_ascii_case(name))
            .ok_or("no such file")
    }

    /// Read a whole file by walking its chain.
    pub fn read_file(&self, entry: &Entry) -> Result<Vec<u8>, &'static str> {
        let mut data = vec![0u8; entry.size as usize];
        if entry.size == 0 {
            return Ok(data);
        }

        let mut cluster = entry.first_cluster;
        let mut written = 0usize;
        // A corrupt table can point a chain back at itself. Without a bound the
        // read never returns, and "the shell hung" is a poor description of
        // "the filesystem is damaged".
        let mut visited = 0u32;

        while written < data.len() {
            if cluster < 2 || cluster as u32 >= self.clusters + 2 {
                return Err("chain left the volume");
            }
            visited += 1;
            if visited > self.clusters {
                return Err("cluster chain loops");
            }

            let mut buffer = vec![0u8; self.cluster_bytes()];
            ata::read(self.cluster_to_sector(cluster), &mut buffer)?;

            let wanted = (data.len() - written).min(buffer.len());
            data[written..written + wanted].copy_from_slice(&buffer[..wanted]);
            written += wanted;

            if written >= data.len() {
                break;
            }

            cluster = self.next_cluster(cluster)?;
            if cluster >= END_OF_CHAIN {
                return Err("chain ended before the file did");
            }
        }

        Ok(data)
    }
}

/// Turn `HELLO   ELF` back into `HELLO.ELF`.
fn decode_name(slot: &[u8]) -> String {
    let mut name = String::new();
    for &byte in &slot[0..8] {
        if byte != b' ' {
            name.push(byte as char);
        }
    }
    let extension: String = slot[8..11]
        .iter()
        .filter(|&&byte| byte != b' ')
        .map(|&byte| byte as char)
        .collect();
    if !extension.is_empty() {
        name.push('.');
        name.push_str(&extension);
    }
    name
}

// --- writing -------------------------------------------------------------
//
// Reading a filesystem is following instructions somebody else wrote. Writing
// one is taking responsibility for a structure that has to stay consistent
// whatever happens next, and that difference is most of what a filesystem *is*.
//
// Three things have to agree after every write: the directory entry (this file
// exists, is this long, and starts here), the FAT (these clusters belong to it,
// in this order, ending here), and the data itself. Update them in the wrong
// order and a power cut leaves a volume that is not merely missing a file but
// actively wrong -- clusters marked in use by nothing, or two files claiming
// the same one.
//
// The order used here is deliberate: data first, then the table, then the
// directory entry last. Every prefix of that sequence leaves a volume that is
// still consistent, just without the new file in it. That is the cheap version
// of what journalling buys properly.

impl Volume {
    fn fat_entry_location(&self, cluster: u16) -> (u32, usize) {
        let offset = cluster as u32 * 2;
        (
            self.fat_start + offset / self.bytes_per_sector,
            (offset % self.bytes_per_sector) as usize,
        )
    }

    /// Point `cluster` at `value` in **every** copy of the table.
    ///
    /// Both copies, because a volume whose FATs disagree is one that other
    /// systems will refuse to mount or, worse, quietly repair by picking one.
    fn set_fat_entry(&self, cluster: u16, value: u16) -> Result<(), &'static str> {
        let (sector, within) = self.fat_entry_location(cluster);
        let mut buffer = [0u8; SECTOR_SIZE];

        for copy in 0..self.fat_count {
            let target = sector + copy * self.sectors_per_fat;
            ata::read(target, &mut buffer)?;
            buffer[within..within + 2].copy_from_slice(&value.to_le_bytes());
            ata::write(target, &buffer)?;
        }
        Ok(())
    }

    /// Find a cluster nobody is using.
    ///
    /// A linear scan from the beginning, which is exactly what makes filling a
    /// nearly full volume slow -- real drivers remember where they last looked.
    fn find_free_cluster(&self, after: u16) -> Result<u16, &'static str> {
        for cluster in after.max(2)..(self.clusters as u16 + 2) {
            if self.next_cluster(cluster)? == 0 {
                return Ok(cluster);
            }
        }
        Err("the volume is full")
    }

    fn cluster_bytes_public(&self) -> usize {
        (self.sectors_per_cluster * self.bytes_per_sector) as usize
    }

    /// Write `data` into a chain of clusters and return the first one.
    fn write_chain(&self, data: &[u8]) -> Result<u16, &'static str> {
        let cluster_bytes = self.cluster_bytes_public();
        let needed = data.len().div_ceil(cluster_bytes).max(1);

        // Reserve the whole chain before writing anything. Allocating as we go
        // would leave half a file's clusters marked in use with nothing
        // pointing at them if we ran out partway.
        let mut chain = Vec::with_capacity(needed);
        let mut search_from = 2u16;
        for _ in 0..needed {
            let cluster = self.find_free_cluster(search_from)?;
            // Claim it immediately, or the next search finds the same one.
            self.set_fat_entry(cluster, 0xFFFF)?;
            search_from = cluster + 1;
            chain.push(cluster);
        }

        // Data first: a cluster with the right bytes and no chain pointing at
        // it is invisible, which is a safe thing to be.
        let mut buffer = vec![0u8; cluster_bytes];
        for (index, &cluster) in chain.iter().enumerate() {
            let start = index * cluster_bytes;
            let end = (start + cluster_bytes).min(data.len());
            buffer.fill(0);
            if start < data.len() {
                buffer[..end - start].copy_from_slice(&data[start..end]);
            }
            ata::write(self.cluster_to_sector(cluster), &buffer)?;
        }

        // Then link them, last to first, so a partial chain is never reachable
        // from the head.
        for index in (0..chain.len()).rev() {
            let value = if index + 1 == chain.len() {
                0xFFFF
            } else {
                chain[index + 1]
            };
            self.set_fat_entry(chain[index], value)?;
        }

        Ok(chain[0])
    }

    /// How many clusters are unused. A full scan of the table, which is what
    /// `df` is doing when it takes a moment on a very large volume.
    pub fn count_free(&self) -> Result<u32, &'static str> {
        let mut free = 0;
        for cluster in 2..(self.clusters as u16 + 2) {
            if self.next_cluster(cluster)? == 0 {
                free += 1;
            }
        }
        Ok(free)
    }

    fn free_chain(&self, first: u16) -> Result<(), &'static str> {
        let mut cluster = first;
        let mut visited = 0u32;
        while cluster >= 2 && cluster < END_OF_CHAIN {
            if visited > self.clusters {
                return Err("cluster chain loops");
            }
            visited += 1;
            let next = self.next_cluster(cluster)?;
            self.set_fat_entry(cluster, 0)?;
            cluster = next;
        }
        Ok(())
    }

    /// Find a directory slot for `name`, reusing its own if it already exists.
    fn directory_slot(&self, name: &str) -> Result<(u32, usize, Option<Entry>), &'static str> {
        let root_sectors =
            (self.root_entries * DIRECTORY_ENTRY_SIZE as u32).div_ceil(self.bytes_per_sector);
        let mut buffer = [0u8; SECTOR_SIZE];
        let mut first_empty: Option<(u32, usize)> = None;

        for index in 0..root_sectors {
            let sector = self.root_start + index;
            ata::read(sector, &mut buffer)?;

            for (slot_index, slot) in buffer.chunks_exact(DIRECTORY_ENTRY_SIZE).enumerate() {
                let offset = slot_index * DIRECTORY_ENTRY_SIZE;
                match slot[0] {
                    ENTRY_FREE | ENTRY_DELETED => {
                        if first_empty.is_none() {
                            first_empty = Some((sector, offset));
                        }
                        if slot[0] == ENTRY_FREE {
                            // Nothing beyond here has ever been used.
                            let (s, o) = first_empty.unwrap();
                            return Ok((s, o, None));
                        }
                    }
                    _ => {
                        if slot[11] & (ATTRIBUTE_VOLUME_LABEL | ATTRIBUTE_DIRECTORY) != 0 {
                            continue;
                        }
                        if decode_name(slot).eq_ignore_ascii_case(name) {
                            return Ok((
                                sector,
                                offset,
                                Some(Entry {
                                    name: decode_name(slot),
                                    first_cluster: read_u16(slot, 26),
                                    size: read_u32(slot, 28),
                                }),
                            ));
                        }
                    }
                }
            }
        }

        match first_empty {
            Some((sector, offset)) => Ok((sector, offset, None)),
            None => Err("the root directory is full"),
        }
    }

    /// Create or replace a file.
    pub fn create(&self, name: &str, data: &[u8]) -> Result<(), &'static str> {
        let (stem, extension) = match name.rsplit_once('.') {
            Some((stem, extension)) => (stem, extension),
            None => (name, ""),
        };
        if stem.is_empty() || stem.len() > 8 || extension.len() > 3 {
            return Err("name must fit 8.3");
        }

        let (sector, offset, existing) = self.directory_slot(name)?;

        // Write the new contents before touching anything that refers to the
        // old ones.
        let first_cluster = if data.is_empty() {
            0
        } else {
            self.write_chain(data)?
        };

        // Only now is the old chain unreachable and safe to release.
        if let Some(old) = &existing {
            if old.first_cluster >= 2 {
                self.free_chain(old.first_cluster)?;
            }
        }

        let mut buffer = [0u8; SECTOR_SIZE];
        ata::read(sector, &mut buffer)?;
        let slot = &mut buffer[offset..offset + DIRECTORY_ENTRY_SIZE];
        slot.fill(0);
        slot[0..11].fill(b' ');
        for (index, byte) in stem.bytes().enumerate() {
            slot[index] = byte.to_ascii_uppercase();
        }
        for (index, byte) in extension.bytes().enumerate() {
            slot[8 + index] = byte.to_ascii_uppercase();
        }
        slot[11] = 0x20; // an ordinary archive file
        slot[26..28].copy_from_slice(&first_cluster.to_le_bytes());
        slot[28..32].copy_from_slice(&(data.len() as u32).to_le_bytes());
        ata::write(sector, &buffer)?;

        Ok(())
    }

    /// Delete a file: release its clusters and mark the directory slot dead.
    ///
    /// `0xE5` in the first byte, which famously does not erase the data -- it
    /// only says the slot may be reused. Every undelete tool ever written is
    /// built on that, and so is every disposal mistake.
    pub fn remove(&self, name: &str) -> Result<(), &'static str> {
        let (sector, offset, existing) = self.directory_slot(name)?;
        let Some(entry) = existing else {
            return Err("no such file");
        };

        let mut buffer = [0u8; SECTOR_SIZE];
        ata::read(sector, &mut buffer)?;
        buffer[offset] = ENTRY_DELETED;
        ata::write(sector, &buffer)?;

        if entry.first_cluster >= 2 {
            self.free_chain(entry.first_cluster)?;
        }
        Ok(())
    }
}
