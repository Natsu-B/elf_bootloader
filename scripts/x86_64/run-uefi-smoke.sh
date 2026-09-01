#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
loader=${1:-"$repo_root/bin/x86_64/x86-uefi-loader.efi"}
guest=${2:-"$repo_root/bin/x86_64/x86_guest_uefi_test.efi"}
monitor=${X86_MONITOR_IMAGE-"$(dirname -- "$loader")/x86-uefi-monitor.efi"}
stage="$repo_root/bin/x86_64"
esp="$stage/esp"
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
trusted_forbidden_markers=(
    'thin-hv: loading runtime monitor'
    'thin-hv: runtime monitor active'
    'thin-hv: variable overlay profile='
    'thin-hv: L1 VMLAUNCH direct='
)
if [[ ! ${X86_VARIABLE_MARKER+x} && ${guest##*/} == x86_guest_uefi_test.efi ]]; then
    if [[ ${loader##*/} == x86-uefi-kvm-loader.efi ]]; then
        variable_marker='thin-hv: uefi native variables PASS'
    else
        variable_marker='thin-hv: uefi variable overlay PASS'
    fi
fi
timeout_seconds=${X86_UEFI_TIMEOUT_SECONDS:-10}
memory=${X86_UEFI_MEMORY:-256M}
smp=${X86_UEFI_SMP:-1}
cpu=${X86_UEFI_CPU:-host,+vmx,-hypervisor}
acpi_s3=${X86_UEFI_ACPI_S3:-0}
wake_cycles=${X86_UEFI_WAKE_CYCLES:-0}
allow_reboot=${X86_UEFI_ALLOW_REBOOT:-0}
require_poweroff=${X86_UEFI_REQUIRE_POWEROFF:-0}
data_disk=${X86_UEFI_DATA_DISK:-}
usernet=${X86_UEFI_USERNET:-0}

die() {
    printf 'x86 UEFI smoke: %s\n' "$*" >&2
    exit 1
}

case "$guest_location" in
    guest | both) trusted_profile=2 ;;
    windows) trusted_profile=1 ;;
    *) die "X86_UEFI_GUEST_LOCATION must be guest, windows, or both" ;;
esac
if [[ ${loader##*/} == x86-uefi-kvm-loader.efi ]]; then
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
[[ -f "$guest" ]] || die "guest payload not found: $guest"
[[ "$timeout_seconds" =~ ^[1-9][0-9]*$ ]] || die 'X86_UEFI_TIMEOUT_SECONDS must be a positive integer'
[[ "$memory" =~ ^[1-9][0-9]*[KMG]$ ]] || die 'X86_UEFI_MEMORY must be a positive QEMU size such as 256M'
[[ "$smp" =~ ^[1-9][0-9]*$ ]] || die 'X86_UEFI_SMP must be a positive integer'
[[ -n "$cpu" ]] || die 'X86_UEFI_CPU must not be empty'
[[ "$acpi_s3" =~ ^[01]$ ]] || die 'X86_UEFI_ACPI_S3 must be 0 or 1'
[[ "$wake_cycles" =~ ^[0-9]+$ ]] || die 'X86_UEFI_WAKE_CYCLES must be a non-negative integer'
[[ "$allow_reboot" =~ ^[01]$ ]] || die 'X86_UEFI_ALLOW_REBOOT must be 0 or 1'
[[ "$require_poweroff" =~ ^[01]$ ]] || die 'X86_UEFI_REQUIRE_POWEROFF must be 0 or 1'
[[ "$usernet" =~ ^[01]$ ]] || die 'X86_UEFI_USERNET must be 0 or 1'
[[ -z "$data_disk" || -f "$data_disk" ]] || die "data disk not found: $data_disk"
((wake_cycles == 0 || acpi_s3 == 1)) || die 'X86_UEFI_WAKE_CYCLES requires X86_UEFI_ACPI_S3=1'
if ((acpi_s3)); then
    [[ ${loader##*/} == x86-uefi-kvm-loader.efi ]] || \
        die 'X86_UEFI_ACPI_S3 is restricted to trusted outer-KVM artifacts'
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

mkdir -p -- "$esp/EFI/BOOT"
install -m 0644 -- "$loader" "$esp/EFI/BOOT/BOOTX64.EFI"
if [[ -n "$monitor" ]]; then
    install -m 0644 -- "$monitor" "$esp/EFI/BOOT/MONITORX64.EFI"
else
    rm -f -- "$esp/EFI/BOOT/MONITORX64.EFI"
fi
rm -f -- "$esp/EFI/BOOT/GUESTX64.EFI" "$esp/EFI/Microsoft/Boot/bootmgfw.efi"
if [[ "$guest_location" != windows ]]; then
    install -m 0644 -- "$guest" "$esp/EFI/BOOT/GUESTX64.EFI"
fi
if [[ "$guest_location" != guest ]]; then
    mkdir -p -- "$esp/EFI/Microsoft/Boot"
    install -m 0644 -- "$guest" "$esp/EFI/Microsoft/Boot/bootmgfw.efi"
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
    -machine q35,accel=kvm \
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
    -drive "if=none,id=esp,format=raw,file=fat:rw:$esp" \
    -device virtio-blk-pci,drive=esp \
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
    if grep -Fq -- "$marker" "$serial_log" &&
        { [[ -z "$return_marker" ]] || grep -Fq -- "$return_marker" "$serial_log"; } &&
        grep -Fq -- "$payload_marker" "$serial_log" &&
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
if [[ -n "$failure_marker" ]] && grep -Fq -- "$failure_marker" "$serial_log"; then
    die "guest failure marker '$failure_marker' observed in $serial_log"
fi
grep -Fq -- "$marker" "$serial_log" || die "marker '$marker' missing from $serial_log (QEMU status $qemu_status)"
if [[ -n "$return_marker" ]]; then
    grep -Fq -- "$return_marker" "$serial_log" || die "marker '$return_marker' missing from $serial_log (QEMU status $qemu_status)"
fi
grep -Fq -- "$payload_marker" "$serial_log" || die "marker '$payload_marker' missing from $serial_log (QEMU status $qemu_status)"
if [[ -n "$variable_marker" ]]; then
    grep -Fq -- "$variable_marker" "$serial_log" || die "marker '$variable_marker' missing from $serial_log (QEMU status $qemu_status)"
fi
if [[ -n "$trusted_chainload_marker" ]]; then
    grep -Fq -- "$trusted_chainload_marker" "$serial_log" || \
        die "marker '$trusted_chainload_marker' missing from $serial_log (QEMU status $qemu_status)"
    for forbidden_marker in "${trusted_forbidden_markers[@]}"; do
        ! grep -Fq -- "$forbidden_marker" "$serial_log" || \
            die "trusted loader emitted forbidden marker '$forbidden_marker'"
    done
fi
((wake_cycle == wake_cycles)) || die "observed $wake_cycle of $wake_cycles requested suspend cycles"
if ((wake_cycles)); then
    offline_count=$(grep -Fc -- 'smpboot: CPU 1 is now offline' "$serial_log" || true)
    online_count=$(grep -Fc -- 'CPU1 is up' "$serial_log" || true)
    ((offline_count == wake_cycles)) || die "observed $offline_count of $wake_cycles CPU1 offline events"
    ((online_count == wake_cycles)) || die "observed $online_count of $wake_cycles CPU1 online events"
fi

((qemu_status == 0)) || die "QEMU exited with status $qemu_status"

printf 'x86 UEFI smoke: observed %s\n' "$marker"
