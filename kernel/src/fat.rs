//! FAT16, read-only.
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
//! ## What is deliberately missing
//!
//! Writing, subdirectories, and long file names. This reads the root directory
//! only, in 8.3 form. Long names are stored as a chain of hidden entries
//! masquerading as volume labels, which is a fascinating piece of backwards
//! compatibility and not one worth implementing to read `hello.elf`.

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
