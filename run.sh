#!/bin/sh

PATH_TO_ELF="$1"

# get absolute path
SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)

DISK_IMG=$SCRIPT_DIR/bin/fat32.img
DISK_MOUNT_DIR=$SCRIPT_DIR/mnt/

# make file ref( https://github.com/PG-MANA/MiniVisor/blob/main/tools/create_disk.sh )
if [ ! -f $DISK_IMG ]; then
    dd if=/dev/zero of=$DISK_IMG  bs=1024 count=2048000
fi

echo -e "o\nn\np\n1\n2048\n\nt\nc\nw\n" | sudo fdisk $DISK_IMG || sudo rm -rf $DISK_IMG
sudo mkfs.vfat -F 32 -h 2048 --offset=2048 $DISK_IMG

rm -rf $DISK_MOUNT_DIR
mkdir -p $DISK_MOUNT_DIR

sudo mount -o loop,offset=$((2048 * 512)) $DISK_IMG $DISK_MOUNT_DIR
sudo cp "$PATH_TO_ELF" "$DISK_MOUNT_DIR/elf-hypervisor.elf"
sudo cp "$SCRIPT_DIR/bin/boot.scr" "$DISK_MOUNT_DIR/boot.scr"
sudo cp "$SCRIPT_DIR/bin/u-boot.bin" "$DISK_MOUNT_DIR/u-boot.bin"
sync
sudo umount $DISK_MOUNT_DIR

qemu-system-aarch64 \
  -M virt,gic-version=3,secure=off,virtualization=on \
  -global virtio-mmio.force-legacy=off \
  -smp 4 -bios bin/u-boot.bin -cpu cortex-a55 -m 4G \
  -nographic -device virtio-blk-device,drive=disk \
  -drive file=$DISK_IMG,format=raw,if=none,media=disk,id=disk

