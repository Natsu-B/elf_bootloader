#!/bin/sh
set -e

PATH_TO_ELF="$1"
SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
BIN="$SCRIPT_DIR/bin"

DISK_IMG="$BIN/disk.img"
MNT1="$SCRIPT_DIR/mnt1"
MNT2="$SCRIPT_DIR/mnt2"
ROOTFS_IMG="$BIN/DISK0"

SIZE_MB=2048

mkdir -p "$BIN" "$MNT1" "$MNT2"

dd if=/dev/zero of="$DISK_IMG" bs=1M count=2048

# - p1: start 1MiB、total 512MiB、FAT32(LBA), bootable
# - p2: Linux(0x83)
sudo sfdisk "$DISK_IMG" <<'EOF'
label: dos
unit: sectors
sector-size: 512

start=2048, size=1048576, type=c, bootable

start=, type=83
EOF

LOOP=$(sudo losetup --find --show --partscan "$DISK_IMG")
echo "loop = $LOOP"

sudo mkfs.vfat -F 32 "${LOOP}p1"
sudo mount "${LOOP}p1" "$MNT1"

sudo cp "$PATH_TO_ELF"               "$MNT1/elf-hypervisor.elf"
sudo cp "$BIN/boot.scr"              "$MNT1/boot.scr"      || true
sudo cp "$BIN/u-boot.bin"            "$MNT1/u-boot.bin"    || true
sudo cp "$BIN/Image"                 "$MNT1/image"
sudo cp "$BIN/qemu_mod.dtb"          "$MNT1/qemu.dtb"
sync

sudo dd if="$ROOTFS_IMG" of="${LOOP}p2" bs=4M conv=fsync

sudo umount "$MNT1"
sudo losetup -d "$LOOP"

qemu-system-aarch64 \
  -M virt,gic-version=3,secure=off,virtualization=on \
  -global virtio-mmio.force-legacy=off \
  -smp 4 -bios "$BIN/u-boot.bin" -cpu cortex-a55 -m 4G \
  -nographic \
  -device virtio-blk-device,drive=disk \
  -drive file="$DISK_IMG",format=raw,if=none,media=disk,id=disk \
  -gdb tcp::1234