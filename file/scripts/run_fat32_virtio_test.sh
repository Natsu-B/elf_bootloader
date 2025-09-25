#!/bin/sh

PATH_TO_ELF="$1"

# get absolute path
SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)

DISK_IMG="$SCRIPT_DIR/../bin/fat32_virtio.img"
DISK_MOUNT_DIR="$SCRIPT_DIR/../mnt/"

mkdir -p "$SCRIPT_DIR/../bin"

# make file ref( https://github.com/PG-MANA/MiniVisor/blob/main/tools/create_disk.sh )
if [ ! -f $DISK_IMG ]; then
    dd if=/dev/zero of=$DISK_IMG  bs=1024 count=2048000
fi
echo -e "o\nn\np\n1\n2048\n\nt\nc\nw\n" | sudo fdisk $DISK_IMG || sudo rm -rf $DISK_IMG
sudo mkfs.vfat -F 32 -h 2048 --offset=2048 $DISK_IMG

rm -rf $DISK_MOUNT_DIR
mkdir -p $DISK_MOUNT_DIR

sudo mount -o loop,offset=$((2048 * 512)) $DISK_IMG $DISK_MOUNT_DIR
sudo mkdir -p "$DISK_MOUNT_DIR/EFI/BOOT/"
sudo cp "${PATH_TO_ELF}" "$DISK_MOUNT_DIR/EFI/BOOT/BOOTAA64.EFI"
sudo cp "$SCRIPT_DIR/hello.txt" $DISK_MOUNT_DIR
sudo cp "$SCRIPT_DIR/very_long_long_example_text.TXT" $DISK_MOUNT_DIR
sync
sudo umount $DISK_MOUNT_DIR

qemu-system-aarch64 \
  -M virt,gic-version=3,secure=off,virtualization=on \
  -global virtio-mmio.force-legacy=off \
  -cpu cortex-a53 -smp 4 -m 4G \
  -bios $SCRIPT_DIR/../../test/RELEASEAARCH64_QEMU_EFI.fd \
  -nographic \
  -semihosting-config enable=on,target=native \
  -no-reboot -no-shutdown \
  -drive file=$DISK_IMG,format=raw,if=none,media=disk,id=disk \
  -device virtio-blk-device,bus=virtio-mmio-bus.0,drive=disk

RETCODE=$?

if [ $RETCODE -eq 0 ]; then
    exit 0
elif [ $RETCODE -eq 1 ]; then
    printf "\nFailed\n"
    exit 1
fi
