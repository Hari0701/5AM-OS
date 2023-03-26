//! Wraps the compiled kernel in a bootable disk image.
//!
//! This program runs on your machine, under your OS, like any other tool. It is
//! not part of 5AM-OS — it just packages it. The `bootloader` crate supplies
//! the real-mode entry, the mode switches, and the page tables that get the CPU
//! into 64-bit long mode before jumping to our kernel_main.
//!
//! Writing that stage ourselves is a worthwhile exercise and a later milestone;
//! borrowing it now means the kernel boots today.

use std::path::PathBuf;

fn main() {
    let kernel = std::env::args()
        .nth(1)
        .expect("usage: boot <path-to-kernel-elf>");
    let kernel = PathBuf::from(kernel);
    assert!(kernel.exists(), "kernel not found: {}", kernel.display());

    let out_dir = kernel
        .parent()
        .expect("kernel has no parent directory")
        .to_path_buf();

    // BIOS image: boots on anything, including QEMU's default firmware.
    let bios = out_dir.join("5am-os-bios.img");
    bootloader::BiosBoot::new(&kernel)
        .create_disk_image(&bios)
        .expect("failed to build the BIOS image");

    // A UEFI image would go here too, and is what a real modern machine
    // boots from. It is disabled in Cargo.toml for now — see the note there.

    println!("{}", bios.display());
}
