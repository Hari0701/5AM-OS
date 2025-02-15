//! Builds a FAT16 disk image for 5AM-OS.
//!
//! Runs on your machine, not in the kernel. It exists because the alternative
//! is asking you to install mtools or to mount an image as root, and because
//! writing the format out by hand is the fastest way to understand what the
//! kernel is about to read back.
//!
//! FAT is worth implementing rather than inventing something simpler. It is on
//! every SD card, every USB stick and every UEFI system partition, it is old
//! enough that its compromises are visible rather than hidden, and — the part
//! that matters here — the kernel's reader can be checked against any other
//! machine. `hdiutil attach` the image on a Mac and Finder will open it. A
//! format I made up would only ever have been readable by the code that was
//! supposed to be under test.
//!
//! ## The layout, in order
//!
//! ```text
//!   boot sector      1 sector    the BPB: how to read everything else
//!   FAT 1            32 sectors  a linked list, one entry per cluster
//!   FAT 2            32 sectors  an identical copy, because 1980s floppies
//!   root directory   32 sectors  512 fixed slots of 32 bytes
//!   data             the rest    clusters, numbered from 2
//! ```

use std::path::PathBuf;

const SECTOR: usize = 512;
const SECTORS_PER_CLUSTER: usize = 4;
const RESERVED_SECTORS: usize = 1;
const FAT_COUNT: usize = 2;
const ROOT_ENTRIES: usize = 512;
const TOTAL_SECTORS: usize = 32768; // 16 MiB
const SECTORS_PER_FAT: usize = 32;

const ROOT_SECTORS: usize = ROOT_ENTRIES * 32 / SECTOR;
const FAT_START: usize = RESERVED_SECTORS;
const ROOT_START: usize = FAT_START + FAT_COUNT * SECTORS_PER_FAT;
const DATA_START: usize = ROOT_START + ROOT_SECTORS;

struct File {
    name: String,
    data: Vec<u8>,
}

fn main() {
    let mut arguments = std::env::args().skip(1);
    let output = PathBuf::from(arguments.next().expect("usage: mkfs <out.img> [name=path ...]"));

    let mut files = Vec::new();
    for argument in arguments {
        let (name, path) = argument
            .split_once('=')
            .unwrap_or_else(|| panic!("expected name=path, got {argument}"));
        let data = std::fs::read(path).unwrap_or_else(|e| panic!("reading {path}: {e}"));
        files.push(File {
            name: name.to_string(),
            data,
        });
    }

    let image = build(&files);
    std::fs::write(&output, &image).expect("writing the image");

    let clusters = (TOTAL_SECTORS - DATA_START) / SECTORS_PER_CLUSTER;
    println!(
        "{}: FAT16, {} KiB, {} clusters of {} bytes, {} file(s)",
        output.display(),
        image.len() / 1024,
        clusters,
        SECTORS_PER_CLUSTER * SECTOR,
        files.len()
    );
}

fn build(files: &[File]) -> Vec<u8> {
    let mut image = vec![0u8; TOTAL_SECTORS * SECTOR];

    // A FAT16 volume must have between 4085 and 65524 clusters. Fewer and it is
    // a FAT12 volume by definition, more and it is FAT32 -- the "16" names the
    // width of a FAT entry, and the cluster count is the *only* thing that
    // decides which variant a reader must use. There is no field that says so.
    let clusters = (TOTAL_SECTORS - DATA_START) / SECTORS_PER_CLUSTER;
    assert!(
        (4085..65525).contains(&clusters),
        "{clusters} clusters is not FAT16"
    );

    write_boot_sector(&mut image);

    // FAT entry 0 is the media descriptor, entry 1 is an end-of-chain marker.
    // Neither describes a cluster: real data starts at cluster 2, which is why
    // every cluster-to-sector calculation subtracts two.
    let mut fat = vec![0u16; SECTORS_PER_FAT * SECTOR / 2];
    fat[0] = 0xFFF8;
    fat[1] = 0xFFFF;

    let mut next_cluster = 2usize;
    let mut directory = vec![0u8; ROOT_ENTRIES * 32];

    for (index, file) in files.iter().enumerate() {
        assert!(index < ROOT_ENTRIES, "too many files for the root directory");

        let needed = file.data.len().div_ceil(SECTORS_PER_CLUSTER * SECTOR).max(1);
        let first = next_cluster;
        assert!(
            first + needed < clusters + 2,
            "{} does not fit on the volume",
            file.name
        );

        // Chain the clusters together. This is the whole idea of FAT: the table
        // is one big array of "what comes after this", and a file is a walk
        // through it. Fast to append to, and the reason reading a fragmented
        // file means seeking back to the table between every cluster.
        for offset in 0..needed {
            let cluster = first + offset;
            fat[cluster] = if offset + 1 == needed {
                0xFFFF // end of chain
            } else {
                (cluster + 1) as u16
            };
        }

        let start = DATA_START * SECTOR + (first - 2) * SECTORS_PER_CLUSTER * SECTOR;
        image[start..start + file.data.len()].copy_from_slice(&file.data);

        write_directory_entry(
            &mut directory[index * 32..(index + 1) * 32],
            &file.name,
            first as u16,
            file.data.len() as u32,
        );

        next_cluster = first + needed;
    }

    // Both copies of the FAT. The second is never read by this kernel and is
    // written anyway, because a volume with one FAT is not a volume other
    // systems will agree to mount.
    for copy in 0..FAT_COUNT {
        let start = (FAT_START + copy * SECTORS_PER_FAT) * SECTOR;
        for (index, entry) in fat.iter().enumerate() {
            let at = start + index * 2;
            image[at..at + 2].copy_from_slice(&entry.to_le_bytes());
        }
    }

    let start = ROOT_START * SECTOR;
    image[start..start + directory.len()].copy_from_slice(&directory);

    image
}

