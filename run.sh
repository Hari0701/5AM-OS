#!/usr/bin/env bash
# Build 5AM-OS and boot it in QEMU.
#
#   ./run.sh          boot with serial output in this terminal
#   ./run.sh --gui    also open QEMU's display window
#   ./run.sh --ai     include the 15M model (adds ~90s to every boot)
#
# The model is opt-in because it is 58 MB and the bootloader reads all of it
# through BIOS disk services before the kernel gets its first instruction. That
# is ninety seconds you wait every single boot. Without it the machine comes up
# in about two, and everything except `llm`, `ask` and `spawn` works exactly the
# same -- which is the right default when you are changing one line at a time.
#
# For the `ask` command, start bridge/bridge.py in another terminal first.
set -euo pipefail

cd "$(dirname "$0")"

# 1. The kernel, for bare metal.
#
# The custom target enables SSE (the stock x86_64-unknown-none ships with
# +soft-float, which would make the neural network unusably slow). No
# precompiled `core` exists for a custom target, so it is built from source —
# that is what build-std is for, and why the first build takes a while.
cargo build --package kernel \
  --target x86_64-5am_os.json \
  -Z json-target-spec \
  -Z build-std=core,compiler_builtins,alloc \
  -Z build-std-features=compiler-builtins-mem \
  --release

# 2. The image builder, for this machine.
cargo build --package boot --release

# Does this image carry the neural network?
export WITH_AI=0
for argument in "$@"; do
  if [[ "$argument" == "--ai" ]]; then
    export WITH_AI=1
  fi
done

KERNEL="target/x86_64-5am_os/release/kernel"
IMAGE="$(./target/release/boot "$KERNEL" | tail -n 1)"

# 3. The filesystem, on a second disk.
#
# The ring 3 program is built here as well as by the kernel's build.rs. That is
# not redundant: build.rs bakes a copy into the kernel image so `user` works
# with no disk at all, and this copy is a FILE, which is the entire point of
# `exec hello.elf`.
(cd userland && cargo build --release)
cargo build --package mkfs --release

FS="target/fs.img"
./target/release/mkfs "$FS" \
  hello.elf=userland/target/x86_64-unknown-none/release/hello \
  fork.elf=userland/target/x86_64-unknown-none/release/forkdemo \
  bye.elf=userland/target/x86_64-unknown-none/release/bye \
  spawn.elf=userland/target/x86_64-unknown-none/release/spawner \
  spin.elf=userland/target/x86_64-unknown-none/release/spin \
  pipe.elf=userland/target/x86_64-unknown-none/release/pipedemo \
  sh.elf=userland/target/x86_64-unknown-none/release/sh \
  greet.elf=userland/target/x86_64-unknown-none/release/greet \
  upper.elf=userland/target/x86_64-unknown-none/release/upper \
  echo.elf=userland/target/x86_64-unknown-none/release/echo \
  readme.txt=disk/readme.txt \
  motd.txt=disk/motd.txt

echo "==> booting $IMAGE"
echo "==> quit with Ctrl-A then X"
echo

# Headless by default; --gui lets QEMU open its own window.
#
# The array is expanded with the `${arr[@]+...}` form because macOS ships bash
# 3.2, where a plain "${arr[@]}" on an EMPTY array counts as an unset variable
# and trips `set -u` — so --gui aborted the script before QEMU ever started.
DISPLAY_ARGS=(-display none)
for argument in "$@"; do
  if [[ "$argument" == "--gui" ]]; then
    DISPLAY_ARGS=()
  fi
done

# Two serial ports, in order: COM1 is the console you are reading, COM2 is the
# AI bridge channel. QEMU maps -serial flags to COM1, COM2, ... in sequence, so
# the order of these two lines is load-bearing.
#
# COM2 is a TCP server that does not wait for a connection: with no bridge
# running the kernel simply times out on `ask`, and everything else is
# unaffected.
# Two disks on the IDE primary bus. Order matters: index=0 is the master, which
# the BIOS boots from, and index=1 is the slave, which ata.rs reads.
exec qemu-system-x86_64 \
  -drive format=raw,file="$IMAGE",if=ide,index=0 \
  -drive format=raw,file="$FS",if=ide,index=1 \
  -serial stdio \
  -serial "tcp:127.0.0.1:${BRIDGE_PORT:-4444},server=on,wait=off" \
  -cpu max \
  -m 512M \
  ${DISPLAY_ARGS[@]+"${DISPLAY_ARGS[@]}"}
