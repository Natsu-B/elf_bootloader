#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
output="$repo_root/bin/x86_64/linux-l1-soak.efi"
serial_log="$repo_root/bin/x86_64/serial.log"
hash_rounds=${LINUX_SOAK_HASH_ROUNDS:-40}
l2_probes=${LINUX_SOAK_L2_PROBES:-1000}
timeout_seconds=${LINUX_SOAK_TIMEOUT_SECONDS:-900}
backend=${LINUX_SOAK_BACKEND:-outer-kvm}

die() {
    printf 'x86 Linux soak: %s\n' "$*" >&2
    exit 1
}

require_marker_count() {
    local expected=$1 marker=$2 count

    count=$(grep -Fc -- "$marker" "$serial_log" || true)
    [[ "$count" == "$expected" ]] || \
        die "expected $expected '$marker' markers, observed $count"
}

select_backend() {
    case "$backend" in
        direct-vmx)
            cpus=1
            loader=bin/x86_64/x86-uefi-loader.efi
            monitor=bin/x86_64/x86-uefi-monitor.efi
            ;;
        outer-kvm)
            cpus=2
            loader=bin/x86_64/x86-uefi-kvm-loader.efi
            monitor=
            ;;
        *) die 'LINUX_SOAK_BACKEND must be direct-vmx or outer-kvm' ;;
    esac
}

check_log() {
    local transcript bytes LC_ALL=C
    [[ -f "$serial_log" && -r "$serial_log" ]] || die 'serial log unavailable'
    bytes=$(wc -c <"$serial_log")
    ((bytes > 0 && bytes <= 2097152)) || die 'invalid serial log size'
    if IFS= read -r -d '' -n 2097153 transcript <"$serial_log"; then
        die 'NUL in serial log'
    fi
    bash "$repo_root/scripts/x86_64/run-uefi-smoke.sh" --check-backend-log "$backend" "$serial_log" \
        || die 'backend provenance or monitor failure'
    if grep -Eq -- 'Kernel panic|Oops:|BUG:|thin-hv-soak: FAIL' "$serial_log"; then
        die 'failure signature in serial log'
    fi
    if [[ "$backend" == outer-kvm ]]; then
        require_marker_count 2 'thin-hv: uefi entry'
        require_marker_count 2 'thin-hv: backend=outer-kvm role=reference'
        require_marker_count 2 'thin-hv: trusted outer KVM direct chainload profile=2 resident_runtime=0'
    else
        # Loader and runtime image each enter once per boot.
        require_marker_count 4 'thin-hv: uefi entry'
        require_marker_count 4 'thin-hv: backend=direct-vmx role=project-l0'
        require_marker_count 2 'thin-hv: private host state PASS'
    fi
    require_marker_count 2 'thin-hv-soak: L1 boot begin'
    require_marker_count 2 "thin-hv-soak: topology backend=$backend l1_cpus=$cpus"
    require_marker_count 2 'thin-hv-soak: nested KVM ready'
    require_marker_count 2 "thin-hv-soak: repeated L2 PASS count=$l2_probes "
    require_marker_count 2 "thin-hv-soak: cpu-memory PASS bytes=134217728 workers=2 hashes=$((hash_rounds * 2)) "
    require_marker_count 2 'thin-hv-soak: usernet PASS packets=5'
    require_marker_count 1 'thin-hv-soak: virtio-blk PASS phase=write bytes=134217728 '
    require_marker_count 1 'thin-hv-soak: virtio-blk PASS phase=reboot-read bytes=134217728 '
    awk -v backend="$backend" -v cpus="$cpus" '
        { sub(/\r$/, "") }
        /^thin-hv-soak: topology / {
            if ($0 != "thin-hv-soak: topology backend=" backend " l1_cpus=" cpus) bad=1
        }
        /^thin-hv-soak: phase=/ {
            if ($0 != "thin-hv-soak: phase=1" && $0 != "thin-hv-soak: phase=2") bad=1
        }
        /^thin-hv-soak: phase=1$/ { if (state != 0) bad=1; state=1 }
        /^thin-hv-soak: reboot requested$/ { if (state != 1) bad=1; state=2 }
        /^thin-hv-soak: phase=2$/ { if (state != 2) bad=1; state=3 }
        /^thin-hv-soak: PASS phase=2 / { if (state != 3) bad=1; state=4 }
        /^thin-hv-soak: poweroff requested$/ { if (state != 4) bad=1; state=5 }
        END { exit (bad || state != 5) }
    ' "$serial_log" || die 'incomplete or out-of-order reboot lifecycle'
}

