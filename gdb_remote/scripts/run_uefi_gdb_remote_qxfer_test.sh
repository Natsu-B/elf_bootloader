#!/bin/sh
set -eu

PATH_TO_ELF="${1:-}"

if [ -z "$PATH_TO_ELF" ]; then
    echo "usage: $0 <path-to-uefi-test-elf>"
    exit 1
fi

SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
REPO_ROOT=$(cd "$SCRIPT_DIR/../.." && pwd)
BIN_DIR="$SCRIPT_DIR/../bin/EFI/BOOT"

rm -rf "$SCRIPT_DIR/../bin/EFI"
mkdir -p "$BIN_DIR"
cp "$PATH_TO_ELF" "$BIN_DIR/BOOTAA64.EFI"

QEMU_BIN=${QEMU_BIN:-qemu-system-aarch64}

"$QEMU_BIN" \
  -M virt,gic-version=3,secure=off,virtualization=on \
  -global virtio-mmio.force-legacy=off \
  -cpu cortex-a53 -smp 4 -m 1G \
  -bios "$REPO_ROOT/test/RELEASEAARCH64_QEMU_EFI.fd" \
  -nographic \
  -semihosting-config enable=on,target=native \
  -no-reboot -no-shutdown \
  -drive file=fat:rw:"$SCRIPT_DIR/../bin",format=raw,if=none,media=disk,id=disk \
  -device virtio-blk-device,drive=disk,bus=virtio-mmio-bus.0
