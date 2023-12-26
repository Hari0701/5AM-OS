#!/usr/bin/env bash
# Build 5AM-OS and boot it in QEMU.
#
#   ./run.sh          boot with serial output in this terminal
#   ./run.sh --gui    also open QEMU's display window
#
# For the `ask` command, start bridge/bridge.py in another terminal first.
set -euo pipefail

cd "$(dirname "$0")"

# 1. The kernel, for bare metal.
cargo build --package kernel --target x86_64-unknown-none --release

# 2. The image builder, for this machine.
cargo build --package boot --release

KERNEL="target/x86_64-unknown-none/release/kernel"
IMAGE="$(./target/release/boot "$KERNEL")"

echo "==> booting $IMAGE"
echo "==> quit with Ctrl-A then X"
echo

DISPLAY_ARGS=(-display none)
if [[ "${1:-}" == "--gui" ]]; then
  DISPLAY_ARGS=()
fi

# Two serial ports, in order: COM1 is the console you are reading, COM2 is the
# AI bridge channel. QEMU maps -serial flags to COM1, COM2, ... in sequence, so
# the order of these two lines is load-bearing.
#
# COM2 is a TCP server that does not wait for a connection: with no bridge
# running the kernel simply times out on `ask`, and everything else is
# unaffected.
exec qemu-system-x86_64 \
  -drive format=raw,file="$IMAGE" \
  -serial stdio \
  -serial "tcp:127.0.0.1:${BRIDGE_PORT:-4444},server=on,wait=off" \
  -m 128M \
  "${DISPLAY_ARGS[@]}"
