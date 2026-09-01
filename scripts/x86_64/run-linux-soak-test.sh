#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
output="$repo_root/bin/x86_64/linux-l1-soak.efi"
hash_rounds=${LINUX_SOAK_HASH_ROUNDS:-40}
l2_probes=${LINUX_SOAK_L2_PROBES:-1000}
timeout_seconds=${LINUX_SOAK_TIMEOUT_SECONDS:-900}

die() {
    printf 'x86 Linux soak: %s\n' "$*" >&2
    exit 1
}

[[ "$hash_rounds" =~ ^[1-9][0-9]*$ ]] || die 'LINUX_SOAK_HASH_ROUNDS must be a positive integer'
[[ "$l2_probes" =~ ^[1-9][0-9]*$ ]] || die 'LINUX_SOAK_L2_PROBES must be a positive integer'
[[ "$timeout_seconds" =~ ^[1-9][0-9]*$ ]] || die 'LINUX_SOAK_TIMEOUT_SECONDS must be a positive integer'

cd -- "$repo_root"
cargo xbuild x86 --release
env \
    LINUX_L1_INIT="$repo_root/scripts/x86_64/linux-l1-soak-init" \
    LINUX_L1_EXTRA_MODULES='virtio_pci virtio_blk virtio_net' \
    LINUX_L1_CMDLINE="console=ttyS0,115200n8 earlycon=uart8250,io,0x3f8,115200n8 rdinit=/init maxcpus=2 panic=-1 thin_hv_soak_hash_rounds=$hash_rounds thin_hv_soak_l2_probes=$l2_probes" \
    scripts/x86_64/build-linux-uki.sh "$output"

disk=$(mktemp "$repo_root/bin/x86_64/linux-soak-disk.XXXXXX.raw")
trap 'rm -f -- "$disk"' EXIT
truncate -s 192M "$disk"

env \
    X86_MONITOR_IMAGE= \
    X86_RETURN_MARKER= \
    X86_VARIABLE_MARKER= \
    X86_GUEST_MARKER='thin-hv-soak: PASS phase=2' \
    X86_UEFI_TIMEOUT_SECONDS="$timeout_seconds" \
    X86_UEFI_MEMORY=2G \
    X86_UEFI_SMP=2 \
    X86_UEFI_CPU='host,+vmx,-hypervisor,kvm=off' \
    X86_UEFI_ALLOW_REBOOT=1 \
    X86_UEFI_DATA_DISK="$disk" \
    X86_UEFI_USERNET=1 \
    scripts/x86_64/run-uefi-smoke.sh \
    bin/x86_64/x86-uefi-kvm-loader.efi "$output"
