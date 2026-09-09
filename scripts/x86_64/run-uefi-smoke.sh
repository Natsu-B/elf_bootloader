#!/usr/bin/env bash
set -euo pipefail

# Shared by transcript validation and the live runner; a halted monitor cannot
# recover by waiting for an unrelated guest success marker.
direct_failure_pattern='thin-hv: (vmx smoke FAIL|vmx guest FAIL|VMRESUME FAIL|VMXOFF status=|host exception |panic|CPUID VMX=0|IA32_FEATURE_CONTROL=unavailable|IA32_VMX_BASIC=unavailable)'

die() {
    printf 'x86 UEFI smoke: %s\n' "$*" >&2
    exit 1
}

# This same gate is exercised without QEMU by the xtask host tests.
check_backend_log() {
    local backend=$1 log=$2 expected line seen=0
    case "$backend" in
        direct-vmx) expected='thin-hv: backend=direct-vmx role=project-l0' ;;
        outer-kvm) expected='thin-hv: backend=outer-kvm role=reference' ;;
        physical-chainload) expected='thin-hv: backend=physical-chainload project_vmx=0 resident_runtime=0' ;;
        physical-preflight) expected='thin-hv: backend=physical-preflight project_vmx=0' ;;
        *) return 1 ;;
    esac
    while IFS= read -r line || [[ -n "$line" ]]; do
        line=${line%$'\r'}
        if [[ "$line" == 'thin-hv: backend='* ]]; then
            [[ "$line" == "$expected" ]] || return 1
            seen=1
        fi
        if [[ "$backend" == direct-vmx ]]; then
            # Runtime entry is logged before VMX setup. Firmware may continue
            # booting Windows after setup returns an error; that is not L0 PASS.
            # VMXOFF status is emitted only by the terminal failure handlers.
            [[ ! "$line" =~ $direct_failure_pattern ]] || return 1
        fi
        if [[ "$backend" != direct-vmx ]]; then
            case "$line" in
                *'thin-hv: loading runtime monitor'* | *'thin-hv: runtime monitor active'* | \
                *'thin-hv: private host state'* | *'thin-hv: host exception '* | \
                *'thin-hv: variable overlay profile='* | *'thin-hv: uefi variable overlay PASS'* | \
                *'thin-hv: L1 '* | *'thin-hv: vmx '*) return 1 ;;
            esac
        fi
        if [[ "$backend" != outer-kvm && "$line" == *'thin-hv: trusted outer KVM'* ]]; then
            return 1
        fi
        if [[ "$backend" == physical-preflight && "$line" == *'thin-hv: guest uefi payload'* ]]; then
            return 1
        fi
    done <"$log"
    ((seen))
}

# Hardware-backed instruction assertions must precede a successful guest return.
# Optional capability checks remain explicit in the evidence, never implied PASS.
check_nested_contract_log() {
    local backend=$1 cpu_profile=$2 log=$3 line transcript bytes phase=0 backends=0 private=0 clobber=0 expected_backends
    local valid invept invvpid readonly shadow ept_types vpid_types success descriptors pku ospke_toggles bit expected_success expected_descriptors LC_ALL=C
    local pass_pattern='^thin-hv: nested contract PASS vmcs=2 cycles=8 vmfail_invalid=9 vmfail_valid=(1[3-9]|2[0-6]) invept=([01]) invvpid=([01]) readonly=([01]) wide_fields=2 misaligned=2 revision=3 entry_failures=3 no_current=7 shadow=([01]) invept_types=([0-3]) invvpid_types=([0-9]|1[0-5]) invalidation_success=([0-6]) descriptor_failures=([0-9]) osxsave_toggles=4 xsetbv_valid=4 xsetbv_gp=4 xsetbv_ud=1 pku=([01]) ospke_toggles=([04]) operand_pf=16 operand_gp=8 operand_ss=1 operand_cross=6 operand_priority=8 host_invalid=34 host_priority=2 host_restore=1 msr_invalid=12 msr_priority=12 msr_ignored=3 guest_msr_shadow=2 fx_cpuid=6 fx_xsetbv=12 fx_entry=68 fx_irq=3 ymm_rounds=[04]$'
    case "$backend" in direct-vmx) expected_backends=2 ;; outer-kvm) expected_backends=1 ;; *) return 1 ;; esac
    case "$cpu_profile" in native|readonly-vmcs) ;; host-xstate) [[ "$backend" == direct-vmx ]] || return 1 ;; *) return 1 ;; esac
    [[ -f "$log" && -r "$log" ]] || return 1
    bytes=$(wc -c <"$log") || return 1
    [[ "$bytes" =~ ^[0-9]+$ ]] && ((bytes > 0 && bytes <= 262144)) || return 1
    if IFS= read -r -d '' -n 262145 transcript <"$log"; then
        return 1
    fi
    ((${#transcript} <= 262144)) || return 1
    check_backend_log "$backend" "$log" || return 1
    while IFS= read -r line || [[ -n "$line" ]]; do
        line=${line%$'\r'}
        if [[ "$line" =~ $pass_pattern ]]; then
            ((phase == 1)) || return 1
            valid=${BASH_REMATCH[1]}
            invept=${BASH_REMATCH[2]}
            invvpid=${BASH_REMATCH[3]}
            readonly=${BASH_REMATCH[4]}
            shadow=${BASH_REMATCH[5]}
            ept_types=${BASH_REMATCH[6]}
            vpid_types=${BASH_REMATCH[7]}
            success=${BASH_REMATCH[8]}
            descriptors=${BASH_REMATCH[9]}
            pku=${BASH_REMATCH[10]}
            ospke_toggles=${BASH_REMATCH[11]}
            ((ospke_toggles == pku * 4)) || return 1
            # The memory-fault contract executes both global invalidations;
            # a transcript lacking these prerequisites cannot claim that count.
            (((ept_types & 2) != 0 && (vpid_types & 4) != 0)) || return 1
            ((invept == (ept_types != 0) && invvpid == (vpid_types != 0))) || return 1
            expected_success=0
            expected_descriptors=$(((ept_types & 1) + (vpid_types & 1)))
            for bit in 0 1 2 3; do
                expected_success=$((expected_success + ((vpid_types >> bit) & 1)))
                expected_descriptors=$((expected_descriptors + ((vpid_types >> bit) & 1) + (((vpid_types & 11) >> bit) & 1)))
            done
            expected_success=$((expected_success + (ept_types & 1) + ((ept_types >> 1) & 1)))
            ((success == expected_success && descriptors == expected_descriptors &&
              valid == 14 - shadow + invept + invvpid + readonly + descriptors)) || return 1
            [[ "$cpu_profile" != readonly-vmcs || "$readonly" == 1 ]] || return 1
            phase=2
            continue
        fi
        case "$line" in
            'thin-hv: host xstate clobber fixture armed')
                [[ "$cpu_profile" == host-xstate ]] && ((phase == 0 && clobber == 0)) || return 1
                clobber=1
                ;;
            'thin-hv: backend='*)
                ((phase == 0 && backends < expected_backends)) || return 1
                backends=$((backends + 1))
                ;;
            'thin-hv: private host state PASS')
                [[ "$backend" == direct-vmx ]] && ((phase == 0 && private == 0)) || return 1
                private=1
                ;;
            'thin-hv: nested contract START')
                ((phase == 0 && backends == expected_backends)) || return 1
                [[ "$backend" != direct-vmx || "$private" == 1 ]] || return 1
                [[ "$cpu_profile" != host-xstate || "$clobber" == 1 ]] || return 1
                phase=1
                ;;
            'thin-hv: vmx guest PASS start_image_status=0x0000000000000000')
                [[ "$backend" == direct-vmx ]] && ((phase == 2)) || return 1
                phase=3
                ;;
            'thin-hv: trusted outer KVM guest PASS')
                [[ "$backend" == outer-kvm ]] && ((phase == 2)) || return 1
                phase=3
                ;;
            *'FAIL'* | *'panic'* | 'thin-hv: nested contract '* | \
            'thin-hv: private host state'* | 'thin-hv: vmx guest PASS'* | \
            'thin-hv: trusted outer KVM guest PASS'*) return 1 ;;
        esac
    done <<<"$transcript"
    ((phase == 3 && backends == expected_backends))
}

