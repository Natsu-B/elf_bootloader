#!/usr/bin/env bash
# Finite two-VM KVM context/memslot regression on one L1 CPU, not L2 SMP.
set -euo pipefail

repo_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)

die() {
    printf 'x86 Linux KVM lifecycle: %s\n' "$*" >&2
    exit 1
}

validate_cycles() {
    [[ "$1" =~ ^[1-9][0-9]{0,3}$ ]] && ((10#$1 <= 4096))
}

check_log() {
    local backend=$1 cycles=$2 log=$3 bytes transcript LC_ALL=C
    case "$backend" in direct-vmx|outer-kvm) ;; *) return 1 ;; esac
    validate_cycles "$cycles" || return 1
    [[ -f "$log" && -r "$log" ]] || return 1
    bytes=$(wc -c <"$log") || return 1
    [[ "$bytes" =~ ^[0-9]+$ ]] || return 1
    ((bytes > 0 && bytes <= 2097152)) || return 1
    # As in the UEFI fixture gates, reject NUL instead of silently discarding it.
    # The extra-byte cap also rejects a log that grows beyond the checked size.
    if IFS= read -r -d '' -n 2097153 transcript <"$log"; then
        return 1
    fi
    bash "$repo_root/scripts/x86_64/run-uefi-smoke.sh" --check-backend-log "$backend" "$log" || return 1
    awk -v backend="$backend" -v cycles="$cycles" '
        {
            sub(/\r$/, "")
            sub(/^\[[ ]*[0-9]+\.[0-9]+\] /, "")
            if ($0 ~ /Kernel panic|Oops:|BUG:|thin-hv: linux L1 L2 KVM FAIL|thin-hv: linux L2 lifecycle FAIL/) bad=1
            if (index($0, "thin-hv: linux L2 lifecycle ") != 1) next
            if ($0 == "thin-hv: linux L2 lifecycle begin backend=" backend " cycles=" cycles " l1_cpus=1") {
                if (begin || count || done || poweroff) bad=1
                begin++
            } else if (index($0, "thin-hv: linux L2 lifecycle cycle=") == 1) {
                count++
                if (begin != 1 || done || poweroff || count > cycles ||
                    $0 != "thin-hv: linux L2 lifecycle cycle=" count " KVM_RUN=IO port=0xe9 data=L2OK vm_contexts=2 rounds=8 io_in=16 io_out=96 halt=32 remaps=14 state_checks=16 sse_checks=16 long64_rounds=16 paging_checks=64 invlpg=16 cr3_writes=48 xmm16_checks=16 msr_checks=16 debug_checks=16 tsc_checks=16 teardown=explicit process_exit=0") bad=1
            } else if ($0 == "thin-hv: linux L2 lifecycle PASS backend=" backend " cycles=" cycles) {
                if (begin != 1 || count != cycles || done || poweroff) bad=1
                done++
            } else if ($0 == "thin-hv: linux L2 lifecycle poweroff requested") {
                if (done != 1 || poweroff) bad=1
                poweroff++
            } else bad=1
        }
        END { exit (bad || begin != 1 || count != cycles || done != 1 || poweroff != 1) }
    ' "$log"
}

