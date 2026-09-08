#!/usr/bin/env bash
# Pinned-upstream KVM userspace tests; their guests are L2, not L3.
set -euo pipefail

repo_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
die() { printf 'x86 Linux KVM selftest: %s\n' "$*" >&2; exit 1; }
valid_test() {
    case "$1" in
        tsc_msrs_test|userspace_msr_exit_test|cr4_cpuid_sync_test|xcr0_cpuid_test|debug_regs|apic_bus_clock_test|xapic_tpr_test|\
        cpuid_test|msrs_test|set_sregs_test|userspace_io_test|state_test|xapic_state_test|xapic_ipi_test|\
        recalc_apic_map_test|tsc_scaling_sync|kvm_clock_test|feature_msrs_test|xss_msr_test|fastops_test|\
        kvm_pv_test|platform_info_test|ucna_injection_test|exit_on_emulation_failure_test|smaller_maxphyaddr_emulation_test|\
        sync_regs_test|fix_hypercall_test|kvm_binary_stats_test|monitor_mwait_test|\
        hyperv_clock|hyperv_cpuid|hyperv_features|hyperv_ipi|hyperv_tlb_flush|hyperv_extended_hypercalls|\
        set_boot_cpu_id|max_vcpuid_cap_test|smm_test|amx_test|pmu_counters_test|pmu_event_filter_test|\
        dirty_log_test|guest_print_test|irqfd_test|set_memory_region_test|coalesced_io_test|\
        hardware_disable_test|guest_memfd_test|system_counter_offset_test|pre_fault_memory_test|\
        demand_paging_test|kvm_create_max_vcpus|kvm_page_table_test|memslot_modification_stress_test|\
        memslot_perf_test|access_tracking_perf_test|dirty_log_perf_test|mmu_stress_test|rseq_test|steal_time|\
        xen_vmcall_test|xen_shinfo_test|private_mem_kvm_exits_test|private_mem_conversions_test|\
        nx_huge_pages_test|dirty_log_page_splitting_test|vmx_exception_with_invalid_guest_state|\
        aperfmperf_test|kvm_buslock_test|hwcr_msr_test) ;;
        *) return 1 ;;
    esac
}

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
            count = 1
            l1_cpus = test_name == "rseq_test" && backend == "outer-kvm" ? 2 : 1
            split("map|unmap|unmap chunked|move active area|move inactive area|RW", slot_names, "|")
            names[1] = "msr_filter_allow"; names[2] = "msr_filter_deny"
            names[3] = "msr_permission_bitmap"; names[4] = "user_exit_msr_flags"
            prefix="user_msr."
            if (test_name == "tsc_msrs_test") { tap=1; count=5 }
            if (test_name == "userspace_msr_exit_test") { tap=1; harness=1; count=4 }
            if (test_name == "sync_regs_test") {
                tap=1; harness=1; count=10; prefix="sync_regs_test."
                split("read_invalid set_invalid req_and_verify_all_valid set_and_verify_various clear_kvm_dirty_regs_bits clear_kvm_valid_and_dirty_regs clear_kvm_valid_regs_bits race_cr4 race_exc race_inj_pen", names, " ")
            }
            if (test_name == "fix_hypercall_test") {
                tap=1; harness=1; count=2; prefix="fix_hypercall."
                split("enable_quirk disable_quirk", names, " ")
            }
            if (test_name == "kvm_binary_stats_test") { tap=1; count=4 }
            if (test_name == "steal_time") {
                tap=1; count=4; prefix=""
                for (i=1; i<=count; i++) names[i]="vcpu" (i-1)
            }
            if (test_name == "monitor_mwait_test") {
                tap=1; count=12; prefix=""
                for (tc=0; tc<16; tc++) {
                    misc=int(tc/2)%2; disabled=int(tc/4)%2; cpuid=int(tc/8)%2
                    if (!misc && cpuid != disabled) continue
                    names[++n]=(tc%2 ? "MWAIT can fault" : "MWAIT never faults") ", " \
                        (misc ? "MISC_ENABLE updates CPUID" : "no CPUID updates") ", CPUID " \
                        (cpuid ? "clear" : "set") (disabled ? ", MWAIT disabled" : "")
                }
            }
        }
        {
            sub(/\r$/, "")
            if ($0 ~ /Kernel panic|Oops:|BUG:|thin-hv: linux KVM selftest FAIL|Test Assertion Failure|^not ok |^Bail out!|^#.*FAIL|^#.*ERROR|# SKIP|# XFAIL|# XPASS/) bad=1
            if (index($0, "thin-hv: linux KVM selftest ") == 1) {
                if ($0 == "thin-hv: linux KVM selftest begin backend=" backend " test=" test_name " l1_cpus=" l1_cpus) {
                    if (begin || ended || done || poweroff) bad=1
                    begin++
                } else if ($0 == "thin-hv: linux KVM selftest exit backend=" backend " test=" test_name " process_exit=0") {
                    if (begin != 1 || (tap && (header != 1 || plan != 1 || passed != count || totals != 1)) ||
                        (test_name == "memslot_perf_test" && (slot_starts != 6 || slot_done != 6)) ||
                        (harness && suite != 1) || ended || done || poweroff) bad=1
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
            if (test_name == "memslot_perf_test") {
                if ($0 ~ /Memslot count|No iterations/) bad=1
                if ($0 ~ /^Testing /) {
                    if (slot_starts != slot_done || begin != 1 || ended) bad=1
                    slot_starts++
                    if ($0 != "Testing " slot_names[slot_starts] " performance with 1 runs, 5 seconds each") bad=1
                }
                if ($0 ~ /^Done /) {
                    if ($0 !~ /^Done [1-9][0-9]* iterations, avg [0-9]+\.[0-9]+s each$/ ||
                        slot_starts != slot_done+1 || ended) bad=1
                    slot_done++
                }
            }
            if ($0 ~ /^TAP version |^1\.\.|^ok |^# Totals:|^# PASSED:/) {
                if (!tap) bad=1
                if (begin != 1 || ended || done || poweroff) bad=1
                if ($0 == "TAP version 13") { if (header || plan || passed || totals) bad=1; header++ }
                else if ($0 == "1.." count) { if (header != 1 || plan || passed || totals) bad=1; plan++ }
                else if ($0 ~ /^ok /) {
                    passed++
                    expected = "ok " passed " " prefix names[passed]
                    if (test_name == "tsc_msrs_test") expected = "ok " passed " stage " (passed + 1) " passed"
                    if (test_name == "kvm_binary_stats_test") expected = "ok " passed " vm" (passed - 1)
                    if (plan != 1 || totals || passed > count || $0 != expected) bad=1
                } else if ($0 == "# PASSED: " count " / " count " tests passed." && harness) {
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
default_timeout=300
case "$test_name" in
    hardware_disable_test|kvm_create_max_vcpus|mmu_stress_test|memslot_perf_test|access_tracking_perf_test) default_timeout=900 ;;
esac
timeout_seconds=${LINUX_SELFTEST_TIMEOUT_SECONDS:-$default_timeout}
valid_test "$test_name" || die 'unsupported pinned KVM selftest name'
# The non-TAP upstream programs return 0 only after UCALL_DONE and all
# assertions. For those, assertions=1 counts the completed program, not its
# individual guest assertions; exit 4 (KSFT_SKIP) is always a failure here.
case "$test_name" in
    tsc_msrs_test) assertions=5 ;;
    userspace_msr_exit_test|kvm_binary_stats_test|steal_time) assertions=4 ;;
    sync_regs_test) assertions=10 ;;
    fix_hypercall_test) assertions=2 ;;
    monitor_mwait_test) assertions=12 ;;
    *) assertions=1 ;;