# Real L2 entries with every PAT/EFER control combination. This image deliberately
# powers off instead of returning into disposable firmware descriptor state.
check_msr_contract_log() {
    local backend=$1 log=$2 line transcript bytes phase=0 cases=0 entries=0 checked=0 backends=0 private=0 expected_backends
    case "$backend" in direct-vmx) expected_backends=2 ;; outer-kvm) expected_backends=1 ;; *) return 1 ;; esac
    [[ -f "$log" && -r "$log" ]] || return 1
    bytes=$(wc -c <"$log") || return 1
    [[ "$bytes" =~ ^[0-9]+$ ]] && ((bytes > 0 && bytes <= 262144)) || return 1
    if IFS= read -r -d '' -n 262145 transcript <"$log"; then return 1; fi
    check_backend_log "$backend" "$log" || return 1
    while IFS= read -r line || [[ -n "$line" ]]; do
        line=${line%$'\r'}
        case "$line" in
            'thin-hv: backend='*)
                ((phase == 0 && backends < expected_backends)) || return 1
                backends=$((backends + 1)) ;;
            'thin-hv: private host state PASS')
                [[ "$backend" == direct-vmx ]] && ((phase == 0 && private == 0)) || return 1
                private=1 ;;
            'thin-hv: MSR contract START')
                ((phase == 0 && backends == expected_backends)) || return 1
                [[ "$backend" != direct-vmx || "$private" == 1 ]] || return 1
                phase=1 ;;
            'thin-hv: MSR matrix case='*)
                ((phase == 1 && cases < 128 && entries == 0)) && [[ "$line" == "thin-hv: MSR matrix case=$cases" ]] || return 1
                cases=$((cases + 1)) ;;
            'thin-hv: MSR entry case='*)
                ((phase == 1 && cases == 128 && entries < 20)) && [[ "$line" == "thin-hv: MSR entry case=$entries" ]] || return 1
                entries=$((entries + 1)) ;;
            'thin-hv: MSR late-failure guest-field changes=0')
                ((phase == 1 && cases == 128 && entries == 20 && checked == 0)) || return 1
                checked=1 ;;
            'thin-hv: MSR contract PASS matrix=128 entry_cases=20 entry_load=7 entry_resume=1 entry_fail=10 early_fail=2 guest_fail=2 vmxoff=1')
                ((phase == 1 && cases == 128 && entries == 20 && checked == 1)) || return 1
                phase=2 ;;
            *'FAIL'* | *'panic'* | 'thin-hv: MSR '* | 'thin-hv: private host state'* | \
            'thin-hv: vmx guest PASS'* | 'thin-hv: trusted outer KVM guest PASS'*) return 1 ;;
        esac
    done <<<"$transcript"
    ((phase == 2 && cases == 128 && entries == 20 && backends == expected_backends))
}

