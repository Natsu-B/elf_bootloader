#!/bin/sh
set -eu

SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
if [ "$#" -ne 1 ] || [ -z "$1" ]; then echo "usage: $0 <path-to-test-elf>"; exit 1; fi

exec "$SCRIPT_DIR/../../../../scripts/run_uboot_qemu_test.sh" \
    "$1" "$SCRIPT_DIR/../bin" virt,gic-version=2,secure=off,virtualization=on cortex-a53 2G
