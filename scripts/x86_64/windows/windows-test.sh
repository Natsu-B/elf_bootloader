#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../../.." && pwd)
work=${WINDOWS_TEST_DIR:-"$repo_root/bin/x86_64/windows"}
iso="$work/win11-enterprise-eval-25h2-en-us.iso"
disk="$work/windows.raw"
vars="$work/windows-vars.fd"
tpm_dir="$work/tpm"
answer_dir="$work/answer"
expected_hash=a61adeab895ef5a4db436e0a7011c92a2ff17bb0357f58b13bbc4062e535e7b9
download_url='https://go.microsoft.com/fwlink/?clcid=0x409&country=us&culture=en-us&linkid=2334167'
marker='thin-hv: windows desktop'

die() {
    printf 'Windows x86 test: %s\n' "$*" >&2
    exit 1
}

need_command() {
    command -v -- "$1" >/dev/null || die "$1 not found; run through nix develop"
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

verify_iso() {
    [[ -f "$iso" ]] || die "ISO not found: $iso; run '$0 download'"
    printf '%s  %s\n' "$expected_hash" "$iso" | sha256sum -c -
}

download_iso() {
    need_command curl
    need_command sha256sum
    mkdir -p -- "$work"
    if [[ -f "$iso" ]]; then
        verify_iso
        return
    fi
    curl --proto '=https' --tlsv1.2 --fail --location --continue-at - \
        --output "$iso.part" "$download_url"
    printf '%s  %s\n' "$expected_hash" "$iso.part" | sha256sum -c -
    mv -- "$iso.part" "$iso"
    printf 'Windows x86 test: downloaded %s\n' "$iso"
}

prepare_install_media() {
    local answer_source="$repo_root/scripts/x86_64/windows"
    mkdir -p -- "$answer_dir"
    install -m 0644 -- "$answer_source/Autounattend.xml" "$answer_dir/Autounattend.xml"
    install -m 0644 -- "$answer_source/first-logon.ps1" "$answer_dir/thin-hv-first-logon.ps1"
}

stop_pid_file() {
    local pid_file=$1 pid
    [[ -r "$pid_file" ]] || return 0
    read -r pid <"$pid_file"
    [[ "$pid" =~ ^[1-9][0-9]*$ ]] || return 0
    [[ -r "/proc/$pid/comm" && "$(<"/proc/$pid/comm")" == swtpm ]] || return 0
    [[ -r "/proc/$pid/cmdline" ]] || return 0
    tr '\0' '\n' <"/proc/$pid/cmdline" | grep -Fxq -- "dir=$tpm_dir" || return 0
    kill "$pid" 2>/dev/null || true
}

run_windows() {
    local mode=$1
    local timeout_seconds memory smp disk_size ovmf_code ovmf_vars
    local qemu swtpm tpm_socket tpm_pid_file monitor_fifo serial_log qemu_log
    local qemu_pid='' elapsed=0 monitor_fd_open=0
    local -a media_args

    need_command qemu-system-x86_64
    need_command qemu-img
    need_command swtpm
    need_command sha256sum
    qemu=$(command -v qemu-system-x86_64)
    swtpm=$(command -v swtpm)
    memory=${WINDOWS_MEMORY:-4G}
    smp=${WINDOWS_SMP:-2}
    disk_size=${WINDOWS_DISK_SIZE:-80G}
    [[ "$memory" =~ ^[1-9][0-9]*[KMG]$ ]] || die 'WINDOWS_MEMORY must be a QEMU size such as 4G'
    [[ "$smp" =~ ^[1-9][0-9]*$ ]] || die 'WINDOWS_SMP must be a positive integer'
    [[ "$disk_size" =~ ^[1-9][0-9]*[KMG]$ ]] || die 'WINDOWS_DISK_SIZE must be a QEMU size such as 80G'

    ovmf_code=$(first_file "${OVMF_FULL_CODE:-}") || die 'OVMF_FULL_CODE not found; run through nix develop'
    ovmf_vars=$(first_file "${OVMF_FULL_VARS:-}") || die 'OVMF_FULL_VARS not found; run through nix develop'
    mkdir -p -- "$work" "$tpm_dir"

    if [[ "$mode" == install ]]; then
        verify_iso
        prepare_install_media
        if [[ ! -f "$disk" ]]; then
            qemu-img create -f raw "$disk" "$disk_size"
        fi
        if [[ ! -f "$vars" ]]; then
            install -m 0600 -- "$ovmf_vars" "$vars"
        fi
        timeout_seconds=${WINDOWS_INSTALL_TIMEOUT_SECONDS:-3600}
        media_args=(
            -drive "if=none,id=install,format=raw,readonly=on,file=$iso"
            -device "ide-cd,bus=ide.1,drive=install,bootindex=1"
            -drive "if=none,id=answer,format=raw,readonly=on,file=fat:$answer_dir"
            -device "usb-storage,bus=xhci.0,drive=answer,removable=on"
            -boot "once=d,menu=off"
        )
    else
        [[ -f "$disk" ]] || die "Windows disk not found: $disk"
        [[ -f "$vars" ]] || die "OVMF variables not found: $vars"
        timeout_seconds=${WINDOWS_BOOT_TIMEOUT_SECONDS:-900}
        media_args=(-boot menu=off)
    fi
    [[ "$timeout_seconds" =~ ^[1-9][0-9]*$ ]] || die 'timeout must be a positive integer'

    tpm_socket="$work/swtpm.sock"
    tpm_pid_file="$work/swtpm.pid"
    monitor_fifo="$work/qemu-monitor.in"
    serial_log="$work/$mode-serial.log"
    qemu_log="$work/$mode-qemu.log"
    stop_pid_file "$tpm_pid_file"
    rm -f -- "$tpm_socket" "$tpm_pid_file" "$monitor_fifo"
    : >"$serial_log"
    : >"$qemu_log"

    cleanup() {
        if [[ -n "$qemu_pid" ]] && kill -0 "$qemu_pid" 2>/dev/null; then
            kill "$qemu_pid" 2>/dev/null || true
            wait "$qemu_pid" 2>/dev/null || true
        fi
        stop_pid_file "$tpm_pid_file"
        if ((monitor_fd_open)); then
            exec 9>&- 9<&-
        fi
        rm -f -- "$tpm_socket" "$tpm_pid_file" "$monitor_fifo"
    }
    trap cleanup EXIT
    trap 'exit 130' INT
    trap 'exit 143' TERM

    "$swtpm" socket --tpm2 --tpmstate "dir=$tpm_dir" \
        --ctrl "type=unixio,path=$tpm_socket" --pid "file=$tpm_pid_file" \
        --log "file=$work/swtpm.log,level=2" --terminate --daemon
    for _ in {1..50}; do
        [[ -S "$tpm_socket" ]] && break
        sleep 0.1
    done
    [[ -S "$tpm_socket" ]] || die "swtpm socket was not created: $tpm_socket"

    mkfifo -- "$monitor_fifo"
    exec 9<>"$monitor_fifo"
    monitor_fd_open=1

    "$qemu" \
        -machine q35,accel=kvm,smm=on \
        -cpu host,+vmx,-hypervisor \
        -smp "$smp" \
        -m "$memory" \
        -nodefaults \
        -display none \
        -vnc "${WINDOWS_VNC:-127.0.0.1:1}" \
        -monitor stdio \
        -serial "file:$serial_log" \
        -rtc base=localtime,clock=host \
        -global driver=cfi.pflash01,property=secure,value=on \
        -drive "if=pflash,format=raw,readonly=on,file=$ovmf_code" \
        -drive "if=pflash,format=raw,file=$vars" \
        -chardev "socket,id=chrtpm,path=$tpm_socket" \
        -tpmdev emulator,id=tpm0,chardev=chrtpm \
        -device tpm-crb,tpmdev=tpm0 \
        -device VGA \
        -device qemu-xhci,id=xhci \
        -device usb-kbd,bus=xhci.0 \
        -device usb-tablet,bus=xhci.0 \
        -drive "if=none,id=windisk,format=raw,file=$disk,cache=writeback,discard=unmap,detect-zeroes=unmap" \
        -device ide-hd,bus=ide.0,drive=windisk,bootindex=2 \
        -netdev user,id=net0 \
        -device e1000e,netdev=net0 \
        "${media_args[@]}" \
        <"$monitor_fifo" >"$qemu_log" 2>&1 &
    qemu_pid=$!
    printf 'Windows x86 test: %s running as PID %s; VNC %s\n' \
        "$mode" "$qemu_pid" "${WINDOWS_VNC:-127.0.0.1:1}"

    while ((elapsed < timeout_seconds)); do
        if grep -Fq -- "$marker" "$serial_log"; then
            printf 'Windows x86 test: observed %s\n' "$marker"
            break
        fi
        if ! kill -0 "$qemu_pid" 2>/dev/null; then
            wait "$qemu_pid" || true
            qemu_pid=
            tail -n 80 -- "$qemu_log" >&2
            die "QEMU exited before marker; logs: $serial_log $qemu_log"
        fi
        sleep 2
        elapsed=$((elapsed + 2))
        if ((elapsed % 30 == 0)); then
            printf 'Windows x86 test: waiting for marker (%ss/%ss)\n' "$elapsed" "$timeout_seconds"
        fi
    done
    grep -Fq -- "$marker" "$serial_log" || die "marker timeout; logs: $serial_log $qemu_log"

    printf 'system_powerdown\n' >&9
    for _ in {1..120}; do
        kill -0 "$qemu_pid" 2>/dev/null || break
        sleep 1
    done
    if kill -0 "$qemu_pid" 2>/dev/null; then
        printf 'quit\n' >&9
    fi
    wait "$qemu_pid" || true
    qemu_pid=
    tail -n 40 -- "$serial_log"
    cleanup
    trap - EXIT INT TERM
}

usage() {
    printf 'usage: %s download|verify|install|boot\n' "$0"
}

case ${1:-} in
    download) download_iso ;;
    verify) verify_iso ;;
    install) run_windows install ;;
    boot) run_windows boot ;;
    -h | --help | help) usage ;;
    *) usage >&2; exit 2 ;;
esac