# This is a separate negative fixture, never a relaxed ordinary backend gate.
# Only the deliberate root exception may fail, and QEMU must stop by timeout.
check_host_exception_log() {
    local status=$1 log=$2 line transcript bytes phase=0 backends=0 payload=0 variables=0
    [[ "$status" == 124 ]] || return 1
    [[ -f "$log" ]] || return 1
    bytes=$(wc -c <"$log") || return 1
    [[ "$bytes" =~ ^[0-9]+$ ]] || return 1
    ((bytes > 0 && bytes <= 262144)) || return 1
    # Bash normally discards NUL bytes in read. Using NUL as the delimiter first
    # rejects them instead of accepting a corrupted marker with bytes removed.
    if IFS= read -r -d '' transcript <"$log"; then
        return 1
    fi
    ((${#transcript} <= 262144)) || return 1
    while IFS= read -r line || [[ -n "$line" ]]; do
        line=${line%$'\r'}
        case "$line" in
            'thin-hv: backend=direct-vmx role=project-l0')
                ((phase == 0 && backends < 2)) || return 1
                backends=$((backends + 1))
                ;;
            'thin-hv: runtime monitor active')
                ((phase == 0 && backends == 2)) || return 1
                phase=1
                ;;
            'thin-hv: private host state PASS')
                ((phase == 1)) || return 1
                phase=2
                ;;
            'thin-hv: guest uefi payload')
                ((phase == 2 && payload == 0 && variables == 0)) || return 1
                payload=1
                ;;
            'thin-hv: uefi variable overlay PASS')
                ((phase == 2 && payload == 1 && variables == 0)) || return 1
                variables=1
                ;;
            'thin-hv: host exception test guest returned')
                ((phase == 2 && payload == 1 && variables == 1)) || return 1
                phase=3
                ;;
            'thin-hv: host exception test armed')
                ((phase == 3)) || return 1
                phase=4
                ;;
            'thin-hv: host exception FAIL: stopped')
                ((phase == 4)) || return 1
                phase=5
                ;;
            *'FAIL'* | *'panic'* | *'thin-hv: VMXOFF status='* | \
            *'thin-hv: CPUID VMX=0'* | *'thin-hv: trusted outer KVM'* | \
            'thin-hv: backend='* | 'thin-hv: runtime monitor active'* | \
            'thin-hv: private host state'* | 'thin-hv: host exception '* | \
            'thin-hv: vmx guest PASS'* | 'thin-hv: guest uefi payload'* | \
            'thin-hv: uefi variable overlay'* | 'thin-hv: uefi native variables'*) return 1 ;;
        esac
    done <<<"$transcript"
    ((phase == 5 && backends == 2 && payload == 1 && variables == 1))
}

