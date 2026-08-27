#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../../.." && pwd)
work=${WINDOWS_TEST_DIR:-"$repo_root/bin/x86_64/windows"}
iso="$work/win11-enterprise-eval-25h2-en-us.iso"
disk="$work/windows.raw"
vars="$work/windows-vars.fd"
monitor_vars="$work/monitor-vars.fd"
hyperv_vars="$work/hyperv-vars.fd"
hyperv_disk="$work/windows-hyperv.qcow2"
hyperv_ready="$work/hyperv-ready"
base_tpm_dir="$work/tpm"
hyperv_tpm_dir="$work/hyperv-tpm"
answer_dir="$work/answer"
monitor_esp="$work/monitor-loader-esp"
hyperv_media="$work/hyperv-media"
loader="$repo_root/bin/x86_64/x86-uefi-loader.efi"
runtime_monitor="$repo_root/bin/x86_64/x86-uefi-monitor.efi"
expected_hash=a61adeab895ef5a4db436e0a7011c92a2ff17bb0357f58b13bbc4062e535e7b9
download_url='https://go.microsoft.com/fwlink/?clcid=0x409&country=us&culture=en-us&linkid=2334167'
marker='thin-hv: windows desktop'
desktop_marker=thinhvwindowsdesktop
hyperv_marker='thin-hv: windows hyperv PASS'

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
    need_command sha256sum
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

prepare_monitor_media() {
    [[ -f "$loader" ]] || die "loader not found: $loader; run 'cargo xbuild x86'"
    [[ -f "$runtime_monitor" ]] || die "runtime monitor not found: $runtime_monitor; run 'cargo xbuild x86'"

    mkdir -p -- "$monitor_esp/EFI/BOOT"
    rm -f -- "$monitor_esp/EFI/BOOT/GUESTX64.EFI"
    install -m 0644 -- "$loader" "$monitor_esp/EFI/BOOT/BOOTX64.EFI"
    install -m 0644 -- "$runtime_monitor" "$monitor_esp/EFI/BOOT/MONITORX64.EFI"
}

prepare_hyperv_media() {
    local source="$repo_root/scripts/x86_64/windows"

    [[ -f "$disk" ]] || die "Windows disk not found: $disk"
    [[ -f "$vars" ]] || die "OVMF variables not found: $vars"
    [[ -d "$base_tpm_dir" ]] || die "TPM state not found: $base_tpm_dir"
    if [[ -f "$hyperv_disk" && -f "$hyperv_vars" && -d "$hyperv_tpm_dir" ]]; then
        :
    elif [[ ! -e "$hyperv_disk" && ! -e "$hyperv_vars" && ! -e "$hyperv_tpm_dir" ]]; then
        qemu-img create -f qcow2 -F raw -b "$disk" "$hyperv_disk"
        install -m 0600 -- "$vars" "$hyperv_vars"
        mkdir -p -- "$hyperv_tpm_dir"
        cp -a -- "$base_tpm_dir/." "$hyperv_tpm_dir/"
        rm -f -- "$hyperv_ready"
    else
        die "partial Hyper-V state; remove together: $hyperv_disk $hyperv_vars $hyperv_tpm_dir $hyperv_ready"
    fi
    mkdir -p -- "$hyperv_media"
    install -m 0644 -- "$source/hyperv-enable.ps1" "$hyperv_media/hyperv-enable.ps1"
    install -m 0644 -- "$source/hyperv-verify.ps1" "$hyperv_media/hyperv-verify.ps1"
}

stop_pid_file() {
    local pid_file=$1 pid
    [[ -r "$pid_file" ]] || return 0
    read -r pid <"$pid_file" || return 0
    [[ "$pid" =~ ^[1-9][0-9]*$ ]] || return 0
    [[ -r "/proc/$pid/comm" && "$(<"/proc/$pid/comm")" == swtpm ]] || return 0
    [[ -r "/proc/$pid/cmdline" ]] || return 0
    tr '\0' '\n' <"/proc/$pid/cmdline" | grep -Fxq -- "dir=$tpm_dir" || return 0
    kill "$pid" 2>/dev/null || true
}

