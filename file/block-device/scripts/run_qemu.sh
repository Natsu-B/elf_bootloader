#!/bin/sh

PATH_TO_ELF="$1"

# get absolute path
SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)

rm -rf "$SCRIPT_DIR/../bin/EFI"
mkdir -p "$SCRIPT_DIR/../bin/EFI/BOOT/"
cp "${PATH_TO_ELF}" "$SCRIPT_DIR/../bin/EFI/BOOT/BOOTAA64.EFI"

qemu-system-aarch64 \
  -M virt,gic-version=3,secure=off,virtualization=on \
  -global virtio-mmio.force-legacy=off \
  -cpu cortex-a53 -smp 4 -m 4G \
  -bios $SCRIPT_DIR/../../../test/RELEASEAARCH64_QEMU_EFI.fd \
  -nographic \
  -semihosting-config enable=on,target=native \
  -no-reboot -no-shutdown \
  -drive id=drive0,file=$SCRIPT_DIR/test.txt,format=raw,if=none \
  -device virtio-blk-device,drive=drive0,bus=virtio-mmio-bus.0 \
  -drive file=fat:rw:$SCRIPT_DIR/../bin,format=raw,if=none,media=disk,id=disk \
  -device virtio-blk-device,drive=disk,bus=virtio-mmio-bus.1

RETCODE=$?

if [ $RETCODE -eq 0 ]; then
    exit 0
elif [ $RETCODE -eq 1 ]; then
    printf "\nFailed\n"
    exit 1
fi
