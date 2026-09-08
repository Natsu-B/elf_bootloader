#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
output="$repo_root/bin/x86_64/linux-l1-suspend.efi"
backend=${LINUX_SUSPEND_BACKEND:-outer-kvm}

die() {
    printf 'x86 Linux S3: %s\n' "$*" >&2
    exit 1
}

select_backend() {
    case "$backend" in
        direct-vmx) cpus=1; loader=bin/x86_64/x86-uefi-loader.efi; monitor=bin/x86_64/x86-uefi-monitor.efi ;;
        outer-kvm) cpus=2; loader=bin/x86_64/x86-uefi-kvm-loader.efi; monitor= ;;
        *) die 'LINUX_SUSPEND_BACKEND must be direct-vmx or outer-kvm' ;;
    esac
}

check_log() {
    local log=$1 bytes transcript LC_ALL=C
    [[ -f "$log" && -r "$log" ]] || die 'serial log unavailable'
    bytes=$(wc -c <"$log")
    ((bytes > 0 && bytes <= 2097152)) || die 'invalid serial log size'
    if IFS= read -r -d '' -n 2097153 transcript <"$log"; then
        die 'NUL or oversized serial log'
    fi
    bash "$repo_root/scripts/x86_64/run-uefi-smoke.sh" --check-backend-log "$backend" "$log" \
        || die 'backend provenance or monitor failure'
    awk -v backend="$backend" -v cpus="$cpus" '
        BEGIN { cycle=1; prefix="thin-hv: linux S3 " }
        function step(expected, next_state) {
            if ($0 != prefix expected) bad=1
            state=next_state
        }
        {
            sub(/\r$/, "")
            sub(/^\[[ ]*[0-9]+\.[0-9]+\] /, "")
            if (index($0, "thin-hv: backend=") == 1) backends++
            if ($0 == "thin-hv: private host state PASS") private_host++
            if ($0 ~ /Kernel panic|Oops:|BUG:|thin-hv: linux S3 FAIL|thin-hv: linux L1 L2 KVM FAIL/) bad=1
            if ($0 == "thin-hv: linux L1 L2 KVM PASS") {
                if (state == 2) state=3
                else if (state == 7) state=8
                else bad=1
                probes++
            } else if (index($0, prefix) == 1) {
                if (state == 0) step("begin backend=" backend " l1_cpus=" cpus, 1)
                else if (state == 1) step("CPUs online PASS before suspend cycles", 2)
                else if (state == 3) step("EFI runtime write PASS cycle=" cycle " variable=DriverFFFF", 4)
                else if (state == 4) step("suspend begin cycle=" cycle, 5)
                else if (state == 5) step("resume cycle=" cycle, 6)
                else if (state == 6) step("CPUs online PASS cycle=" cycle, 7)
                else if (state == 8) step("EFI runtime resume PASS cycle=" cycle " variable=DriverFFFF", 9)
                else if (state == 9) step("EFI runtime delete PASS cycle=" cycle " variable=DriverFFFF", 10)
                else if (state == 10) {
                    step("EFI runtime PASS cycle=" cycle, cycle == 3 ? 11 : 3)
                    cycle++
                } else if (state == 11) step("nested KVM PASS cycles=3", 12)
                else if (state == 12) step("poweroff requested", 13)
                else bad=1
            }
        }
        END {
            exit (bad || state != 13 || probes != 4 || cycle != 4 ||
                  backends != (backend == "direct-vmx" ? 2 : 1) ||
                  private_host != (backend == "direct-vmx" ? 1 : 0))
        }
    ' "$log" || die 'incomplete or out-of-order S3 lifecycle'
}

if [[ ${1:-} == --check-log ]]; then
    [[ $# == 3 ]] || die 'usage: --check-log BACKEND LOG'
    backend=$2
    select_backend
    check_log "$3"
    exit 0
fi
[[ $# == 0 ]] || die 'configure using LINUX_SUSPEND_BACKEND'
select_backend
printf 'x86 Linux S3: backend=%s environment=QEMU/kvm l1_cpus=%s\n' "$backend" "$cpus"

cd -- "$repo_root"
cargo xbuild x86 --release
env \
    LINUX_L1_INIT="$repo_root/scripts/x86_64/linux-l1-suspend-init" \
    LINUX_L1_EXTRA_MODULES= \
    LINUX_L1_CMDLINE="console=ttyS0,115200n8 earlycon=uart8250,io,0x3f8,115200n8 rdinit=/init maxcpus=$cpus panic=0 thin_hv_s3_backend=$backend" \
    scripts/x86_64/build-linux-uki.sh "$output"
env \
    X86_UEFI_BACKEND="$backend" \
    X86_MONITOR_IMAGE="$monitor" \
    X86_UEFI_ACCEL=kvm \
    X86_UEFI_PHYSICAL_POLICY=0 \
    X86_UEFI_HOST_EXCEPTION_TEST=0 \
    X86_RETURN_MARKER= \
    X86_VARIABLE_MARKER= \
    X86_GUEST_MARKER='thin-hv: linux S3 poweroff requested' \
    X86_GUEST_FAILURE_MARKER='thin-hv: linux S3 FAIL' \
    X86_UEFI_TIMEOUT_SECONDS=90 \
    X86_UEFI_MEMORY=1G \
    X86_UEFI_SMP="$cpus" \
    X86_UEFI_CPU='host,+vmx,-hypervisor,kvm=off' \
    X86_UEFI_ACPI_S3=1 \
    X86_UEFI_WAKE_CYCLES=3 \
    X86_UEFI_ALLOW_REBOOT=0 \
    X86_UEFI_REQUIRE_POWEROFF=1 \
    X86_UEFI_DATA_DISK= \
    X86_UEFI_USERNET=0 \
    scripts/x86_64/run-uefi-smoke.sh \
    "$loader" "$output"
check_log "$repo_root/bin/x86_64/serial.log"
