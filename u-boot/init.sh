#!/bin/sh

set -e

# get absolute path
SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)

UBOOT_DIR="$SCRIPT_DIR/u-boot"
BIN_DIR="$SCRIPT_DIR/../bin"

cd "$UBOOT_DIR"

echo "--- Configuring and building u-boot ---"
make qemu_arm64_defconfig
make -j$(nproc)

mkdir -p "$BIN_DIR"

cp u-boot.bin "$BIN_DIR/"

$UBOOT_DIR/tools/mkimage -A arm64 -T script -C none -d $SCRIPT_DIR/boot.txt $BIN_DIR/boot.scr

echo "--- Build complete. Binary is at $BIN_DIR/u-boot.bin ---"
