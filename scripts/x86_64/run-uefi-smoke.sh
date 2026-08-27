#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
loader=${1:-"$repo_root/bin/x86_64/x86-uefi-loader.efi"}
guest=${2:-"$repo_root/bin/x86_64/x86_guest_uefi_test.efi"}
stage="$repo_root/bin/x86_64"
esp="$stage/esp"
serial_log="$stage/serial.log"
vars="$stage/OVMF_VARS.fd"
marker='thin-hv: uefi entry'
vmx_marker='thin-hv: vmx guest PASS'
payload_marker='thin-hv: guest uefi payload'
timeout_seconds=${X86_UEFI_TIMEOUT_SECONDS:-10}

die() {
    printf 'x86 UEFI smoke: %s\n' "$*" >&2
    exit 1
}

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
[[ -f "$guest" ]] || die "guest payload not found: $guest"
[[ "$timeout_seconds" =~ ^[1-9][0-9]*$ ]] || die 'X86_UEFI_TIMEOUT_SECONDS must be a positive integer'
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
install -m 0644 -- "$guest" "$esp/EFI/BOOT/GUESTX64.EFI"
install -m 0600 -- "$ovmf_vars" "$vars"
: >"$serial_log"

set +e
timeout --foreground --kill-after=2s "${timeout_seconds}s" \
    "$qemu" \
    -machine q35,accel=kvm \
    -cpu host,+vmx,-hypervisor \
    -smp 1 \
    -m 256M \
    -nodefaults \
    -display none \
    -monitor none \
    -serial "file:$serial_log" \
    -no-reboot \
    -no-shutdown \
    -drive "if=pflash,format=raw,readonly=on,file=$ovmf_code" \
    -drive "if=pflash,format=raw,file=$vars" \
    -drive "if=none,id=esp,format=raw,file=fat:rw:$esp" \
    -device virtio-blk-pci,drive=esp
qemu_status=$?
set -e

cat -- "$serial_log"
grep -Fq -- "$marker" "$serial_log" || die "marker '$marker' missing from $serial_log (QEMU status $qemu_status)"
grep -Fq -- "$vmx_marker" "$serial_log" || die "marker '$vmx_marker' missing from $serial_log (QEMU status $qemu_status)"
grep -Fq -- "$payload_marker" "$serial_log" || die "marker '$payload_marker' missing from $serial_log (QEMU status $qemu_status)"

case $qemu_status in
    0 | 124) ;;
    *) die "QEMU exited with status $qemu_status" ;;
esac

printf 'x86 UEFI smoke: observed %s\n' "$marker"
