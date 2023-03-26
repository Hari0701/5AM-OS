#!/usr/bin/env bash
# Build 5AM-OS and boot it in QEMU.
#
#   ./run.sh          boot with serial output in this terminal
#   ./run.sh --gui    also open QEMU's display window
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

exec qemu-system-x86_64 \
  -drive format=raw,file="$IMAGE" \
  -serial stdio \
  -m 128M \
  "${DISPLAY_ARGS[@]}"
