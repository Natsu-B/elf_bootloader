#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
output="$repo_root/bin/x86_64/linux-l1-suspend.efi"

cd -- "$repo_root"
cargo xbuild x86 --release
env \
    LINUX_L1_INIT="$repo_root/scripts/x86_64/linux-l1-suspend-init" \
    LINUX_L1_CMDLINE='console=ttyS0,115200n8 earlycon=uart8250,io,0x3f8,115200n8 rdinit=/init maxcpus=2 panic=-1' \
    scripts/x86_64/build-linux-uki.sh "$output"
env \
    X86_MONITOR_IMAGE= \
    X86_RETURN_MARKER= \
    X86_VARIABLE_MARKER= \
    X86_GUEST_MARKER='thin-hv: linux S3 nested KVM PASS cycles=3' \
    X86_UEFI_TIMEOUT_SECONDS=90 \
    X86_UEFI_MEMORY=1G \
    X86_UEFI_SMP=2 \
    X86_UEFI_CPU='host,+vmx,-hypervisor,kvm=off' \
    X86_UEFI_ACPI_S3=1 \
    X86_UEFI_WAKE_CYCLES=3 \
    scripts/x86_64/run-uefi-smoke.sh \
    bin/x86_64/x86-uefi-kvm-loader.efi "$output"