esac
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
cpu='host,+vmx,-hypervisor,kvm=off'
memory=2G
pci_profile=firmware-default
l1_cpus=1
if [[ "$test_name" == rseq_test && "$backend" == outer-kvm ]]; then
    # CPU migration requires two L1 CPUs. Direct L0 does not own APs yet;
    # never infer Direct SMP success from this reference-only topology.
    l1_cpus=2
fi
case "$test_name" in
    kvm_create_max_vcpus|mmu_stress_test)
        # Bound these memory-heavy tests inside the explicit QEMU fixture.
        memory=4G; pci_profile=q35-smoke-1g ;;
esac
if [[ "$test_name" == monitor_mwait_test ]]; then
    # QEMU masks MONITOR by default even on supporting Intel hardware. This
    # named test profile requests it explicitly; unsupported KVM still fails.
    cpu='host,+vmx,+monitor,-hypervisor,kvm=off'
fi
printf 'x86 Linux KVM selftest: test=%s cpu=%s\n' "$test_name" "$cpu"
extra_modules=
[[ "$test_name" != aperfmperf_test ]] || extra_modules=msr
cd -- "$repo_root"
cargo xbuild x86 --release
env LINUX_L1_INIT="$repo_root/scripts/x86_64/linux-l1-selftest-init" \
    LINUX_L1_KVM_SELFTEST="$selftest" LINUX_L1_EXTRA_MODULES="$extra_modules" \
    LINUX_L1_CMDLINE="console=ttyS0,115200n8 earlycon=uart8250,io,0x3f8,115200n8 rdinit=/init maxcpus=$l1_cpus panic=0 thin_hv_selftest_backend=$backend thin_hv_selftest_name=$test_name" \
    scripts/x86_64/build-linux-uki.sh "$output"
env X86_UEFI_BACKEND="$backend" X86_UEFI_ACCEL=kvm X86_MONITOR_IMAGE="$monitor" \
    X86_UEFI_PHYSICAL_POLICY=0 X86_UEFI_HOST_EXCEPTION_TEST=0 \
    X86_UEFI_CPU="$cpu" X86_UEFI_MEMORY="$memory" X86_UEFI_SMP="$l1_cpus" \
    X86_UEFI_PCI_PROFILE="$pci_profile" \
    X86_UEFI_GUEST_LOCATION=guest X86_UEFI_ALLOW_REBOOT=0 X86_UEFI_REQUIRE_POWEROFF=1 \
    X86_UEFI_ACPI_S3=0 X86_UEFI_WAKE_CYCLES=0 X86_UEFI_DATA_DISK= X86_UEFI_USERNET=0 \
    X86_UEFI_TIMEOUT_SECONDS="$timeout_seconds" \
    X86_RETURN_MARKER="thin-hv: linux KVM selftest PASS backend=$backend test=$test_name assertions=$assertions" \
    X86_VARIABLE_MARKER= X86_GUEST_MARKER='thin-hv: linux KVM selftest poweroff requested' \
    X86_GUEST_FAILURE_MARKER='thin-hv: linux KVM selftest FAIL' \
    scripts/x86_64/run-uefi-smoke.sh "$loader" "$output"
check_log "$backend" "$test_name" "$serial_log" || die 'selftest evidence rejected'
printf 'x86 Linux KVM selftest: PASS backend=%s role=%s test=%s environment=QEMU/kvm (not physical hardware)\n' "$backend" "$role" "$test_name"
