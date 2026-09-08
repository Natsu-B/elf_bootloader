#!/usr/bin/env bash
# Real Linux L2 boot/workload/recreate A/B test using the existing UEFI runner.
set -euo pipefail
repo_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
die() { printf 'x86 Linux L2 OS: %s\n' "$*" >&2; exit 1; }
check_log() {
    local backend=$1 log=$2 bytes transcript LC_ALL=C
    case "$backend" in direct-vmx|outer-kvm) ;; *) return 1 ;; esac
    [[ -f "$log" && -r "$log" ]] || return 1
    bytes=$(wc -c <"$log") || return 1
    [[ "$bytes" =~ ^[0-9]+$ ]] && ((bytes > 0 && bytes <= 2097152)) || return 1
    if IFS= read -r -d '' -n 2097153 transcript <"$log"; then return 1; fi
    bash "$repo_root/scripts/x86_64/run-uefi-smoke.sh" --check-backend-log "$backend" "$log" || return 1
    awk -v backend="$backend" '
        {
            sub(/\r$/, "")
            if ($0 ~ /Kernel panic|Oops:|BUG:|thin-hv: linux (L2 OS|OS L2) FAIL|Segmentation fault/) bad=1
            if ($0 == "thin-hv: linux L2 OS begin backend=" backend " boots=6 l1_cpus=1") {
                if (begin || boot || done) bad=1
                begin++
            } else if ($0 ~ /^thin-hv: linux L2 OS start /) {
                if (begin != 1 || active || done || boot >= 6) bad=1
                boot++; cpus = boot % 2 ? 1 : 2
                if ($0 != "thin-hv: linux L2 OS start boot=" boot " cpus=" cpus " accel=kvm") bad=1
                active=1; guest_begin=0; guest_pass=0; shutdown=0
            } else if ($0 ~ /^L2: thin-hv: linux OS L2 begin /) {
                if (!active || guest_begin || guest_pass || shutdown || $0 != "L2: thin-hv: linux OS L2 begin cpus=" cpus) bad=1
                guest_begin++
            } else if ($0 ~ /^L2: thin-hv: linux OS L2 PASS /) {
                if (!active || guest_begin != 1 || guest_pass || shutdown ||
                    $0 != "L2: thin-hv: linux OS L2 PASS cpus=" cpus " workers=2 hashes=32 memory_mib=32 copy_mib=32 sha256=83ee47245398adee79bd9c0a8bc57b821e92aba10f5f9ade8a5d1fae4d8c4302 boot=" boot " disk_mib=32 disk_persist=" (boot == 1 ? 0 : 1) " net_packets=3") bad=1
                guest_pass++
            } else if ($0 ~ /^L2: (\[[ ]*[0-9]+\.[0-9]+\] )?reboot: Power down$/) {
                if (!active || guest_pass != 1 || shutdown) bad=1
                shutdown++
            } else if ($0 ~ /^thin-hv: linux L2 OS exit /) {
                if (!active || guest_begin != 1 || guest_pass != 1 || shutdown != 1 ||
                    $0 != "thin-hv: linux L2 OS exit boot=" boot " cpus=" cpus " process_exit=0") bad=1
                active=0; exited++
            } else if ($0 == "thin-hv: linux L2 OS PASS backend=" backend " boots=6 l1_cpus=1 l2_cpus=1,2") {
                if (begin != 1 || active || exited != 6 || done) bad=1
                done++
            } else if ($0 == "thin-hv: linux L2 OS poweroff requested") {
                if (done != 1 || poweroff) bad=1
                poweroff++
            } else if ($0 ~ /^(L2: )?thin-hv: linux (L2 OS|OS L2) /) bad=1
        }
        END { exit (bad || begin != 1 || boot != 6 || exited != 6 || active || done != 1 || poweroff != 1) }
    ' "$log"
}
if [[ ${1:-} == --check-log ]]; then
    [[ $# == 3 ]] || die 'usage: --check-log BACKEND LOG'
    check_log "$2" "$3" || die 'incomplete or failed L2 OS evidence'
    exit 0
fi
[[ $# == 0 ]] || die 'configure LINUX_L2_OS_BACKEND'
backend=${LINUX_L2_OS_BACKEND:-direct-vmx}
case "$backend" in
    direct-vmx) loader=x86-uefi-loader.efi; monitor="$repo_root/bin/x86_64/x86-uefi-monitor.efi" ;;
    outer-kvm) loader=x86-uefi-kvm-loader.efi; monitor= ;;
    *) die 'backend must be direct-vmx or outer-kvm; no fallback exists' ;;
esac
cd -- "$repo_root"
cargo xbuild x86 --release
output="$repo_root/bin/x86_64/linux-l2-os-$backend.efi"
env LINUX_L1_L2_OS=1 LINUX_L1_KVM_SELFTEST= LINUX_L1_EXTRA_MODULES= \
    LINUX_L1_INIT="$repo_root/scripts/x86_64/linux-l1-l2-os-init" \
    LINUX_L1_CMDLINE="console=ttyS0,115200n8 rdinit=/init maxcpus=1 panic=0 thin_hv_l2_os_backend=$backend" \
    bash scripts/x86_64/build-linux-uki.sh "$output"
env X86_UEFI_BACKEND="$backend" X86_UEFI_ACCEL=kvm X86_MONITOR_IMAGE="$monitor" \
    X86_UEFI_PHYSICAL_POLICY=0 X86_UEFI_HOST_EXCEPTION_TEST=0 \
    X86_UEFI_CPU='host,+vmx,-hypervisor,kvm=off' X86_UEFI_MEMORY=2G X86_UEFI_SMP=1 \
    X86_UEFI_GUEST_LOCATION=guest X86_UEFI_ALLOW_REBOOT=0 X86_UEFI_REQUIRE_POWEROFF=1 \
    X86_UEFI_ACPI_S3=0 X86_UEFI_WAKE_CYCLES=0 X86_UEFI_DATA_DISK= X86_UEFI_USERNET=0 \
    X86_UEFI_TIMEOUT_SECONDS=900 X86_VARIABLE_MARKER= \
    X86_RETURN_MARKER="thin-hv: linux L2 OS PASS backend=$backend boots=6 l1_cpus=1 l2_cpus=1,2" \
    X86_GUEST_MARKER='thin-hv: linux L2 OS poweroff requested' \
    X86_GUEST_FAILURE_MARKER='thin-hv: linux L2 OS FAIL' \
    bash scripts/x86_64/run-uefi-smoke.sh "$repo_root/bin/x86_64/$loader" "$output"
check_log "$backend" "$repo_root/bin/x86_64/serial.log" || die 'incomplete or failed L2 OS evidence'
printf 'x86 Linux L2 OS: PASS backend=%s boots=6 l1_cpus=1 l2_cpus=1,2 environment=QEMU/KVM (not physical hardware)\n' "$backend"
