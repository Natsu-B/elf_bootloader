#!/bin/sh

PATH_TO_ELF="$1"

# get absolute path
SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)

DISK_IMG="$SCRIPT_DIR/../bin/fat32_virtio.img"
DISK_MOUNT_DIR="$SCRIPT_DIR/../mnt/"

mkdir -p "$SCRIPT_DIR/../bin"

# 64 MiB covers the test data while sparse allocation keeps setup cheap.
truncate -s 64M "$DISK_IMG"
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

QEMU_GDB_ARGS=""
if [ -n "$XTASK_QEMU_GDB_SOCKET" ]; then
    rm -f "$XTASK_QEMU_GDB_SOCKET"
    QEMU_GDB_ARGS="-gdb unix:path=$XTASK_QEMU_GDB_SOCKET,server=on,wait=off"
fi

qemu-system-aarch64 \
  -M virt,gic-version=3,secure=off,virtualization=on \
  -global virtio-mmio.force-legacy=off \
  -cpu cortex-a53 -smp 4 -m 4G \
  -bios $SCRIPT_DIR/../../test/RELEASEAARCH64_QEMU_EFI.fd \
  -nographic \
  -semihosting-config enable=on,target=native \
  -no-reboot -no-shutdown \
  -drive file=$DISK_IMG,format=raw,if=none,media=disk,id=disk \
  -device virtio-blk-device,bus=virtio-mmio-bus.0,drive=disk \
  $QEMU_GDB_ARGS

RETCODE=$?

if [ $RETCODE -eq 0 ]; then
    # Mount the disk image again to check the created file
    sudo mount -o loop,offset=$((2048 * 512)) $DISK_IMG $DISK_MOUNT_DIR
    if [ ! -d "$DISK_MOUNT_DIR/testdir" ]; then
        echo "FAIL: testdir not found"
        sudo umount $DISK_MOUNT_DIR
        exit 1
    fi
    if [ ! -f "$DISK_MOUNT_DIR/testdir/testfile.txt" ]; then
        echo "FAIL: testfile.txt not found"
        sudo umount $DISK_MOUNT_DIR
        exit 1
    fi
    if [ "$(sudo cat $DISK_MOUNT_DIR/testdir/testfile.txt)" != "test content" ]; then
        echo "FAIL: testfile.txt content mismatch"
        sudo umount $DISK_MOUNT_DIR
        exit 1
    fi
    sudo umount $DISK_MOUNT_DIR
    echo "Host check: PASS"
    exit 0
else
    printf "\nFailed\n"
    exit "$RETCODE"
fi
