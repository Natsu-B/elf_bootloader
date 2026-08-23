#!/bin/sh

PATH_TO_ELF="$1"

# get absolute path
SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
DISK_IMG="$SCRIPT_DIR/../bin/virtio_blk_test.img"

rm -rf "$SCRIPT_DIR/../bin/EFI"
mkdir -p "$SCRIPT_DIR/../bin/EFI/BOOT/"
cp "${PATH_TO_ELF}" "$SCRIPT_DIR/../bin/EFI/BOOT/BOOTAA64.EFI"
# QEMU writes to the device, so each run starts from a fresh fixture copy.
cp "$SCRIPT_DIR/test.txt" "$DISK_IMG"

QEMU_GDB_ARGS=""
if [ -n "$XTASK_QEMU_GDB_SOCKET" ]; then
    rm -f "$XTASK_QEMU_GDB_SOCKET"
    QEMU_GDB_ARGS="-gdb unix:path=$XTASK_QEMU_GDB_SOCKET,server=on,wait=off"
fi

qemu-system-aarch64 \
  -M virt,gic-version=3,secure=off,virtualization=on \
  -global virtio-mmio.force-legacy=off \
  -cpu cortex-a53 -smp 4 -m 4G \
  -bios $SCRIPT_DIR/../../../test/RELEASEAARCH64_QEMU_EFI.fd \
  -nographic \
  -semihosting-config enable=on,target=native \
  -no-reboot -no-shutdown \
  -drive id=drive0,file=$DISK_IMG,format=raw,if=none \
  -device virtio-blk-device,drive=drive0,bus=virtio-mmio-bus.0 \
  -drive file=fat:rw:$SCRIPT_DIR/../bin,format=raw,if=none,media=disk,id=disk \
  -device virtio-blk-device,drive=disk,bus=virtio-mmio-bus.1 \
  $QEMU_GDB_ARGS

RETCODE=$?

if [ "$RETCODE" -eq 0 ]; then
    exit 0
else
    printf "\nFailed\n"
    exit "$RETCODE"
fi
