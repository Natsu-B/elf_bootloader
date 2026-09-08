#!/usr/bin/env bash
# Pinned-upstream KVM userspace tests; their guests are L2, not L3.
set -euo pipefail

repo_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
die() { printf 'x86 Linux KVM selftest: %s\n' "$*" >&2; exit 1; }
valid_test() { case "$1" in tsc_msrs_test|userspace_msr_exit_test|cr4_cpuid_sync_test|xcr0_cpuid_test|debug_regs) ;; *) return 1 ;; esac; }

check_log() {
    local backend=$1 test_name=$2 log=$3 bytes transcript LC_ALL=C
    case "$backend" in direct-vmx|outer-kvm) ;; *) return 1 ;; esac
    valid_test "$test_name" || return 1
    [[ -f "$log" && -r "$log" ]] || return 1
    bytes=$(wc -c <"$log") || return 1
    [[ "$bytes" =~ ^[0-9]+$ ]] && ((bytes > 0 && bytes <= 2097152)) || return 1
    if IFS= read -r -d '' -n 2097153 transcript <"$log"; then return 1; fi
    bash "$repo_root/scripts/x86_64/run-uefi-smoke.sh" --check-backend-log "$backend" "$log" || return 1
    awk -v backend="$backend" -v test_name="$test_name" '
        BEGIN {
            tap = test_name == "tsc_msrs_test" || test_name == "userspace_msr_exit_test"
            count = test_name == "tsc_msrs_test" ? 5 : (tap ? 4 : 1)
            names[1] = "msr_filter_allow"; names[2] = "msr_filter_deny"
            names[3] = "msr_permission_bitmap"; names[4] = "user_exit_msr_flags"
        }
        {
            sub(/\r$/, "")
            if ($0 ~ /Kernel panic|Oops:|BUG:|thin-hv: linux KVM selftest FAIL|Test Assertion Failure|^not ok |^Bail out!|^#.*FAIL|^#.*ERROR|# SKIP|# XFAIL|# XPASS/) bad=1
            if (index($0, "thin-hv: linux KVM selftest ") == 1) {
                if ($0 == "thin-hv: linux KVM selftest begin backend=" backend " test=" test_name " l1_cpus=1") {
                    if (begin || ended || done || poweroff) bad=1
                    begin++
                } else if ($0 == "thin-hv: linux KVM selftest exit backend=" backend " test=" test_name " process_exit=0") {
                    if (begin != 1 || (tap && (header != 1 || plan != 1 || passed != count || totals != 1)) ||
                        (test_name == "userspace_msr_exit_test" && suite != 1) || ended || done || poweroff) bad=1
                    ended++
                } else if ($0 == "thin-hv: linux KVM selftest PASS backend=" backend " test=" test_name " assertions=" count) {
                    if (ended != 1 || done || poweroff) bad=1
                    done++
                } else if ($0 == "thin-hv: linux KVM selftest poweroff requested") {
                    if (done != 1 || poweroff) bad=1
                    poweroff++
                } else bad=1
                next
            }
            if ($0 ~ /^TAP version |^1\.\.|^ok |^# Totals:|^# PASSED:/) {
                if (!tap) bad=1
                if (begin != 1 || ended || done || poweroff) bad=1
                if ($0 == "TAP version 13") { if (header || plan || passed || totals) bad=1; header++ }
                else if ($0 == "1.." count) { if (header != 1 || plan || passed || totals) bad=1; plan++ }
                else if ($0 ~ /^ok /) {
                    passed++
                    expected = test_name == "tsc_msrs_test" ? "ok " passed " stage " (passed + 1) " passed" : "ok " passed " user_msr." names[passed]
                    if (plan != 1 || totals || passed > count || $0 != expected) bad=1
                } else if ($0 == "# PASSED: 4 / 4 tests passed." && test_name == "userspace_msr_exit_test") {
                    if (passed != count || totals || suite) bad=1
                    suite++
                } else if ($0 == "# Totals: pass:" count " fail:0 xfail:0 xpass:0 skip:0 error:0") {
                    if (passed != count || totals) bad=1
                    totals++
                } else bad=1
            }
        }
        END { exit (bad || begin != 1 || ended != 1 || done != 1 || poweroff != 1) }
    ' "$log"
}