if [[ ${1:-} == --check-log ]]; then
    [[ $# == 5 ]] || die 'usage: --check-log BACKEND HASH_ROUNDS L2_PROBES LOG'
    backend=$2 hash_rounds=$3 l2_probes=$4 serial_log=$5
    select_backend
    [[ "$hash_rounds" =~ ^[1-9][0-9]{0,3}$ && "$l2_probes" =~ ^[1-9][0-9]{0,3}$ ]] \
        || die 'invalid coverage count'
    check_log
    exit 0
fi
[[ $# == 0 ]] || die 'configure using LINUX_SOAK_* environment variables'
select_backend

[[ "$hash_rounds" =~ ^[1-9][0-9]{0,3}$ ]] || die 'LINUX_SOAK_HASH_ROUNDS must be in 1..9999'
[[ "$l2_probes" =~ ^[1-9][0-9]{0,3}$ ]] || die 'LINUX_SOAK_L2_PROBES must be in 1..9999'
[[ "$timeout_seconds" =~ ^[1-9][0-9]{0,3}$ ]] && ((timeout_seconds <= 3600)) \
    || die 'LINUX_SOAK_TIMEOUT_SECONDS must be in 1..3600'
printf 'x86 Linux soak: backend=%s environment=QEMU/kvm l1_cpus=%s\n' "$backend" "$cpus"

cd -- "$repo_root"
cargo xbuild x86 --release
env \
    LINUX_L1_INIT="$repo_root/scripts/x86_64/linux-l1-soak-init" \
    LINUX_L1_EXTRA_MODULES='virtio_pci virtio_blk virtio_net' \
    LINUX_L1_CMDLINE="console=ttyS0,115200n8 earlycon=uart8250,io,0x3f8,115200n8 rdinit=/init maxcpus=$cpus panic=0 thin_hv_soak_backend=$backend thin_hv_soak_hash_rounds=$hash_rounds thin_hv_soak_l2_probes=$l2_probes" \
    scripts/x86_64/build-linux-uki.sh "$output"

disk=$(mktemp "$repo_root/bin/x86_64/linux-soak-disk.XXXXXX.raw")
trap 'rm -f -- "$disk"' EXIT
truncate -s 192M "$disk"

env \
    X86_UEFI_BACKEND="$backend" \
    X86_UEFI_PCI_PROFILE="${LINUX_SOAK_PCI_PROFILE:-firmware-default}" \
    X86_MONITOR_IMAGE="$monitor" \
    X86_UEFI_ACCEL=kvm \
    X86_UEFI_PHYSICAL_POLICY=0 \
    X86_UEFI_HOST_EXCEPTION_TEST=0 \
    X86_RETURN_MARKER='thin-hv-soak: PASS phase=2' \
    X86_VARIABLE_MARKER= \
    X86_GUEST_MARKER='thin-hv-soak: poweroff requested' \
    X86_GUEST_FAILURE_MARKER='thin-hv-soak: FAIL' \
    X86_UEFI_TIMEOUT_SECONDS="$timeout_seconds" \
    X86_UEFI_MEMORY=2G \
    X86_UEFI_SMP="$cpus" \
    X86_UEFI_CPU='host,+vmx,-hypervisor,kvm=off' \
    X86_UEFI_ALLOW_REBOOT=1 \
    X86_UEFI_ACPI_S3=0 \
    X86_UEFI_WAKE_CYCLES=0 \
    X86_UEFI_REQUIRE_POWEROFF=1 \
    X86_UEFI_DATA_DISK="$disk" \
    X86_UEFI_USERNET=1 \
    scripts/x86_64/run-uefi-smoke.sh \
    "$loader" "$output"

check_log
