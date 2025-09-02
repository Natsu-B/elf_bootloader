#!/bin/sh

qemu-system-aarch64 \
  -M virt,gic-version=3,secure=off,virtualization=on \
  -smp 4 -bios bin/u-boot.bin -cpu cortex-a53 -m 4G \
  -nographic -device virtio-blk-device,drive=disk \
  -drive file=fat:rw:bin/,format=raw,if=none,media=disk,id=disk \
  -gdb tcp::1234 #-S