# Pure log checks are called by the existing xtask host-test suite; no VM starts.
if [[ ${1:-} == --check-log ]]; then
    [[ $# == 4 ]] || die 'usage: --check-log BACKEND CYCLES LOG'
    check_log "$2" "$3" "$4" || die 'lifecycle evidence rejected'
    exit 0
fi
[[ $# == 0 ]] || die 'configure with LINUX_KVM_BACKEND, LINUX_KVM_CYCLES, and LINUX_KVM_TIMEOUT_SECONDS'

backend=${LINUX_KVM_BACKEND:-direct-vmx}
cycles=${LINUX_KVM_CYCLES:-64}
timeout_seconds=${LINUX_KVM_TIMEOUT_SECONDS:-300}
memory=${LINUX_KVM_MEMORY:-2G}
case "$memory" in 2G|4G|12G) ;; *) die 'LINUX_KVM_MEMORY must be 2G, 4G or 12G' ;; esac
host_xstate_test=${LINUX_KVM_HOST_XSTATE_TEST:-0}
[[ "$host_xstate_test" =~ ^[01]$ ]] || die 'LINUX_KVM_HOST_XSTATE_TEST must be 0 or 1'
[[ "$host_xstate_test" == 0 || "$backend" == direct-vmx ]] || die 'host XSTATE fixture requires project Direct L0'
validate_cycles "$cycles" || die 'LINUX_KVM_CYCLES must be an integer in 1..4096 without leading zeros'
[[ "$timeout_seconds" =~ ^[1-9][0-9]{0,3}$ ]] && ((10#$timeout_seconds <= 3600)) \
    || die 'LINUX_KVM_TIMEOUT_SECONDS must be an integer in 1..3600 without leading zeros'
case "$backend" in
    direct-vmx)
        loader="$repo_root/bin/x86_64/x86-uefi-loader.efi"
        monitor="$repo_root/bin/x86_64/x86-uefi-monitor.efi"
        role=project-l0
        ;;
    outer-kvm)
        loader="$repo_root/bin/x86_64/x86-uefi-kvm-loader.efi"
        monitor=
        role=reference
        ;;
    *) die 'LINUX_KVM_BACKEND must be direct-vmx or outer-kvm; no backend fallback exists' ;;
esac
if ((host_xstate_test)); then
    loader="$repo_root/bin/x86_64/x86-uefi-host-xstate-loader.efi"
    monitor="$repo_root/bin/x86_64/x86-uefi-host-xstate-monitor.efi"
fi
output="$repo_root/bin/x86_64/linux-l1-kvm-$backend.efi"
serial_log="$repo_root/bin/x86_64/serial.log"

cd -- "$repo_root"
cargo xbuild x86 --release
env \
    LINUX_L1_INIT="$repo_root/scripts/x86_64/linux-l1-init" \
    LINUX_L1_KVM_PROBE="$repo_root/scripts/x86_64/linux-l1-kvm-probe.c" \
    LINUX_L1_EXTRA_MODULES= \
    LINUX_L1_CMDLINE="console=ttyS0,115200n8 earlycon=uart8250,io,0x3f8,115200n8 rdinit=/init maxcpus=1 panic=0 thin_hv_kvm_backend=$backend thin_hv_kvm_cycles=$cycles" \
    scripts/x86_64/build-linux-uki.sh "$output"

env \
    X86_UEFI_BACKEND="$backend" \
    X86_UEFI_ACCEL=kvm \
    X86_MONITOR_IMAGE="$monitor" \
    X86_UEFI_CPU='host,+vmx,-hypervisor,kvm=off' \
    X86_UEFI_MEMORY="$memory" \
    X86_UEFI_SMP=1 \
    X86_UEFI_GUEST_LOCATION=guest \
    X86_UEFI_ALLOW_REBOOT=0 \
    X86_UEFI_REQUIRE_POWEROFF=1 \
    X86_UEFI_ACPI_S3=0 \
    X86_UEFI_WAKE_CYCLES=0 \
    X86_UEFI_DATA_DISK= \
    X86_UEFI_USERNET=0 \
    X86_UEFI_TIMEOUT_SECONDS="$timeout_seconds" \
    X86_RETURN_MARKER="thin-hv: linux L2 lifecycle PASS backend=$backend cycles=$cycles" \
    X86_VARIABLE_MARKER= \
    X86_GUEST_MARKER='thin-hv: linux L2 lifecycle poweroff requested' \
    X86_GUEST_FAILURE_MARKER='thin-hv: linux L2 lifecycle FAIL' \
    scripts/x86_64/run-uefi-smoke.sh "$loader" "$output"

check_log "$backend" "$cycles" "$serial_log" || die 'lifecycle evidence rejected'
if ((host_xstate_test)); then
    [[ $(grep -Fxc $'thin-hv: host xstate clobber fixture armed\r' "$serial_log") == 1 ]] \
        || die 'host XSTATE clobber fixture did not run exactly once'
fi
printf 'x86 Linux KVM lifecycle: PASS backend=%s role=%s cycles=%s environment=QEMU/kvm (not physical hardware)\n' \
    "$backend" "$role" "$cycles"
