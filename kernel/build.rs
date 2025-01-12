//! Builds the ring 3 program and hands the kernel the resulting ELF.
//!
//! The kernel has no filesystem, so a program cannot be *opened* — it has to
//! arrive some other way. Embedding it means `include_bytes!`, and that needs
//! the file to exist before the kernel compiles, which is what this does.
//!
//! Nested cargo needs two things to behave. It gets its own target directory,
//! because two cargo processes sharing one would deadlock on the lock file.
//! And the parent's RUSTFLAGS are stripped: this build runs with a custom
//! target and `-Z build-std`, none of which apply to a user program compiled
//! for a stock target.

use std::path::PathBuf;
use std::process::Command;

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let userland = manifest.parent().unwrap().join("userland");

    println!("cargo:rerun-if-changed={}", userland.join("src/main.rs").display());
    println!("cargo:rerun-if-changed={}", userland.join("link.ld").display());
    println!("cargo:rerun-if-changed={}", userland.join("Cargo.toml").display());
    println!("cargo:rerun-if-changed={}", userland.join(".cargo/config.toml").display());

    let target_dir = out.join("userland-target");
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());

    let mut command = Command::new(cargo);
    command
        .current_dir(&userland)
        .arg("build")
        .arg("--release")
        .arg("--target-dir")
        .arg(&target_dir);

    // Anything the parent build set that would follow us into a build it does
    // not describe.
    for variable in [
        "RUSTFLAGS",
        "CARGO_ENCODED_RUSTFLAGS",
        "RUSTC_WORKSPACE_WRAPPER",
        "CARGO_BUILD_TARGET",
        "CARGO_BUILD_RUSTFLAGS",
        "CARGO_UNSTABLE_BUILD_STD",
        "CARGO_UNSTABLE_BUILD_STD_FEATURES",
    ] {
        command.env_remove(variable);
    }

    let status = command.status().expect("failed to run cargo for userland");
    assert!(status.success(), "building the userland program failed");

    let built = target_dir.join("x86_64-unknown-none/release/hello");
    let destination = out.join("user.elf");
    std::fs::copy(&built, &destination)
        .unwrap_or_else(|error| panic!("copying {}: {error}", built.display()));
}
