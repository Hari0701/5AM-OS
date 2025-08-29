//! Wraps the compiled kernel in a bootable disk image.
//!
//! This program runs on your machine, under your OS, like any other tool. It is
//! not part of 5AM-OS — it just packages it. The `bootloader` crate supplies
//! the real-mode entry, the mode switches, and the page tables that get the CPU
//! into 64-bit long mode before jumping to our kernel_main.
//!
//! Writing that stage ourselves is a worthwhile exercise and a later milestone;
//! borrowing it now means the kernel boots today.

use std::path::{Path, PathBuf};

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
    let mut builder = bootloader::BiosBoot::new(&kernel);

    // The kernel has no filesystem and no disk driver, so the only way to get
    // 58MB of neural network weights into it is to have the bootloader place
    // them in memory before the kernel starts. That is what a ramdisk is.
    //
    // Only one ramdisk is supported, and we need two files, so they are packed
    // into a single blob with a small header. See kernel/src/llm.rs.
    // ...and 58 MB is a lot to drag through BIOS disk services on every boot.
    // Reading it costs about ninety seconds before the kernel runs at all,
    // which is fine once and miserable when you are changing one line and
    // rebooting to see what happened. So it is opt-in: `--ai` (or WITH_AI=1)
    // includes it, and the default image boots in a couple of seconds.
    let want_ai = std::env::var("WITH_AI").map(|v| v == "1").unwrap_or(false);
    if want_ai {
        if let Some(blob) = pack_assets(&out_dir) {
            builder.set_ramdisk(&blob);
        }
    } else {
        println!("note: no model in this image (run.sh --ai includes it)");
    }

    let bios = out_dir.join("5am-os-bios.img");
    builder
        .create_disk_image(&bios)
        .expect("failed to build the BIOS image");

    // A UEFI image would go here too, and is what a real modern machine
    // boots from. It is disabled in Cargo.toml for now — see the note there.

    println!("{}", bios.display());
}

/// Pack the model and tokenizer into one ramdisk blob.
///
/// Layout, all little-endian:
///
///     magic      u32   0x354D4C4D  ("5MLM")
///     model_len  u64
///     tok_len    u64
///     model      model_len bytes
///     tokenizer  tok_len bytes
///
/// Returns None when the assets are absent — the OS boots fine without them,
/// it just cannot run the model, and `llm` says so.
fn pack_assets(out_dir: &Path) -> Option<PathBuf> {
    let assets = Path::new(env!("CARGO_MANIFEST_DIR")).parent()?.join("assets");
    let model_path = assets.join("model.bin");
    let tokenizer_path = assets.join("tokenizer.bin");

    if !model_path.exists() || !tokenizer_path.exists() {
        eprintln!(
            "note: {} not found — building without the model. \
             See README (`the neural network`) for how to fetch it.",
            assets.display()
        );
        return None;
    }

    let model = std::fs::read(&model_path).expect("could not read the model");
    let tokenizer = std::fs::read(&tokenizer_path).expect("could not read the tokenizer");

    let mut blob = Vec::with_capacity(20 + model.len() + tokenizer.len());
    blob.extend_from_slice(&0x354D_4C4Du32.to_le_bytes());
    blob.extend_from_slice(&(model.len() as u64).to_le_bytes());
    blob.extend_from_slice(&(tokenizer.len() as u64).to_le_bytes());
    blob.extend_from_slice(&model);
    blob.extend_from_slice(&tokenizer);

    let path = out_dir.join("5am-os-ramdisk.bin");
    std::fs::write(&path, &blob).expect("could not write the ramdisk blob");
    eprintln!("ramdisk: {} MiB", blob.len() / 1024 / 1024);
    Some(path)
}
