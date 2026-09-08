#!/usr/bin/env bash
set -euo pipefail
repo_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
manifest="$repo_root/scripts/x86_64/linux-l2-kunit-cases.txt"
classifier="$repo_root/scripts/x86_64/check-kunit-case.awk"
die() { printf 'x86 KVM unit matrix: %s\n' "$*" >&2; exit 1; }
check_text_file() {
    local file=$1 limit=$2 bytes transcript
    [[ -f "$file" && -r "$file" ]] || return 1
    bytes=$(wc -c <"$file") || return 1
    ((bytes > 0 && bytes <= limit)) || return 1
    if IFS= read -r -d '' -n "$((limit + 1))" transcript <"$file"; then return 1; fi
}
check_manifest() {
    check_text_file "$1" 16384 || return 1
    awk -F'|' '
        /^#/ || /^$/ { next }
        {
            if (NF != 7 || $1 !~ /^[a-z][a-z0-9_-]*$/ || seen[$1]++ ||
                $2 !~ /^[a-z][a-z0-9_-]*\.flat$/ || $3 !~ /^[1-4]$/ ||
                $4 !~ /^[0-9]+$/ || $4 < 128 || $4 > 2048 ||
                $5 !~ /^[0-9]+$/ || $5 < 1 || $5 > 300 ||
                $6 !~ /^[A-Za-z0-9_.,+=-]+$/ || $7 !~ /^[-a-z0-9_]+$/) bad=1
            count++
        }
        END { exit (bad || count < 1 || count > 100) }
    ' "$1"
}
if [[ ${1:-} == --check-manifest ]]; then
    [[ $# == 2 ]] || die 'usage: --check-manifest FILE'
    check_manifest "$2" || die 'invalid bounded case manifest'
    exit 0
fi
check_manifest "$manifest" || die 'invalid bounded case manifest'
if [[ ${1:-} == --check-case ]]; then
    [[ $# == 4 && "$3" =~ ^[0-9]{1,3}$ && "$3" -le 255 ]] || die 'usage: --check-case NAME STATUS LOG'
    argument=$(awk -F'|' -v name="$2" '$1 == name { print $7 }' "$manifest")
    [[ -n "$argument" ]] && check_text_file "$4" 4194304 || die 'unknown case or invalid bounded log'
    exec awk -v name="$2" -v argument="$argument" -v status="$3" -f "$classifier" "$4"
fi
if [[ ${1:-} == --check-log ]]; then
    [[ $# == 4 ]] || die 'usage: --check-log BACKEND SELECTION LOG'
    case "$2" in direct-vmx|outer-kvm) ;; *) die 'invalid backend' ;; esac
    check_text_file "$4" 67108864 || die 'invalid bounded log'
    bash "$repo_root/scripts/x86_64/run-uefi-smoke.sh" --check-backend-log "$2" "$4"
    awk -v matrix=1 -v backend="$2" -v selection="$3" -v manifest="$manifest" \
        -f "$classifier" "$4" || die 'incomplete matrix or one or more FAIL/SKIP cases (not all PASS)'
    exit 0
fi
[[ $# == 0 ]] || die 'configure LINUX_KUNIT_BACKEND, LINUX_L2_QEMU and LINUX_L2_KUNIT_DIR'
backend=${LINUX_KUNIT_BACKEND:-direct-vmx}
selection=${LINUX_KUNIT_CASE:-all}
if [[ "$selection" != all ]]; then
    awk -F'|' -v name="$selection" '$1 == name { found=1 } END { exit !found }' "$manifest" || die 'unknown case'
fi
[[ -d ${LINUX_L2_KUNIT_DIR:-} ]] || die 'set LINUX_L2_KUNIT_DIR to pinned upstream x86 flat outputs'
case "$backend" in
    direct-vmx) loader=x86-uefi-loader.efi; monitor="$repo_root/bin/x86_64/x86-uefi-monitor.efi" ;;
    outer-kvm) loader=x86-uefi-kvm-loader.efi; monitor= ;;
    *) die 'invalid backend; no fallback exists' ;;
esac
cd -- "$repo_root"
cargo xbuild x86 --release
output="$repo_root/bin/x86_64/linux-kunit-$backend.efi"
env LINUX_L1_L2_OS=1 LINUX_L1_KVM_SELFTEST= LINUX_L1_EXTRA_MODULES= \
    LINUX_L1_INIT="$repo_root/scripts/x86_64/linux-l1-kunit-init" \
    LINUX_L1_CMDLINE="console=ttyS0,115200n8 rdinit=/init maxcpus=1 panic=0 thin_hv_kunit_backend=$backend thin_hv_kunit_case=$selection" \
    bash scripts/x86_64/build-linux-uki.sh "$output"
env X86_UEFI_BACKEND="$backend" X86_UEFI_ACCEL=kvm X86_MONITOR_IMAGE="$monitor" \
    X86_UEFI_PCI_PROFILE=q35-smoke-1g \
    X86_UEFI_PHYSICAL_POLICY=0 X86_UEFI_HOST_EXCEPTION_TEST=0 \
    X86_UEFI_CPU='host,+vmx,-hypervisor,kvm=off' X86_UEFI_MEMORY=4G X86_UEFI_SMP=1 \
    X86_UEFI_GUEST_LOCATION=guest X86_UEFI_ALLOW_REBOOT=0 X86_UEFI_REQUIRE_POWEROFF=1 \
    X86_UEFI_ACPI_S3=0 X86_UEFI_WAKE_CYCLES=0 X86_UEFI_DATA_DISK= X86_UEFI_USERNET=0 \
    X86_UEFI_TIMEOUT_SECONDS=7200 X86_VARIABLE_MARKER= \
    X86_RETURN_MARKER="thin-hv: KVM unit matrix complete backend=$backend selection=$selection" \
    X86_GUEST_MARKER='thin-hv: KVM unit matrix poweroff requested' \
    X86_GUEST_FAILURE_MARKER='thin-hv: KVM unit matrix FAIL' \
    bash scripts/x86_64/run-uefi-smoke.sh "$repo_root/bin/x86_64/$loader" "$output"
bash "$0" --check-log "$backend" "$selection" "$repo_root/bin/x86_64/serial.log"
printf 'x86 KVM unit matrix: PASS backend=%s selection=%s environment=QEMU/KVM (not physical hardware)\n' "$backend" "$selection"
