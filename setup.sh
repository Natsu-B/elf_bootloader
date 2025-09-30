#!/bin/sh

SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
BIN="$SCRIPT_DIR/bin"

DISK_IMG="$BIN/disk.img"
DTB_FILE="$BIN/qemu.dtb"
DTB_MOD_FILE="$BIN/qemu_mod.dtb"
DTS_FILE="$BIN/qemu.dts"

TMP_DTS="$(mktemp)"

BOOTARGS='bootargs = "root=/dev/vda2 rw rootwait console=ttyAMA0,115200 earlycon=pl011,0x09000000";'

qemu-system-aarch64 \
  -M virt,gic-version=3,secure=off,virtualization=on \
  -global virtio-mmio.force-legacy=off \
  -smp 4 -bios "$BIN/u-boot.bin" -cpu cortex-a55 -m 4G \
  -nographic \
  -device virtio-blk-device,drive=disk \
  -drive file="$DISK_IMG",format=raw,if=none,media=disk,id=disk \
  -gdb tcp::1234 -machine dumpdtb=$DTB_FILE

dtc -I dtb -O dts -o $DTS_FILE $DTB_FILE

if grep -q "bootargs" "$DTS_FILE"; then
    echo "Error: bootargs already exists in chosen node" >&2
    exit 1
fi

awk -v bootargs="$BOOTARGS" '
/^[[:space:]]*chosen[[:space:]]*\{/ {
    print $0
    print "        " bootargs
    next
}
{ print $0 }
' "$DTS_FILE" > "$TMP_DTS"

mv "$TMP_DTS" "$DTS_FILE"

dtc -I dts -O dtb -o $DTB_MOD_FILE $DTS_FILE