fn write_boot_sector(image: &mut [u8]) {
    // A jump instruction, because this field is where the CPU would begin if
    // the volume were bootable. Ours is not, and the bytes are still required:
    // some drivers reject a volume whose first byte is not 0xEB or 0xE9.
    image[0..3].copy_from_slice(&[0xEB, 0x3C, 0x90]);
    image[3..11].copy_from_slice(b"5AMOS1.0");

    let put16 = |image: &mut [u8], at: usize, value: u16| {
        image[at..at + 2].copy_from_slice(&value.to_le_bytes());
    };
    let put32 = |image: &mut [u8], at: usize, value: u32| {
        image[at..at + 4].copy_from_slice(&value.to_le_bytes());
    };

    put16(image, 11, SECTOR as u16);
    image[13] = SECTORS_PER_CLUSTER as u8;
    put16(image, 14, RESERVED_SECTORS as u16);
    image[16] = FAT_COUNT as u8;
    put16(image, 17, ROOT_ENTRIES as u16);
    put16(image, 19, TOTAL_SECTORS as u16);
    image[21] = 0xF8; // "fixed disk"
    put16(image, 22, SECTORS_PER_FAT as u16);
    put16(image, 24, 32); // sectors per track
    put16(image, 26, 8); // heads
    put32(image, 28, 0); // hidden sectors: no partition table, we are the volume
    put32(image, 32, 0);

    image[36] = 0x80; // drive number
    image[38] = 0x29; // extended boot signature: the next three fields exist
    put32(image, 39, 0x5A_4D_05_A1);
    image[43..54].copy_from_slice(b"5AMOS      ");
    image[54..62].copy_from_slice(b"FAT16   ");

    // Every sector that claims to be a boot sector ends with this.
    image[510] = 0x55;
    image[511] = 0xAA;
}

/// Write one 32-byte directory entry in 8.3 form.
///
/// Eight characters, a dot, three more, upper case, space padded. Long file
/// names were bolted on later as a chain of hidden entries pretending to be
/// something else; this writes only the original form, which every FAT reader
/// ever written can understand.
fn write_directory_entry(slot: &mut [u8], name: &str, first_cluster: u16, size: u32) {
    let (stem, extension) = match name.rsplit_once('.') {
        Some((stem, extension)) => (stem, extension),
        None => (name, ""),
    };
    assert!(stem.len() <= 8, "{name}: stem longer than 8 characters");
    assert!(extension.len() <= 3, "{name}: extension longer than 3");

    slot[0..11].fill(b' ');
    for (index, byte) in stem.bytes().enumerate() {
        slot[index] = byte.to_ascii_uppercase();
    }
    for (index, byte) in extension.bytes().enumerate() {
        slot[8 + index] = byte.to_ascii_uppercase();
    }

    slot[11] = 0x20; // attributes: an ordinary archive file
    slot[26..28].copy_from_slice(&first_cluster.to_le_bytes());
    slot[28..32].copy_from_slice(&size.to_le_bytes());
}