if [[ ${1:-} == --check-log ]]; then
    [[ $# == 4 ]] || die 'usage: --check-log BACKEND TEST LOG'
    check_log "$2" "$3" "$4" || die 'selftest evidence rejected'
    exit 0
fi
if [[ ${1:-} == --check-elf ]]; then
    [[ $# == 2 ]] || die 'usage: --check-elf ELF'
    exec bash "$repo_root/scripts/x86_64/build-linux-uki.sh" --check-selftest-elf "$2"
fi
[[ $# == 0 ]] || die 'configure LINUX_SELFTEST_BACKEND, LINUX_SELFTEST_NAME, and LINUX_SELFTEST_ELF'
backend=${LINUX_SELFTEST_BACKEND:-direct-vmx}
test_name=${LINUX_SELFTEST_NAME:-}
selftest=${LINUX_SELFTEST_ELF:-}
timeout_seconds=${LINUX_SELFTEST_TIMEOUT_SECONDS:-300}
valid_test "$test_name" || die 'unsupported pinned KVM selftest name'
# The three non-TAP upstream programs return 0 only after UCALL_DONE and all
# assertions. For those, assertions=1 counts the completed program, not its
# individual guest assertions; exit 4 (KSFT_SKIP) is always a failure here.
case "$test_name" in tsc_msrs_test) assertions=5 ;; userspace_msr_exit_test) assertions=4 ;; *) assertions=1 ;; esac
[[ "$timeout_seconds" =~ ^[1-9][0-9]{0,3}$ ]] && ((10#$timeout_seconds <= 3600)) || die 'timeout must be 1..3600 seconds'
bash "$repo_root/scripts/x86_64/build-linux-uki.sh" --check-selftest-elf "$selftest"
case "$backend" in
    direct-vmx)
        loader="$repo_root/bin/x86_64/x86-uefi-loader.efi"
        monitor="$repo_root/bin/x86_64/x86-uefi-monitor.efi"
        role=project-l0 ;;
    outer-kvm)
        loader="$repo_root/bin/x86_64/x86-uefi-kvm-loader.efi"
        monitor=
        role=reference ;;
    *) die 'backend must be direct-vmx or outer-kvm; no fallback exists' ;;
esac
output="$repo_root/bin/x86_64/linux-selftest-$test_name-$backend.efi"
serial_log="$repo_root/bin/x86_64/serial.log"
cd -- "$repo_root"
cargo xbuild x86 --release
env LINUX_L1_INIT="$repo_root/scripts/x86_64/linux-l1-selftest-init" \
    LINUX_L1_KVM_SELFTEST="$selftest" LINUX_L1_EXTRA_MODULES= \
    LINUX_L1_CMDLINE="console=ttyS0,115200n8 earlycon=uart8250,io,0x3f8,115200n8 rdinit=/init maxcpus=1 panic=0 thin_hv_selftest_backend=$backend thin_hv_selftest_name=$test_name" \
    scripts/x86_64/build-linux-uki.sh "$output"
env X86_UEFI_BACKEND="$backend" X86_UEFI_ACCEL=kvm X86_MONITOR_IMAGE="$monitor" \
    X86_UEFI_PHYSICAL_POLICY=0 X86_UEFI_HOST_EXCEPTION_TEST=0 \
    X86_UEFI_CPU='host,+vmx,-hypervisor,kvm=off' X86_UEFI_MEMORY=2G X86_UEFI_SMP=1 \
    X86_UEFI_GUEST_LOCATION=guest X86_UEFI_ALLOW_REBOOT=0 X86_UEFI_REQUIRE_POWEROFF=1 \
    X86_UEFI_ACPI_S3=0 X86_UEFI_WAKE_CYCLES=0 X86_UEFI_DATA_DISK= X86_UEFI_USERNET=0 \
    X86_UEFI_TIMEOUT_SECONDS="$timeout_seconds" \
    X86_RETURN_MARKER="thin-hv: linux KVM selftest PASS backend=$backend test=$test_name assertions=$assertions" \
    X86_VARIABLE_MARKER= X86_GUEST_MARKER='thin-hv: linux KVM selftest poweroff requested' \
    X86_GUEST_FAILURE_MARKER='thin-hv: linux KVM selftest FAIL' \
    scripts/x86_64/run-uefi-smoke.sh "$loader" "$output"
check_log "$backend" "$test_name" "$serial_log" || die 'selftest evidence rejected'
printf 'x86 Linux KVM selftest: PASS backend=%s role=%s test=%s environment=QEMU/kvm (not physical hardware)\n' "$backend" "$role" "$test_name"
