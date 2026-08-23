#!/bin/sh
set -eu

if [ "$#" -ne 5 ] || [ -z "$1" ] || [ -z "$2" ] || [ -z "$3" ] || [ -z "$4" ] || [ -z "$5" ]; then
    echo "usage: $0 <path-to-test-elf> <fat-dir> <machine> <cpu> <memory>"
    exit 1
fi

PATH_TO_ELF=$1
FAT_DIR=$2
MACHINE=$3
CPU=$4
MEMORY=$5
SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
REPO_ROOT=$(cd "$SCRIPT_DIR/.." && pwd)
BIN=$(cd "$REPO_ROOT/bin" && pwd)
FAT_DIR=$(cd "$(dirname "$FAT_DIR")" && pwd)/$(basename "$FAT_DIR")
case $FAT_DIR in
    "$REPO_ROOT/arch_hal/aarch64_hal/gic/bin"|"$REPO_ROOT/arch_hal/aarch64_hal/paging/bin") ;;
    *) echo "unsupported FAT directory: $FAT_DIR"; exit 1 ;;
esac

rm -rf "$FAT_DIR"
mkdir -p "$FAT_DIR"
cp "$PATH_TO_ELF" "$FAT_DIR/elf-hypervisor.elf"
cp "$BIN/boot.scr" "$BIN/u-boot.bin" "$FAT_DIR"
if [ -f "$BIN/qemu.dtb" ]; then
    cp "$BIN/qemu.dtb" "$FAT_DIR/qemu.dtb"
fi

set -- qemu-system-aarch64 \
  -M "$MACHINE" \
  -global virtio-mmio.force-legacy=off \
  -smp 4 \
  -bios "$FAT_DIR/u-boot.bin" \
  -cpu "$CPU" -m "$MEMORY" \
  -nographic -no-reboot \
  -semihosting-config enable=on,target=native \
  -drive "file=fat:rw:$FAT_DIR,format=raw,if=none,media=disk,id=disk" \
  -device virtio-blk-device,drive=disk,bus=virtio-mmio-bus.0
if [ -n "${XTASK_QEMU_GDB_SOCKET:-}" ]; then
    rm -f "$XTASK_QEMU_GDB_SOCKET"
    set -- "$@" -gdb "unix:path=$XTASK_QEMU_GDB_SOCKET,server=on,wait=off"
fi

set +e
"$@"
RETCODE=$?
set -e

case $RETCODE in
    0) exit 0 ;;
    1) printf "\nFailed\n"; exit 1 ;;
    *) printf "\nUnexpected QEMU exit code: %s\n" "$RETCODE"; exit "$RETCODE" ;;
esac