# Fixed QEMU fixtures distinguish a real EPT construction from a capability skip.
# The application may legitimately skip elsewhere; that is not this KVM test's PASS.
check_preflight_ept_log() {
    local accel=$1 log=$2 line transcript bytes passes=0 skips=0 vmx_markers=0 tables leaves
    local pass_pattern='^thin-hv: preflight EPT audit PASS scope=uefi-memory-map tables=([1-9][0-9]{0,2}) leaves=([1-9][0-9]{0,19}) private_pages=288 mmio_complete=0 direct_vmx_ready=0$'
    [[ "$accel" == kvm || "$accel" == tcg ]] || return 1
    [[ -f "$log" ]] || return 1
    bytes=$(wc -c <"$log") || return 1
    [[ "$bytes" =~ ^[0-9]+$ ]] || return 1
    ((bytes > 0 && bytes <= 262144)) || return 1
    if IFS= read -r -d '' transcript <"$log"; then
        return 1
    fi
    ((${#transcript} <= 262144)) || return 1
    while IFS= read -r line || [[ -n "$line" ]]; do
        line=${line%$'\r'}
        if [[ "$line" =~ $pass_pattern ]]; then
            [[ "$accel" == kvm ]] || return 1
            ((passes == 0 && skips == 0)) || return 1
            tables=${BASH_REMATCH[1]}
            leaves=${BASH_REMATCH[2]}
            ((tables <= 256)) || return 1
            # Validate u64 without Bash signed arithmetic wrapping large counts.
            if ((${#leaves} == 20)) && [[ "$leaves" > 18446744073709551615 ]]; then
                return 1
            fi
            passes=1
            continue
        fi
        case "$line" in
            'thin-hv: preflight EPT audit SKIP reason=no-ept-capability direct_vmx_ready=0')
                [[ "$accel" == tcg ]] || return 1
                ((skips == 0 && passes == 0)) || return 1
                skips=1
                ;;
            'thin-hv: preflight VMX=0 direct_vmx_ready=0')
                [[ "$accel" == tcg ]] || return 1
                ((vmx_markers == 0)) || return 1
                vmx_markers=1
                ;;
            'thin-hv: preflight VMX=1 direct_vmx_ready=0')
                [[ "$accel" == kvm ]] || return 1
                ((vmx_markers == 0)) || return 1
                vmx_markers=1
                ;;
            *'FAIL'* | *'panic'* | 'thin-hv: preflight EPT audit'* | \
            'thin-hv: preflight VMX='*) return 1 ;;
        esac
    done <<<"$transcript"
    if [[ "$accel" == kvm ]]; then
        ((passes == 1 && skips == 0))
    else
        ((passes == 0 && skips == 1 && vmx_markers == 1))
    fi
}

# Validate the complete ordered transcript, not just the final fixture marker.
# Expected loader errors are accepted only within their own negative test case.
check_physical_policy_log() {
    local log=$1 line index=0 active=0 complete=0 secondary=0
    local entries=0 backends=0 payloads=0 returns=0 failures=0
    local names=(default-windows explicit-linux other-esp-only malformed-options self-path)
    local statuses=(0x0 0x0 0x800000000000000e 0x8000000000000002 0x800000000000000f)
    local paths=(windows linux)
    check_backend_log physical-chainload "$log" || return 1
    while IFS= read -r line || [[ -n "$line" ]]; do
        line=${line%$'\r'}
        case "$line" in
            'thin-hv: physical policy secondary_esp_visible=1 targets=windows,linux,other-only PASS')
                ((secondary == 0 && active == 0 && index == 0 && complete == 0)) || return 1
                secondary=1
                ;;
            'thin-hv: physical policy case='*' begin')
                ((secondary == 1 && active == 0 && complete == 0 && index < ${#names[@]})) || return 1
                [[ "$line" == "thin-hv: physical policy case=${names[index]} begin" ]] || return 1
                active=1 entries=0 backends=0 payloads=0 returns=0 failures=0
                ;;
            'thin-hv: uefi entry')
                ((active == 1 && entries == 0 && backends == 0)) || return 1
                entries=1
                ;;
            'thin-hv: backend='*)
                ((active == 1 && entries == 1 && backends == 0)) || return 1
                backends=1
                ;;
            'thin-hv: physical policy payload '*)
                ((active == 1 && backends == 1 && index < 2 && payloads == 0 && returns == 0)) || return 1
                [[ "$line" == "thin-hv: physical policy payload path=${paths[index]} current_esp=1 PASS" ]] || return 1
                payloads=1
                ;;
            'thin-hv: physical chainload PASS')
                ((active == 1 && index < 2 && payloads == 1 && returns == 0)) || return 1
                returns=1
                ;;
            'thin-hv: physical chainload FAIL: '*)
                ((active == 1 && backends == 1 && index >= 2 && failures == 0)) || return 1
                [[ "$line" == *" status=${statuses[index]}" ]] || return 1
                failures=1
                ;;
            'thin-hv: physical policy case='*' PASS status='*)
                ((active == 1 && entries == 1 && backends == 1)) || return 1
                [[ "$line" == "thin-hv: physical policy case=${names[index]} PASS status=${statuses[index]}" ]] || return 1
                if ((index < 2)); then
                    ((payloads == 1 && returns == 1 && failures == 0)) || return 1
                else
                    ((payloads == 0 && returns == 0 && failures == 1)) || return 1
                fi
                active=0
                index=$((index + 1))
                ;;
            'thin-hv: physical policy harness PASS')
                ((active == 0 && complete == 0 && index == ${#names[@]})) || return 1
                complete=1
                ;;
            *'thin-hv:'*'FAIL'* | *'thin-hv:'*'panic'* | \
            *'thin-hv: guest uefi payload'* | *'thin-hv: uefi native variables'* | \
            'thin-hv: physical policy '*) return 1 ;;
            'thin-hv: physical chainload scope='*)
                ((active == 1 && backends == 1 && index < 3 && payloads == 0 && returns == 0 && failures == 0)) || return 1
                if ((index == 0)); then
                    [[ "$line" == 'thin-hv: physical chainload scope=current-esp explicit_path=0' ]] || return 1
                else
                    [[ "$line" == 'thin-hv: physical chainload scope=current-esp explicit_path=1' ]] || return 1
                fi
                ;;
            'thin-hv: physical chainload '*) return 1 ;;
        esac
    done <"$log"
    ((secondary == 1 && complete == 1 && active == 0 && index == ${#names[@]}))
}

if [[ ${1:-} == --check-backend-log ]]; then
    [[ $# == 3 ]] || die 'usage: --check-backend-log BACKEND LOG'
    check_backend_log "$2" "$3" || die "backend provenance check failed for $2"
    exit 0
fi
if [[ ${1:-} == --check-nested-contract-log ]]; then
    [[ $# == 4 ]] || die 'usage: --check-nested-contract-log BACKEND CPU_PROFILE LOG'
    check_nested_contract_log "$2" "$3" "$4" || die 'nested VMX contract transcript check failed'
    exit 0
fi
if [[ ${1:-} == --check-msr-contract-log ]]; then
    [[ $# == 3 ]] || die 'usage: --check-msr-contract-log BACKEND LOG'
    check_msr_contract_log "$2" "$3" || die 'nested MSR contract transcript check failed'
    exit 0
fi
if [[ ${1:-} == --check-physical-policy-log ]]; then
    [[ $# == 2 ]] || die 'usage: --check-physical-policy-log LOG'
    check_physical_policy_log "$2" || die 'physical-chainload policy transcript check failed'
    exit 0
fi
if [[ ${1:-} == --check-host-exception-log ]]; then
    [[ $# == 3 ]] || die 'usage: --check-host-exception-log QEMU_STATUS LOG'
    check_host_exception_log "$2" "$3" || die 'root host-exception fixture transcript/status check failed'
    exit 0
fi
if [[ ${1:-} == --check-preflight-ept-log ]]; then
    [[ $# == 3 ]] || die 'usage: --check-preflight-ept-log ACCEL LOG'
    check_preflight_ept_log "$2" "$3" || die 'preflight EPT construction transcript check failed'
    exit 0
fi

repo_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
loader=${1:-"$repo_root/bin/x86_64/x86-uefi-loader.efi"}
guest=${2:-"$repo_root/bin/x86_64/x86_guest_uefi_test.efi"}
backend=${X86_UEFI_BACKEND:-direct-vmx}
pci_profile=${X86_UEFI_PCI_PROFILE:-firmware-default}
pci_args=()
case "$pci_profile" in
    firmware-default) ;;
    q35-smoke-1g)
        # Same bounded QEMU-only aperture as the existing Windows runner.
        # Explicit A/B fixture, never a platform-derived EPT or a fallback.
        pci_args=(-global q35-pcihost.pci-hole64-size=1G
            -fw_cfg name=opt/ovmf/X-PciMmio64Mb,string=1024) ;;
    *) die 'X86_UEFI_PCI_PROFILE must be firmware-default or q35-smoke-1g' ;;
esac
printf 'x86 UEFI smoke: PCI profile=%s environment=QEMU (not physical hardware)\n' "$pci_profile"
physical_policy=${X86_UEFI_PHYSICAL_POLICY:-0}
host_exception_test=${X86_UEFI_HOST_EXCEPTION_TEST:-0}
[[ "$physical_policy" =~ ^[01]$ ]] || die 'X86_UEFI_PHYSICAL_POLICY must be 0 or 1'
[[ "$host_exception_test" =~ ^[01]$ ]] || die 'X86_UEFI_HOST_EXCEPTION_TEST must be 0 or 1'
if ((host_exception_test)); then
    [[ "$backend" == direct-vmx && "$physical_policy" == 0 ]] || \
        die 'host-exception fixtures require the explicit direct-vmx backend and no physical policy fixture'
fi
if ((physical_policy)); then
    [[ "$backend" == physical-chainload ]] || die 'physical policy fixtures require the explicit physical-chainload backend'
fi
monitor=${X86_MONITOR_IMAGE-}
if [[ "$backend" == direct-vmx ]]; then
    monitor=${X86_MONITOR_IMAGE-"$(dirname -- "$loader")/x86-uefi-monitor.efi"}
fi
stage="$repo_root/bin/x86_64"
serial_log="$stage/serial.log"
qemu_log="$stage/qemu.log"
monitor_fifo="$stage/qemu-monitor.$$.in"
vars="$stage/OVMF_VARS.fd"
marker='thin-hv: uefi entry'
return_marker=${X86_RETURN_MARKER-'thin-hv: vmx guest PASS'}
payload_marker=${X86_GUEST_MARKER-'thin-hv: guest uefi payload'}
failure_marker=${X86_GUEST_FAILURE_MARKER-}
variable_marker=${X86_VARIABLE_MARKER-}
guest_location=${X86_UEFI_GUEST_LOCATION:-guest}
trusted_chainload_marker=
case "$backend" in
    direct-vmx) ;;
    outer-kvm) return_marker=${X86_RETURN_MARKER-'thin-hv: trusted outer KVM guest PASS'} ;;
    physical-chainload)
        guest_location=${X86_UEFI_GUEST_LOCATION:-windows}
        return_marker='thin-hv: physical chainload PASS'
        ;;
    physical-preflight)
        guest_location=none
        return_marker='thin-hv: physical preflight PASS'
        payload_marker=
        variable_marker=
        ;;
    *) die 'X86_UEFI_BACKEND must be direct-vmx, outer-kvm, physical-chainload, or physical-preflight' ;;
esac
if [[ "$backend" != direct-vmx && -n "$monitor" ]]; then
    die "$backend must not stage a project runtime monitor"
fi
if [[ ! ${X86_VARIABLE_MARKER+x} && ${guest##*/} == x86_guest_uefi_test.efi ]]; then
    if [[ "$backend" == outer-kvm || "$backend" == physical-chainload ]]; then
        variable_marker='thin-hv: uefi native variables PASS'
    elif [[ "$backend" == direct-vmx ]]; then
        variable_marker='thin-hv: uefi variable overlay PASS'
    fi
fi
if ((physical_policy)); then
    marker='thin-hv: physical policy case=default-windows begin'
    return_marker='thin-hv: physical policy harness PASS'
    payload_marker=
    variable_marker=
    failure_marker='thin-hv: physical policy harness FAIL'
fi
timeout_seconds=${X86_UEFI_TIMEOUT_SECONDS:-10}
memory=${X86_UEFI_MEMORY:-256M}
smp=${X86_UEFI_SMP:-1}
cpu=${X86_UEFI_CPU:-host,+vmx,-hypervisor}
accel=${X86_UEFI_ACCEL:-kvm}
acpi_s3=${X86_UEFI_ACPI_S3:-0}
wake_cycles=${X86_UEFI_WAKE_CYCLES:-0}
allow_reboot=${X86_UEFI_ALLOW_REBOOT:-0}
require_poweroff=${X86_UEFI_REQUIRE_POWEROFF:-0}
data_disk=${X86_UEFI_DATA_DISK:-}
usernet=${X86_UEFI_USERNET:-0}

case "$guest_location" in
    guest | both) trusted_profile=2 ;;
    windows) trusted_profile=1 ;;
    none) [[ "$backend" == physical-preflight ]] || die 'only preflight may omit the guest' ;;
    *) die "X86_UEFI_GUEST_LOCATION must be guest, windows, or both" ;;
esac
if [[ "$backend" == physical-chainload && "$guest_location" != windows ]]; then
    die 'physical-chainload smoke requires the Windows test payload on its parent ESP'
fi
if [[ "$backend" == outer-kvm ]]; then
    trusted_chainload_marker="thin-hv: trusted outer KVM direct chainload profile=$trusted_profile resident_runtime=0"
fi

first_file() {
    local candidate
    for candidate in "$@"; do
        if [[ -n "$candidate" && -f "$candidate" ]]; then
            printf '%s\n' "$candidate"
            return 0
        fi
    done
    return 1
}

[[ -f "$loader" ]] || die "loader not found: $loader"
[[ -z "$monitor" || -f "$monitor" ]] || die "runtime monitor not found: $monitor"
[[ "$guest_location" == none || -f "$guest" ]] || die "guest payload not found: $guest"
policy_loader="$stage/x86-uefi-physical-loader.efi"
if ((physical_policy)); then
    [[ -f "$policy_loader" ]] || die "physical policy project loader not found: $policy_loader"
fi
[[ "$timeout_seconds" =~ ^[1-9][0-9]*$ ]] || die 'X86_UEFI_TIMEOUT_SECONDS must be a positive integer'
[[ "$memory" =~ ^[1-9][0-9]*[KMG]$ ]] || die 'X86_UEFI_MEMORY must be a positive QEMU size such as 256M'
[[ "$smp" =~ ^[1-9][0-9]*$ ]] || die 'X86_UEFI_SMP must be a positive integer'
[[ -n "$cpu" ]] || die 'X86_UEFI_CPU must not be empty'
[[ "$accel" == kvm || "$accel" == tcg ]] || die 'X86_UEFI_ACCEL must be kvm or tcg'
[[ "$acpi_s3" =~ ^[01]$ ]] || die 'X86_UEFI_ACPI_S3 must be 0 or 1'
[[ "$wake_cycles" =~ ^[0-9]+$ ]] || die 'X86_UEFI_WAKE_CYCLES must be a non-negative integer'
[[ "$allow_reboot" =~ ^[01]$ ]] || die 'X86_UEFI_ALLOW_REBOOT must be 0 or 1'
[[ "$require_poweroff" =~ ^[01]$ ]] || die 'X86_UEFI_REQUIRE_POWEROFF must be 0 or 1'
[[ "$usernet" =~ ^[01]$ ]] || die 'X86_UEFI_USERNET must be 0 or 1'
[[ -z "$data_disk" || -f "$data_disk" ]] || die "data disk not found: $data_disk"
((wake_cycles == 0 || acpi_s3 == 1)) || die 'X86_UEFI_WAKE_CYCLES requires X86_UEFI_ACPI_S3=1'
if ((acpi_s3)); then
    # Direct S3 is a QEMU correctness test, not a supported hardware boot mode.
    # Its one visible CPU must remain distinct from reference CPU-hotplug coverage.
    case "$backend:$smp" in
        outer-kvm:2|direct-vmx:1) ;;
        *) die 'S3 tests require outer-kvm with two CPUs or direct-vmx with one CPU' ;;
    esac
fi
if ((physical_policy)); then
    [[ "$smp" == 1 && "$acpi_s3" == 0 && "$wake_cycles" == 0 && \
        "$allow_reboot" == 0 && "$require_poweroff" == 0 && \
        "$usernet" == 0 && -z "$data_disk" ]] || \
        die 'physical policy fixtures require one CPU, no extra devices, and no suspend/reboot/poweroff mode'
fi
if ((host_exception_test)); then
    [[ "$accel" == kvm && "$smp" == 1 && "$memory" == 256M && \
        "$acpi_s3" == 0 && "$wake_cycles" == 0 && "$allow_reboot" == 0 && \
        "$require_poweroff" == 0 && "$usernet" == 0 && -z "$data_disk" && \
        "$guest_location" == guest && -z "$failure_marker" ]] || \
        die 'host-exception fixtures require QEMU/KVM, one CPU, 256M, the native guest, and no extra modes/devices'
    [[ "$timeout_seconds" =~ ^([1-9]|[1-5][0-9]|60)$ ]] || \
        die 'host-exception timeout must be bounded to 1..60 seconds'
    [[ ${loader##*/} == x86-uefi-host-exception-loader.efi && \
        ${monitor##*/} == x86-uefi-host-exception-monitor.efi && \
        ${guest##*/} == x86_guest_uefi_test.efi ]] || \
        die 'host-exception fixtures require their separate test-only loader/runtime artifacts and native guest'
elif [[ ${loader##*/} == x86-uefi-host-exception-loader.efi || \
        ${monitor##*/} == x86-uefi-host-exception-monitor.efi ]]; then
    die 'test-only host-exception artifacts must not run as an ordinary smoke backend'
fi
command -v timeout >/dev/null || die "GNU timeout is required"

qemu=${QEMU_SYSTEM_X86_64:-qemu-system-x86_64}
qemu=$(command -v -- "$qemu") || die "qemu-system-x86_64 not found"
qemu_real=$(readlink -f -- "$qemu")
qemu_share=$(dirname -- "$qemu_real")/../share/qemu
ovmf_dir=${OVMF_FD_DIR:-}

ovmf_code=$(first_file \
    "${OVMF_CODE:-}" \
    "${ovmf_dir:+$ovmf_dir/OVMF_CODE.fd}" \
    "$qemu_share/edk2-x86_64-code.fd" \
    "$qemu_share/OVMF_CODE.fd" \
    /run/current-system/sw/share/qemu/edk2-x86_64-code.fd \
    /usr/share/qemu/edk2-x86_64-code.fd \
    /usr/share/OVMF/OVMF_CODE.fd \
    /usr/share/OVMF/OVMF_CODE_4M.fd \
    /usr/share/edk2/x64/OVMF_CODE.fd) || die "OVMF code image not found; set OVMF_CODE"

ovmf_vars=$(first_file \
    "${OVMF_VARS:-${OVMF_VARS_TEMPLATE:-}}" \
    "${ovmf_dir:+$ovmf_dir/OVMF_VARS.fd}" \
    "$qemu_share/edk2-i386-vars.fd" \
    "$qemu_share/OVMF_VARS.fd" \
    /run/current-system/sw/share/qemu/edk2-i386-vars.fd \
    /usr/share/qemu/edk2-i386-vars.fd \
    /usr/share/OVMF/OVMF_VARS.fd \
    /usr/share/OVMF/OVMF_VARS_4M.fd \
    /usr/share/edk2/x64/OVMF_VARS.fd) || die "OVMF variable template not found; set OVMF_VARS"

esp=
policy_esp=
cleanup_esps() {
    local directory
    for directory in "$esp" "$policy_esp"; do
        [[ -n "$directory" ]] || continue
        # Only files placed in this invocation's mktemp directories are removed.
        rm -f -- "$directory/EFI/BOOT/BOOTX64.EFI" "$directory/EFI/BOOT/MONITORX64.EFI" \
            "$directory/EFI/BOOT/GUESTX64.EFI" "$directory/EFI/Microsoft/Boot/bootmgfw.efi" \
            "$directory/EFI/ubuntu/shimx64.efi" "$directory/EFI/Test/PHYSICAL.EFI" \
            "$directory/EFI/Test/OTHERONLY.EFI"
        rmdir -- "$directory/EFI/Microsoft/Boot" "$directory/EFI/Microsoft" \
            "$directory/EFI/ubuntu" "$directory/EFI/Test" "$directory/EFI/BOOT" \
            "$directory/EFI" "$directory" 2>/dev/null || true
    done
}
trap cleanup_esps EXIT
mkdir -p -- "$stage"
# All payloads, including the fake bootmgfw.efi, live only on this test ESP.
esp=$(mktemp -d "$stage/esp.XXXXXX")
mkdir -p -- "$esp/EFI/BOOT"
install -m 0644 -- "$loader" "$esp/EFI/BOOT/BOOTX64.EFI"
if [[ -n "$monitor" ]]; then
    install -m 0644 -- "$monitor" "$esp/EFI/BOOT/MONITORX64.EFI"
else
    rm -f -- "$esp/EFI/BOOT/MONITORX64.EFI"
fi
rm -f -- "$esp/EFI/BOOT/GUESTX64.EFI" "$esp/EFI/Microsoft/Boot/bootmgfw.efi"
if [[ "$guest_location" == guest || "$guest_location" == both ]]; then
    install -m 0644 -- "$guest" "$esp/EFI/BOOT/GUESTX64.EFI"
fi
if [[ "$guest_location" == windows || "$guest_location" == both ]]; then
    mkdir -p -- "$esp/EFI/Microsoft/Boot"
    install -m 0644 -- "$guest" "$esp/EFI/Microsoft/Boot/bootmgfw.efi"
fi
esp_args=(
    -drive "if=none,id=esp,format=raw,file=fat:rw:$esp"
    -device virtio-blk-pci,drive=esp
)
if ((physical_policy)); then
    mkdir -p -- "$esp/EFI/Test" "$esp/EFI/ubuntu"
    install -m 0644 -- "$policy_loader" "$esp/EFI/Test/PHYSICAL.EFI"
    install -m 0644 -- "$guest" "$esp/EFI/ubuntu/shimx64.efi"
    policy_esp=$(mktemp -d "$stage/esp-policy-other.XXXXXX")
    mkdir -p -- "$policy_esp/EFI/Microsoft/Boot" "$policy_esp/EFI/ubuntu" "$policy_esp/EFI/Test"
    install -m 0644 -- "$guest" "$policy_esp/EFI/Microsoft/Boot/bootmgfw.efi"
    install -m 0644 -- "$guest" "$policy_esp/EFI/ubuntu/shimx64.efi"
    install -m 0644 -- "$guest" "$policy_esp/EFI/Test/OTHERONLY.EFI"
    # Present duplicate targets first, but boot only the driver on the primary
    # ESP. Both disks are immutable fixtures, never an installed Windows ESP.
    esp_args=(
        -drive "if=none,id=policy-other,format=raw,readonly=on,file=fat:ro:$policy_esp"
        -device virtio-blk-pci,drive=policy-other
        -drive "if=none,id=esp,format=raw,readonly=on,file=fat:ro:$esp"
        -device virtio-blk-pci,drive=esp,bootindex=1
    )
fi
install -m 0600 -- "$ovmf_vars" "$vars"
: >"$serial_log"
: >"$qemu_log"

qemu_pid=
monitor_fd_open=0
sleep_args=(-global ICH9-LPC.disable_s3=1 -global ICH9-LPC.disable_s4=1)
if ((acpi_s3)); then
    sleep_args=(-global ICH9-LPC.disable_s3=0 -global ICH9-LPC.disable_s4=1)
fi
reboot_args=(-no-reboot)
((allow_reboot)) && reboot_args=()
shutdown_args=(-no-shutdown)
((require_poweroff)) && shutdown_args=()
extra_device_args=()
if [[ -n "$data_disk" ]]; then
    extra_device_args+=(
        -drive "if=none,id=data,format=raw,file=$data_disk,cache=writeback"
        -device virtio-blk-pci,drive=data,serial=THINHVDATA
    )
fi
if ((usernet)); then
    extra_device_args+=(
        -netdev user,id=net0,restrict=on
        -device virtio-net-pci,netdev=net0
    )
fi
cleanup() {
    if [[ -n "$qemu_pid" ]] && kill -0 "$qemu_pid" 2>/dev/null; then
        kill "$qemu_pid" 2>/dev/null || true
        wait "$qemu_pid" 2>/dev/null || true
    fi
    if ((monitor_fd_open)); then
        exec 9>&- 9<&-
    fi
    rm -f -- "$monitor_fifo"
    cleanup_esps
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

rm -f -- "$monitor_fifo"
mkfifo -- "$monitor_fifo"
exec 9<>"$monitor_fifo"
monitor_fd_open=1

set +e
timeout --foreground --kill-after=2s "${timeout_seconds}s" \
    "$qemu" \
    -machine "q35,accel=$accel" \
    "${pci_args[@]}" \
    "${sleep_args[@]}" \
    -cpu "$cpu" \
    -smp "$smp" \
    -m "$memory" \
    -nodefaults \
    -display none \
    -monitor stdio \
    -serial "file:$serial_log" \
    "${reboot_args[@]}" \
    "${shutdown_args[@]}" \
    -drive "if=pflash,format=raw,readonly=on,file=$ovmf_code" \
    -drive "if=pflash,format=raw,file=$vars" \
    "${esp_args[@]}" \
    "${extra_device_args[@]}" \
    <"$monitor_fifo" >"$qemu_log" 2>&1 &
qemu_pid=$!
set -e

wake_cycle=0
wake_marker_seen=0
suspended_baseline=0
for ((elapsed = 0; elapsed < timeout_seconds * 10; elapsed++)); do
    if ((wake_cycle < wake_cycles)); then
        wake_marker="thin-hv: linux S3 suspend begin cycle=$((wake_cycle + 1))"
        if ((wake_marker_seen == 0)) && grep -Fq -- "$wake_marker" "$serial_log"; then
            suspended_baseline=$(grep -Fc -- 'VM status: paused (suspended)' "$qemu_log" || true)
            wake_marker_seen=1
        fi
        if ((wake_marker_seen)); then
            printf 'info status\n' >&9
            suspended_now=$(grep -Fc -- 'VM status: paused (suspended)' "$qemu_log" || true)
            if ((suspended_now > suspended_baseline)); then
                printf 'system_wakeup\n' >&9
                wake_cycle=$((wake_cycle + 1))
                wake_marker_seen=0
            fi
        fi
    fi
    if [[ -n "$failure_marker" ]] && grep -Fq -- "$failure_marker" "$serial_log"; then
        printf 'quit\n' >&9
        break
    fi
    if grep -Eq 'Kernel panic|Oops:|BUG:' "$serial_log"; then
        printf 'quit\n' >&9
        break
    fi
    if [[ "$backend" == direct-vmx ]] && ((!host_exception_test)) &&
        grep -Eq -- "$direct_failure_pattern" "$serial_log"; then
        printf 'quit\n' >&9
        break
    fi
    if ((!host_exception_test)) && grep -Fq -- "$marker" "$serial_log" &&
        { [[ -z "$return_marker" ]] || grep -Fq -- "$return_marker" "$serial_log"; } &&
        { [[ -z "$payload_marker" ]] || grep -Fq -- "$payload_marker" "$serial_log"; } &&
        { [[ -z "$variable_marker" ]] || grep -Fq -- "$variable_marker" "$serial_log"; } &&
        { [[ -z "$trusted_chainload_marker" ]] || grep -Fq -- "$trusted_chainload_marker" "$serial_log"; }; then
        if ((!require_poweroff)); then
            printf 'quit\n' >&9
            break
        fi
    fi
    kill -0 "$qemu_pid" 2>/dev/null || break
    sleep 0.1
done

set +e
wait "$qemu_pid"
qemu_status=$?
set -e
qemu_pid=

cat -- "$serial_log"
if ((qemu_status != 0)); then
    cat -- "$qemu_log" >&2
fi
if ((host_exception_test)); then
    check_host_exception_log "$qemu_status" "$serial_log" || \
        die 'root host-exception fixture transcript/status check failed'
    printf 'x86 UEFI host-exception fixture: PASS backend=direct-vmx environment=QEMU/KVM expected_stop=124 (not physical hardware)\n'
    exit 0
fi
if [[ -n "$failure_marker" ]] && grep -Fq -- "$failure_marker" "$serial_log"; then
    die "guest failure marker '$failure_marker' observed in $serial_log"
fi
grep -Fq -- "$marker" "$serial_log" || die "marker '$marker' missing from $serial_log (QEMU status $qemu_status)"
if [[ -n "$return_marker" ]]; then
    grep -Fq -- "$return_marker" "$serial_log" || die "marker '$return_marker' missing from $serial_log (QEMU status $qemu_status)"
fi
if [[ -n "$payload_marker" ]]; then
    grep -Fq -- "$payload_marker" "$serial_log" || die "marker '$payload_marker' missing from $serial_log (QEMU status $qemu_status)"
fi
if [[ -n "$variable_marker" ]]; then
    grep -Fq -- "$variable_marker" "$serial_log" || die "marker '$variable_marker' missing from $serial_log (QEMU status $qemu_status)"
fi
if [[ -n "$trusted_chainload_marker" ]]; then
    grep -Fq -- "$trusted_chainload_marker" "$serial_log" || \
        die "marker '$trusted_chainload_marker' missing from $serial_log (QEMU status $qemu_status)"
fi
check_backend_log "$backend" "$serial_log" || die "backend provenance check failed for $backend in $serial_log"
if [[ "$backend" == physical-preflight ]]; then
    check_preflight_ept_log "$accel" "$serial_log" || die 'preflight EPT construction transcript check failed'
fi
if ((physical_policy)); then
    check_physical_policy_log "$serial_log" || die 'physical-chainload policy transcript check failed'
fi
((wake_cycle == wake_cycles)) || die "observed $wake_cycle of $wake_cycles requested suspend cycles"
if ((wake_cycles)); then
    offline_count=$(grep -Fc -- 'smpboot: CPU 1 is now offline' "$serial_log" || true)
    online_count=$(grep -Fc -- 'CPU1 is up' "$serial_log" || true)
    expected_cpu_events=0
    [[ "$smp" == 2 ]] && expected_cpu_events=$wake_cycles
    ((offline_count == expected_cpu_events)) || die "observed $offline_count of $expected_cpu_events CPU1 offline events"
    ((online_count == expected_cpu_events)) || die "observed $online_count of $expected_cpu_events CPU1 online events"
fi

((qemu_status == 0)) || die "QEMU exited with status $qemu_status"

printf 'x86 UEFI smoke: PASS backend=%s environment=QEMU/%s (not physical hardware)\n' "$backend" "$accel"