run_dialog_command() {
    local command=$1 submit_key=${2:-ret} index key

    printf 'sendkey esc 20\n' >&9
    sleep 0.1
    printf 'sendkey esc 20\n' >&9
    sleep 1
    printf 'sendkey meta_l-r 20\n' >&9
    sleep 3
    for ((index = 0; index < ${#command}; index++)); do
        key=${command:index:1}
        case $key in
            ' ') key=spc ;;
            /) key=slash ;;
            \\) key=backslash ;;
            :) key=shift-semicolon ;;
            .) key='dot' ;;
            -) key=minus ;;
            '>') key=shift-dot ;;
        esac
        printf 'sendkey %s 20\n' "$key" >&9
        sleep 0.1
    done
    printf 'sendkey %s 20\n' "$submit_key" >&9
}

probe_windows_desktop() {
    # ponytail: the Run dialog is the desktop-ready probe; use a guest agent
    # only if later tests need general command execution inside Windows.
    run_dialog_command "cmd /c echo $desktop_marker>com2"
}

probe_hyperv_enable() {
    run_dialog_command 'powershell -nop -ep bypass -f d:\hyperv-enable.ps1' ctrl-shift-ret
    sleep 5
    printf 'sendkey alt-y 20\n' >&9
}

run_windows() {
    local mode=$1
    local timeout_seconds memory smp disk_size ovmf_code ovmf_vars active_vars
    local disk_image=$disk disk_format=raw tpm_dir=$base_tpm_dir tpm_instance=base
    local qemu swtpm tpm_socket tpm_pid_file monitor_fifo serial_log desktop_serial_log qemu_log
    local expected_marker marker_log
    local qemu_pid='' qemu_status elapsed=0 monitor_fd_open=0 hyperv_probe_sent=0
    local -a media_args

    need_command qemu-system-x86_64
    need_command swtpm
    qemu=$(command -v qemu-system-x86_64)
    swtpm=$(command -v swtpm)
    memory=${WINDOWS_MEMORY:-4G}
    if [[ "$mode" == monitor || "$mode" == monitor-hyperv ]]; then
        [[ "$memory" == 4G ]] || die 'monitor mode currently requires WINDOWS_MEMORY=4G'
        smp=1
    elif [[ "$mode" == hyperv ]]; then
        [[ "$memory" == 4G ]] || die 'Hyper-V control currently requires WINDOWS_MEMORY=4G'
        smp=2
    else
        smp=${WINDOWS_SMP:-2}
    fi
    [[ "$memory" =~ ^[1-9][0-9]*[KMG]$ ]] || die 'WINDOWS_MEMORY must be a QEMU size such as 4G'
    [[ "$smp" =~ ^[1-9][0-9]*$ ]] || die 'WINDOWS_SMP must be a positive integer'

    ovmf_code=$(first_file "${OVMF_FULL_CODE:-}") || die 'OVMF_FULL_CODE not found; run through nix develop'
    ovmf_vars=$(first_file "${OVMF_FULL_VARS:-}") || die 'OVMF_FULL_VARS not found; run through nix develop'
    mkdir -p -- "$work"
    if [[ -f "$hyperv_disk" && "$mode" != hyperv && "$mode" != monitor-hyperv ]]; then
        # ponytail: keep the raw backing immutable instead of duplicating its
        # allocated blocks; remove all Hyper-V state before changing the base.
        die "Hyper-V overlay exists; remove its disk, vars, TPM, and ready marker together before changing the base"
    fi

    if [[ "$mode" == install ]]; then
        need_command qemu-img
        disk_size=${WINDOWS_DISK_SIZE:-80G}
        [[ "$disk_size" =~ ^[1-9][0-9]*[KMG]$ ]] || \
            die 'WINDOWS_DISK_SIZE must be a QEMU size such as 80G'
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
        active_vars=$vars
    elif [[ "$mode" == monitor ]]; then
        [[ -f "$disk" ]] || die "Windows disk not found: $disk"
        prepare_monitor_media
        install -m 0600 -- "$ovmf_vars" "$monitor_vars"
        timeout_seconds=${WINDOWS_BOOT_TIMEOUT_SECONDS:-900}
        active_vars=$monitor_vars
        media_args=(
            -drive "if=none,id=monitor-esp,format=raw,snapshot=on,file=fat:ro:$monitor_esp"
            -device "ide-hd,bus=ide.1,drive=monitor-esp,bootindex=1"
            -boot "menu=off,strict=on"
        )
    elif [[ "$mode" == hyperv ]]; then
        need_command qemu-img
        prepare_hyperv_media
        timeout_seconds=${WINDOWS_HYPERV_TIMEOUT_SECONDS:-1200}
        active_vars=$hyperv_vars
        disk_image=$hyperv_disk
        disk_format=qcow2
        tpm_dir=$hyperv_tpm_dir
        tpm_instance=hyperv
        media_args=(
            -drive "if=none,id=hyperv-media,format=raw,readonly=on,file=fat:$hyperv_media"
            -device "usb-storage,bus=xhci.0,drive=hyperv-media,removable=on"
            -boot "menu=off"
        )
    elif [[ "$mode" == monitor-hyperv ]]; then
        need_command qemu-img
        prepare_hyperv_media
        [[ -f "$hyperv_ready" ]] || die "direct Hyper-V PASS missing; run '$0 hyperv' first"
        prepare_monitor_media
        install -m 0600 -- "$ovmf_vars" "$monitor_vars"
        timeout_seconds=${WINDOWS_HYPERV_TIMEOUT_SECONDS:-600}
        active_vars=$monitor_vars
        disk_image=$hyperv_disk
        disk_format=qcow2
        tpm_dir=$hyperv_tpm_dir
        tpm_instance=hyperv
        media_args=(
            -drive "if=none,id=monitor-esp,format=raw,snapshot=on,file=fat:ro:$monitor_esp"
            -device "ide-hd,bus=ide.1,drive=monitor-esp,bootindex=1"
            -boot "menu=off,strict=on"
        )
    else
        [[ -f "$disk" ]] || die "Windows disk not found: $disk"
        [[ -f "$vars" ]] || die "OVMF variables not found: $vars"
        timeout_seconds=${WINDOWS_BOOT_TIMEOUT_SECONDS:-900}
        media_args=(-boot menu=off)
        active_vars=$vars
    fi
    [[ "$timeout_seconds" =~ ^[1-9][0-9]*$ ]] || die 'timeout must be a positive integer'
    mkdir -p -- "$tpm_dir"

    tpm_socket="$work/swtpm-$tpm_instance.sock"
    tpm_pid_file="$work/swtpm-$tpm_instance.pid"
    monitor_fifo="$work/qemu-monitor-$mode.in"
    serial_log="$work/$mode-serial.log"
    desktop_serial_log="$work/$mode-desktop-serial.log"
    qemu_log="$work/$mode-qemu.log"
    stop_pid_file "$tpm_pid_file"
    rm -f -- "$tpm_socket" "$tpm_pid_file" "$monitor_fifo"
    : >"$serial_log"
    : >"$desktop_serial_log"
    : >"$qemu_log"
    if [[ "$mode" == hyperv || "$mode" == monitor-hyperv ]]; then
        expected_marker=$hyperv_marker
        marker_log=$desktop_serial_log
    elif [[ "$mode" == monitor ]]; then
        expected_marker=$desktop_marker
        marker_log=$desktop_serial_log
    else
        expected_marker=$marker
        marker_log=$serial_log
    fi

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

    # ponytail: keep q35 PCI MMIO inside the current 8 GiB EPT; bare-metal
    # launch must derive RAM/MMIO ranges from firmware resources.
    "$qemu" \
        -machine q35,accel=kvm,smm=on \
        -global q35-pcihost.pci-hole64-size=1G \
        -cpu host,+vmx,-hypervisor \
        -fw_cfg name=opt/ovmf/X-PciMmio64Mb,string=1024 \
        -smp "$smp" \
        -m "$memory" \
        -nodefaults \
        -display none \
        -vnc "${WINDOWS_VNC:-127.0.0.1:1}" \
        -monitor stdio \
        -serial "file:$serial_log" \
        -serial "file:$desktop_serial_log" \
        -rtc base=localtime,clock=host \
        -global driver=cfi.pflash01,property=secure,value=on \
        -drive "if=pflash,format=raw,readonly=on,file=$ovmf_code" \
        -drive "if=pflash,format=raw,file=$active_vars" \
        -chardev "socket,id=chrtpm,path=$tpm_socket" \
        -tpmdev emulator,id=tpm0,chardev=chrtpm \
        -device tpm-crb,tpmdev=tpm0 \
        -device VGA \
        -device qemu-xhci,id=xhci \
        -device usb-kbd,bus=xhci.0 \
        -device usb-tablet,bus=xhci.0 \
        -drive "if=none,id=windisk,format=$disk_format,file=$disk_image,cache=writeback,discard=unmap,detect-zeroes=unmap" \
        -device ide-hd,bus=ide.0,drive=windisk,bootindex=2 \
        -netdev user,id=net0 \
        -device e1000e,netdev=net0 \
        "${media_args[@]}" \
        <"$monitor_fifo" >"$qemu_log" 2>&1 &
    qemu_pid=$!
    printf 'Windows x86 test: %s running as PID %s; VNC %s\n' \
        "$mode" "$qemu_pid" "${WINDOWS_VNC:-127.0.0.1:1}"
    if [[ "$mode" == install ]]; then
        # Microsoft's UEFI DVD loader waits for "Press any key" before Setup.
        for _ in {1..20}; do
            sleep 1
            printf 'sendkey spc\n' >&9
        done
    fi

    while ((elapsed < timeout_seconds)); do
        if grep -Fq -- "$expected_marker" "$marker_log"; then
            printf 'Windows x86 test: observed %s\n' "$expected_marker"
            break
        fi
        if ! kill -0 "$qemu_pid" 2>/dev/null; then
            set +e
            wait "$qemu_pid"
            qemu_status=$?
            set -e
            qemu_pid=
            tail -n 80 -- "$qemu_log" >&2
            die "QEMU exited with status $qemu_status before marker; logs: $serial_log $marker_log $qemu_log"
        fi
        sleep 2
        elapsed=$((elapsed + 2))
        if [[ "$mode" == monitor ]] && ((elapsed >= 150 && elapsed % 30 == 0)); then
            probe_windows_desktop
        fi
        if [[ "$mode" == hyperv ]] && ((elapsed >= 150 && !hyperv_probe_sent)); then
            probe_hyperv_enable
            hyperv_probe_sent=1
        fi
        if ((elapsed % 30 == 0)); then
            printf 'Windows x86 test: waiting for marker (%ss/%ss)\n' "$elapsed" "$timeout_seconds"
        fi
    done
    grep -Fq -- "$expected_marker" "$marker_log" || \
        die "marker timeout; logs: $serial_log $marker_log $qemu_log"
    if [[ "$mode" == hyperv ]]; then
        : >"$hyperv_ready"
    fi
    if [[ "$mode" == monitor || "$mode" == monitor-hyperv ]]; then
        grep -Fq -- 'thin-hv: runtime monitor active' "$serial_log" || \
            die "monitor marker missing from $serial_log"
    fi

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
    printf 'usage: %s download|verify|install|boot|monitor|hyperv|monitor-hyperv\n' "$0"
}

case ${1:-} in
    download) download_iso ;;
    verify) verify_iso ;;
    install) run_windows install ;;
    boot) run_windows boot ;;
    monitor) run_windows monitor ;;
    hyperv) run_windows hyperv ;;
    monitor-hyperv) run_windows monitor-hyperv ;;
    -h | --help | help) usage ;;
    *) usage >&2; exit 2 ;;
esac
