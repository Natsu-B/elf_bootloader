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
wsl_ready="$work/wsl-ready"
wsl_msi="$work/wsl.2.7.11.0.x64.msi"
base_tpm_dir="$work/tpm"
hyperv_tpm_dir="$work/hyperv-tpm"
answer_dir="$work/answer"
monitor_esp="$work/monitor-loader-esp"
hyperv_media="$work/hyperv-media"
loader="$repo_root/bin/x86_64/x86-uefi-loader.efi"
runtime_monitor="$repo_root/bin/x86_64/x86-uefi-monitor.efi"
trusted_kvm_loader="$repo_root/bin/x86_64/x86-uefi-kvm-loader.efi"
expected_hash=a61adeab895ef5a4db436e0a7011c92a2ff17bb0357f58b13bbc4062e535e7b9
download_url='https://go.microsoft.com/fwlink/?clcid=0x409&country=us&culture=en-us&linkid=2334167'
wsl_msi_hash=a611ddacee689d2fb1fb5319e58af7f3998864d86cdce632eadd8e61614a0f9d
wsl_msi_url='https://github.com/microsoft/WSL/releases/download/2.7.11/wsl.2.7.11.0.x64.msi'
marker='thin-hv: windows desktop'
desktop_marker=thinhvwindowsdesktop
hyperv_marker='thin-hv: windows hyperv PASS'
wsl_marker='thin-hv: windows wsl2 PASS'
wsl_fail_marker='thin-hv: windows wsl2 FAIL'
daily_soak_marker='thin-hv: windows daily soak PASS'
daily_soak_fail_marker='thin-hv: windows daily soak FAIL'
s4_request_marker='thin-hv: windows hibernate request'
s4_pass_marker='thin-hv: windows hibernate PASS state=S4 guest_resume=1'
s4_fail_marker='thin-hv: windows hibernate FAIL'
trusted_marker='thin-hv: trusted outer KVM direct chainload profile=1 resident_runtime=0'
physical_test_marker='thin-hv: windows physical-status self-test PASS exit=0'
physical_test_failure='thin-hv: windows physical-status self-test FAIL'
physical_test_json='{"schema":"thin-hv.physical-status.self-test.v1","result":"PASS","hardware_queries":0}'

die() {
    printf 'Windows x86 test: %s\n' "$*" >&2
    exit 1
}

need_command() {
    command -v -- "$1" >/dev/null || die "$1 not found; run through nix develop"
}

host_uptime_seconds() {
    local uptime

    read -r uptime _ </proc/uptime
    printf '%s\n' "${uptime%%.*}"
}

serial_has_exact_marker() {
    local expected=$1
    shift

    [[ -n "$expected" && "$expected" != *$'\r'* && "$expected" != *$'\n'* ]] || return 2
    # cmd.exe emits CRLF, while SerialPort.WriteLine emits LF. Accept exactly
    # those records, without deleting embedded CR or permitting extra text.
    # Do not use -q: offset-pipeline producers must drain under pipefail.
    LC_ALL=C grep -aFx -e "$expected" -e "$expected"$'\r' -- "$@" >/dev/null
}

serial_has_direct_failure() {
    # L0 emits these only on terminal paths. Stop a failed disposable VM without
    # spending its entire boot timeout at the frozen firmware/guest screen.
    LC_ALL=C grep -aE '^thin-hv: (vmx guest FAIL:|vmx smoke FAIL:|resident VMX launch FAIL:|host exception FAIL:|nested VMX abort)' -- "$@" >/dev/null
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

verify_wsl_msi() {
    need_command sha256sum
    [[ -f "$wsl_msi" ]] || die "WSL MSI not found: $wsl_msi; run '$0 download-wsl'"
    printf '%s  %s\n' "$wsl_msi_hash" "$wsl_msi" | sha256sum -c -
}

download_wsl_msi() {
    need_command curl
    need_command sha256sum
    mkdir -p -- "$work"
    if [[ -f "$wsl_msi" ]]; then
        verify_wsl_msi
        return
    fi
    curl --proto '=https' --tlsv1.2 --fail --location --continue-at - \
        --output "$wsl_msi.part" "$wsl_msi_url"
    printf '%s  %s\n' "$wsl_msi_hash" "$wsl_msi.part" | sha256sum -c -
    mv -- "$wsl_msi.part" "$wsl_msi"
    printf 'Windows x86 test: downloaded %s\n' "$wsl_msi"
}

prepare_install_media() {
    local answer_source="$repo_root/scripts/x86_64/windows"
    mkdir -p -- "$answer_dir"
    install -m 0644 -- "$answer_source/Autounattend.xml" "$answer_dir/Autounattend.xml"
    install -m 0644 -- "$answer_source/first-logon.ps1" "$answer_dir/thin-hv-first-logon.ps1"
}

copy_windows_test_esp() (
    local source=$1 format=$2 destination=$3 temporary='' image_bytes offset extent esp_bytes tail_sector
    local selector="$repo_root/scripts/x86_64/windows/windows-esp-offset.py"
    need_command sfdisk
    need_command mcopy
    need_command python3
    [[ -f "$source" ]] || die 'same-ESP fixture requires a regular QEMU Windows image'
    if [[ "$format" == qcow2 ]]; then
        need_command qemu-img
        need_command truncate
        need_command dd
        temporary=$(mktemp -d "$work/monitor-esp-source.XXXXXX")
        # Only these freshly created disposable files are removed; the source
        # and backing chain stay read-only. Never materialize the Windows volume
        # merely to copy its ESP (an 80 GiB conversion can exhaust CI storage).
        trap 'rm -f -- "$temporary/gpt.raw" "$temporary/tail.raw" "$temporary/esp.raw"; rmdir -- "$temporary"' EXIT
        image_bytes=$(qemu-img info -f qcow2 --output=json -- "$source" | python3 "$selector" --virtual-size) \
            || die 'unsupported Windows QEMU image geometry'
        # Bounded primary/backup GPT windows. sfdisk still validates the tables;
        # layouts whose arrays lie outside these windows fail, with no fallback.
        # The sparse middle is never read as partition content or written back.
        tail_sector=$((image_bytes / 512 - 2048))
        qemu-img dd -f qcow2 -O raw bs=512 count=2048 "if=$source" "of=$temporary/gpt.raw"
        qemu-img dd -f qcow2 -O raw bs=512 "skip=$tail_sector" "if=$source" "of=$temporary/tail.raw"
        [[ $(stat -Lc %s -- "$temporary/gpt.raw") == 1048576 &&
           $(stat -Lc %s -- "$temporary/tail.raw") == 1048576 ]] || die 'incomplete Windows test GPT read'
        truncate -s "$image_bytes" "$temporary/gpt.raw"
        dd "if=$temporary/tail.raw" "of=$temporary/gpt.raw" bs=512 "seek=$tail_sector" conv=notrunc status=none
        extent=$(sfdisk --json "$temporary/gpt.raw" | python3 "$selector" --extent "$image_bytes") \
            || die 'cannot identify one original Windows test ESP'
        read -r offset esp_bytes <<< "$extent"
        # QEMU 10.1 img_dd caps the input at count*bs BEFORE applying skip;
        # unlike dd(1), count is the exclusive end here. The checked extent's
        # sum is <= image_bytes <= INT64_MAX. Reject any short/changed semantics.
        qemu-img dd -f qcow2 -O raw bs=512 "skip=$((offset / 512))" "count=$(((offset + esp_bytes) / 512))" \
            "if=$source" "of=$temporary/esp.raw"
        [[ $(stat -Lc %s -- "$temporary/esp.raw") == "$esp_bytes" ]] || die 'incomplete Windows test ESP read'
        source="$temporary/esp.raw"
        offset=0
    elif [[ "$format" == raw ]]; then
        image_bytes=$(stat -Lc %s -- "$source")
        offset=$(sfdisk --json -- "$source" | python3 "$selector" "$image_bytes") \
            || die 'cannot identify one original Windows test ESP'
    else
        die 'unsupported Windows ESP source format'
    fi
    mkdir -p -- "$destination/EFI"
    # Read the existing ESP with mtools; no mount, loop device, BCD edit or
    # source-disk write. This is disposable QEMU staging, never claimed as the
    # physical motherboard's original ESP. Hyper-V copies its own overlay BCD,
    # not the normal Windows base image's different boot configuration.
    mcopy -s -i "$source@@$offset" ::/EFI/Microsoft "$destination/EFI/" \
        || die 'cannot read Windows boot files for same-ESP QEMU fixture'
    [[ -f "$destination/EFI/Microsoft/Boot/bootmgfw.efi" && -f "$destination/EFI/Microsoft/Boot/BCD" ]] \
        || die 'incomplete original Windows boot files in QEMU fixture'
)

prepare_monitor_media() {
    local boot_loader=${1:-$loader}
    local monitor_image=${2-$runtime_monitor}

    [[ -f "$boot_loader" ]] || die "loader not found: $boot_loader; run 'cargo xbuild x86'"
    if [[ -n "$monitor_image" ]]; then
        [[ -f "$monitor_image" ]] || \
            die "runtime monitor not found: $monitor_image; run 'cargo xbuild x86'"
    fi

    if [[ ${direct_mode:-qemu-research} == physical-uefi || ${direct_mode:-qemu-research} == smp-uefi ]]; then
        monitor_esp=$(mktemp -d "$work/monitor-physical-esp.XXXXXX")
        if [[ "$mode" == monitor-hyperv ]]; then
            copy_windows_test_esp "$hyperv_disk" qcow2 "$monitor_esp"
        else
            copy_windows_test_esp "$disk" raw "$monitor_esp"
        fi
        printf 'Windows x86 test: same-ESP fixture source=original-test-esp copy=read-only-source bcd_modified=0 physical_hardware=0\n'
    fi

    mkdir -p -- "$monitor_esp/EFI/BOOT"
    rm -f -- "$monitor_esp/EFI/BOOT/GUESTX64.EFI" \
        "$monitor_esp/EFI/BOOT/MONITORX64.EFI"
    install -m 0644 -- "$boot_loader" "$monitor_esp/EFI/BOOT/BOOTX64.EFI"
    if [[ -n "$monitor_image" ]]; then
        install -m 0644 -- "$monitor_image" "$monitor_esp/EFI/BOOT/MONITORX64.EFI"
    fi
    if [[ ${direct_mode:-qemu-research} == smp-uefi ]]; then
        install -m 0644 -- "$repo_root/scripts/x86_64/windows/cpu-probe.ps1" \
            "$monitor_esp/thin-hv-cpu-probe.ps1"
    fi
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
        rm -f -- "$hyperv_ready" "$wsl_ready"
    else
        die "partial Hyper-V state; remove together: $hyperv_disk $hyperv_vars $hyperv_tpm_dir $hyperv_ready $wsl_ready"
    fi
    mkdir -p -- "$hyperv_media"
    install -m 0644 -- "$source/hyperv-enable.ps1" "$hyperv_media/hyperv-enable.ps1"
    install -m 0644 -- "$source/hyperv-verify.ps1" "$hyperv_media/hyperv-verify.ps1"
}

prepare_wsl_media() (
    local daily_soak_run_id=${1:-}
    local source="$repo_root/scripts/x86_64/windows"
    local busybox=${BUSYBOX_STATIC:-} rootfs_work root description applet applets listing
    local external_url=${WINDOWS_DAILY_SOAK_EXTERNAL_URL:-}

    umask 022
    verify_wsl_msi
    need_command file
    need_command tar
    [[ -x "$busybox" ]] || die 'BUSYBOX_STATIC is not an executable file; run through nix develop'
    description=$(file -Lb -- "$busybox")
    [[ "$description" == *x86-64* && "$description" == *'statically linked'* ]] || \
        die "BUSYBOX_STATIC is not a static x86-64 executable: $description"
    applets=$("$busybox" --list)
    for applet in sh cat uname test; do
        grep -Fxq -- "$applet" <<<"$applets" || \
            die "BUSYBOX_STATIC lacks required applet: $applet"
    done
    for applet in dd rm sha256sum timeout wget; do
        grep -Fxq -- "$applet" <<<"$applets" || \
            die "BUSYBOX_STATIC lacks daily-soak applet: $applet"
    done

    rootfs_work=$(mktemp -d)
    root=$rootfs_work/root
    install -d -m 0755 -- "$root" "$root/bin" "$root/etc" "$root/proc" \
        "$root/sys" "$root/dev" "$root/root"
    install -d -m 1777 -- "$root/tmp"
    install -m 0755 -- "$busybox" "$root/bin/busybox"
    for applet in sh cat uname test; do
        ln -s busybox "$root/bin/$applet"
    done
    printf 'root:x:0:0:root:/root:/bin/sh\n' >"$root/etc/passwd"
    printf 'root:x:0:\n' >"$root/etc/group"
    printf '%s\n' '[boot]' 'systemd=false' '[automount]' 'enabled=false' \
        'mountFsTab=false' '[interop]' 'appendWindowsPath=false' \
        >"$root/etc/wsl.conf"
    tar --sort=name --mtime=@0 --owner=0 --group=0 --numeric-owner --format=gnu \
        -C "$root" -cf "$hyperv_media/thin-hv-wsl-rootfs.tar" .
    rm -rf -- "$rootfs_work"
    listing=$(tar -tf "$hyperv_media/thin-hv-wsl-rootfs.tar")
    grep -Fxq -- './bin/busybox' <<<"$listing" || die 'generated WSL rootfs is invalid'

    ln -f -- "$wsl_msi" "$hyperv_media/$(basename -- "$wsl_msi")"
    install -m 0644 -- "$source/wsl-enable.ps1" "$hyperv_media/wsl-enable.ps1"
    install -m 0644 -- "$source/wsl-verify.ps1" "$hyperv_media/wsl-verify.ps1"
    install -m 0644 -- "$source/wsl-soak.ps1" "$hyperv_media/wsl-soak.ps1"
    install -m 0644 -- "$source/hibernate-verify.ps1" "$hyperv_media/hibernate-verify.ps1"
    if [[ -n "$external_url" ]]; then
        [[ "$external_url" =~ ^https://[^[:space:]]+$ && \
            "$external_url" != *'?'* && "$external_url" != *'#'* && \
            "$external_url" != *'@'* ]] || \
            die 'WINDOWS_DAILY_SOAK_EXTERNAL_URL must be a public HTTPS URL without credentials, query, or fragment'
    fi
    printf '%s\n' "$external_url" >"$hyperv_media/daily-soak-external-url.txt"
    printf '%s\n' "$daily_soak_run_id" >"$hyperv_media/daily-soak-run-id.txt"
    sha256sum -- "$hyperv_media/thin-hv-wsl-rootfs.tar"
)

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
    local command=$1 submit_key=${2:-ret} destination=${3:-dialog} index key

    case "$destination" in
        dialog)
            printf 'sendkey esc 20\n' >&9
            sleep 0.1
            printf 'sendkey esc 20\n' >&9
            sleep 1
            printf 'sendkey meta_l-r 20\n' >&9
            sleep 3
            ;;
        console) ;;
        *) die 'keyboard destination must be dialog or console' ;;
    esac
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
            '+') key=shift-equal ;;
            '=') key=equal ;;
            [A-Z]) key="shift-${key,,}" ;;
        esac
        printf 'sendkey %s 20\n' "$key" >&9
        sleep 0.1
    done
    printf 'sendkey %s 20\n' "$submit_key" >&9
}

run_console_command() {
    local offset started
    # Encoded PowerShell commands exceed the Run dialog's command-length limit.
    # Open cmd first; its input path preserves the complete encoded payload.
    offset=$(stat -c %s -- "$desktop_serial_log")
    run_dialog_command 'cmd /k echo thinhvconsoleready>com2'
    started=$(host_uptime_seconds)
    while ! tail -c "+$((offset + 1))" -- "$desktop_serial_log" | \
        serial_has_exact_marker thinhvconsoleready; do
        (($(host_uptime_seconds) - started < 30)) || die 'Windows command console handshake failed'
        qemu_is_owned || die 'QEMU exited during command console handshake'
        sleep 1
    done
    run_dialog_command "$1" ret console
}

probe_windows_desktop() {
    # ponytail: the Run dialog is the desktop-ready probe; use a guest agent
    # only if later tests need general command execution inside Windows.
    run_dialog_command "cmd /c echo $desktop_marker>com2"
}

cpu_test_command() {
    local cpus=$1 bootstrap encoded
    case "$cpus" in 1|2|4|8) ;; *) die 'CPU probe requires 1, 2, 4 or 8 processors' ;; esac
    bootstrap='$p=@(Get-PSDrive -PSProvider FileSystem|ForEach-Object {Join-Path $_.Root "thin-hv-cpu-probe.ps1"}|Where-Object {Test-Path -LiteralPath $_ -PathType Leaf});if($p.Count -ne 1){exit 1};& $p[0] -ExpectedProcessors '
    encoded=$(printf '%s%s' "$bootstrap" "$cpus" | iconv -f UTF-8 -t UTF-16LE | base64 -w0)
    [[ "$encoded" =~ ^[A-Za-z0-9+/]+={0,2}$ ]] || die 'invalid encoded CPU probe command'
    printf 'powershell -nop -ep bypass -encodedcommand %s' "$encoded"
}

physical_test_command() {
    local bootstrap encoded

    # Select exactly one mounted test volume; never assume a drive letter.
    bootstrap='$p=@(Get-PSDrive -PSProvider FileSystem|ForEach-Object {Join-Path $_.Root "thin-hv-physical-status-test.ps1"}|Where-Object {Test-Path -LiteralPath $_ -PathType Leaf});if($p.Count -ne 1){exit 1};& $p[0]'
    encoded=$(printf '%s' "$bootstrap" | iconv -f UTF-8 -t UTF-16LE | base64 -w0)
    [[ "$encoded" =~ ^[A-Za-z0-9+/]+={0,2}$ ]] || die 'invalid encoded physical-status test command'
    printf 'powershell -nop -ep bypass -encodedcommand %s' "$encoded"
}

capture_physical_test_screen() {
    local screen=$1

    # QEMU monitor paths are quoted separately from the shell's path handling.
    screen=${screen//\\/\\\\}
    screen=${screen//\"/\\\"}
    printf 'screendump "%s"\n' "$screen" >&9
    sleep 0.5
}

probe_hyperv_enable() {
    run_dialog_command 'powershell -nop -ep bypass -f d:\hyperv-enable.ps1' ctrl-shift-ret
    sleep 5
    printf 'sendkey alt-y 20\n' >&9
}

probe_wsl_enable() {
    run_dialog_command 'powershell -nop -ep bypass -f d:\wsl-enable.ps1' ctrl-shift-ret
    sleep 5
    printf 'sendkey alt-y 20\n' >&9
}

probe_wsl_soak() {
    local minutes=$1 rounds=$2

    # The scheduled verifier emits its marker just before exiting. Let its
    # foreground PowerShell window close before opening the Run dialog.
    sleep 5
    run_dialog_command "powershell -nop -ep bypass -f d:\\wsl-soak.ps1 -minutes $minutes -rounds $rounds" ctrl-shift-ret
    sleep 5
    printf 'sendkey alt-y 20\n' >&9
}

probe_s4() {
    # Let the scheduled verifier's foreground window close before opening Run.
    sleep 5
    run_dialog_command 'powershell -nop -ep bypass -f d:\hibernate-verify.ps1 -reset' ctrl-shift-ret
    sleep 5
    printf 'sendkey alt-y 20\n' >&9
}

valid_poweroff_timeout() {
    [[ "$1" =~ ^[1-9][0-9]{0,3}$ ]] && ((10#$1 <= 1800))
}

configure_pci_profile() {
    # Sets the caller's local profile/array before any disk or firmware writes.
    pci_profile=${WINDOWS_PCI_PROFILE:-firmware-default}
    pci_args=()
    case "$pci_profile" in
        firmware-default) ;;
        q35-smoke-1g)
            # Explicit old QEMU A/B fixture, never an automatic fallback.
            pci_args=(-global q35-pcihost.pci-hole64-size=1G
                -fw_cfg name=opt/ovmf/X-PciMmio64Mb,string=1024) ;;
        *) die 'WINDOWS_PCI_PROFILE must be firmware-default or q35-smoke-1g' ;;
    esac
}

configure_direct_mode() {
    direct_mode=${WINDOWS_DIRECT_MODE:-qemu-research}
    case "$direct_mode" in
        qemu-research) ;;
        physical-uefi|smp-uefi)
            [[ "$1" == monitor || "$1" == monitor-hyperv || "$1" == print-direct-images ]] \
                || die 'WINDOWS_DIRECT_MODE requires a Direct monitor test'
            local image_kind=physical
            [[ "$direct_mode" != smp-uefi ]] || image_kind=smp
            loader="$repo_root/bin/x86_64/x86-uefi-$image_kind-direct-loader.efi"
            runtime_monitor="$repo_root/bin/x86_64/x86-uefi-$image_kind-direct-monitor.efi"
            ;;
        *) die 'WINDOWS_DIRECT_MODE must be qemu-research, physical-uefi or smp-uefi' ;;
    esac
}

# Project SMP requires its separate handoff image, never just a larger -smp.
# Keep WSL/S4/soak at their qualified two-CPU reference setting.
windows_smp() {
    local mode=$1 requested=$2 selected direct=${WINDOWS_DIRECT_MODE:-qemu-research}
    case "$mode" in
        monitor|monitor-hyperv)
            selected=${requested:-1}
            if [[ "$direct" == smp-uefi ]]; then
                case "$selected" in 1|2|4|8) ;; *) return 1 ;; esac
            else
                [[ "$selected" == 1 ]] || return 1
            fi ;;
        hyperv|trusted-kvm-hyperv)
            selected=${requested:-2}
            [[ "$selected" == 1 || "$selected" == 2 ]] || return 1 ;;
        wsl|wsl-s4|trusted-kvm-wsl|trusted-kvm-wsl-soak|trusted-kvm-s4)
            selected=${requested:-2}
            [[ "$selected" == 2 ]] || return 1 ;;
        install|boot|check-physical-status) selected=${requested:-2} ;;
        *) return 1 ;;
    esac
    [[ "$selected" =~ ^[1-9][0-9]*$ ]] || return 1
    printf '%s\n' "$selected"
}

# Diagnostic only: freeze the first reset, without editing guest BCD or firmware.
# This run can never produce a qualification PASS, including before that reset.
configure_reset_diagnostic() {
    reset_diagnostic=${WINDOWS_STOP_ON_RESET-0}
    reset_args=()
    case "$reset_diagnostic" in
        0) ;;
        1)
            [[ "$1" == monitor || "$1" == monitor-hyperv ]] || \
                die 'WINDOWS_STOP_ON_RESET requires a Direct monitor test'
            reset_args=(-no-reboot -no-shutdown) ;;
        *) die 'WINDOWS_STOP_ON_RESET must be 0 or 1' ;;
    esac
}

# QEMU HMP describes a reset held by -no-shutdown as "paused (shutdown)".
# Keep that distinct from a user pause; neither can satisfy a running gate.
direct_status_records() {
    tr -d '\r' | sed -n \
        -e 's/.*VM status: paused (shutdown)$/shutdown/p' \
        -e 's/.*VM status: \(running\|paused\)$/\1/p'
}

run_windows() {
    local mode=$1
    local s4_phase=${2:-}
    local timeout_seconds memory smp cpu disk_size ovmf_code ovmf_vars active_vars disable_s4=1
    local daily_soak_minutes=0 daily_soak_rounds=0 daily_soak_timeout daily_soak_run_id=''
    local disk_image=$disk disk_format=raw disk_snapshot=off
    local tpm_dir=$base_tpm_dir tpm_instance=base
    local qemu swtpm tpm_socket tpm_pid_file monitor_fifo serial_log desktop_serial_log qemu_log
    local expected_marker marker_log media_file wsl_media_stamp=''
    local qemu_pid='' qemu_status elapsed=0 monitor_fd_open=0 setup_probe_sent=0
    local boot_started
    local soak_probe_sent=0 soak_probe_elapsed=-1 soak_phase1_seen=0
    local soak_started_uptime=-1 soak_elapsed_seconds soak_completed_rounds soak_required_rounds
    local trusted_boot_count
    local marker_seen=0 wsl_failed=0 wsl_monitor_offset=-1 wsl_probe_offset=0
    local s4_probe_elapsed=-1
    local poweroff_timeout_seconds=${WINDOWS_POWEROFF_TIMEOUT_SECONDS:-120}
    local wsl_ready_matches=0
    local is_trusted=0 is_wsl=0 is_s4=0 is_daily_soak=0
    local backend_label='outer-kvm / reference'
    local is_physical_test=0 physical_state='' physical_started=0 physical_probe_started=-1
    local physical_desktop_probe_sent=0
    local is_direct=0 diagnostics_failure_captured=0
    local pci_profile reset_diagnostic
    local direct_mode loader=$loader runtime_monitor=$runtime_monitor monitor_esp=$monitor_esp
    local -a pci_args reset_args media_args network_args=(-netdev user,id=net0)

    configure_pci_profile
    configure_direct_mode "$mode"
    configure_reset_diagnostic "$mode"
    smp=$(windows_smp "$mode" "${WINDOWS_SMP-}") || die "WINDOWS_SMP is unsupported for $mode"
    valid_poweroff_timeout "$poweroff_timeout_seconds" || die 'poweroff timeout must be 1..1800 seconds'

    [[ "$mode" == check-physical-status ]] && is_physical_test=1

    if [[ "$mode" == trusted-kvm-hyperv || "$mode" == trusted-kvm-wsl || \
        "$mode" == trusted-kvm-wsl-soak || "$mode" == trusted-kvm-s4 ]]; then
        is_trusted=1
    fi
    if [[ "$mode" == wsl || "$mode" == wsl-s4 || "$mode" == trusted-kvm-wsl || \
        "$mode" == trusted-kvm-wsl-soak || "$mode" == trusted-kvm-s4 ]]; then
        is_wsl=1
    fi
    if [[ "$mode" == trusted-kvm-wsl-soak ]]; then
        is_daily_soak=1
        daily_soak_run_id=$(< /proc/sys/kernel/random/uuid)
        [[ "$daily_soak_run_id" =~ ^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$ ]] || \
            die 'failed to generate daily-soak run ID'
    fi
    if [[ "$mode" == wsl-s4 || "$mode" == trusted-kvm-s4 ]]; then
        is_s4=1
        disable_s4=0
        [[ "$s4_phase" == request || "$s4_phase" == resume ]] || \
            die "$mode phase must be request or resume"
    fi

    if ((is_trusted)) && [[ "$s4_phase" != resume ]]; then
        need_command cargo
        (cd -- "$repo_root" && cargo xbuild x86 --release)
    fi

    need_command qemu-system-x86_64
    need_command swtpm
    qemu=$(command -v qemu-system-x86_64)
    swtpm=$(command -v swtpm)
    memory=${WINDOWS_MEMORY:-4G}
    if ((is_trusted)); then
        cpu=${WINDOWS_CPU:-host,+vmx,-hypervisor,kvm=off}
    else
        cpu=${WINDOWS_CPU:-host,+vmx,-hypervisor}
    fi
    [[ -n "$cpu" ]] || die 'WINDOWS_CPU must not be empty'
    if [[ "$mode" == monitor || "$mode" == monitor-hyperv ]]; then
        is_direct=1
        backend_label='direct-vmx / project L0'
        [[ "$memory" == 4G ]] || die 'monitor mode currently requires WINDOWS_MEMORY=4G'
    elif [[ "$mode" == hyperv || "$mode" == wsl || "$mode" == wsl-s4 || \
        "$mode" == trusted-kvm-hyperv || \
        "$mode" == trusted-kvm-wsl || "$mode" == trusted-kvm-wsl-soak || \
        "$mode" == trusted-kvm-s4 ]]; then
        [[ "$memory" == 4G ]] || die 'Hyper-V control currently requires WINDOWS_MEMORY=4G'
    fi
    [[ "$memory" =~ ^[1-9][0-9]*[KMG]$ ]] || die 'WINDOWS_MEMORY must be a QEMU size such as 4G'
    printf 'Windows x86 test: backend=%s mode=%s l1_cpus=%s environment=QEMU/KVM (not physical hardware)\n' \
        "$backend_label" "$mode" "$smp"
    printf 'Windows x86 test: PCI profile=%s environment=QEMU (not physical hardware)\n' "$pci_profile"
    if ((is_direct)); then
        printf 'Windows x86 test: Direct mode=%s environment=QEMU (not physical hardware)\n' "$direct_mode"
        printf 'Windows x86 test: stop-on-reset=%s diagnostic-only=%s\n' "$reset_diagnostic" "$reset_diagnostic"
        if [[ "$direct_mode" == smp-uefi ]]; then
            need_command iconv
            need_command base64
        fi
    fi

    ovmf_code=$(first_file "${OVMF_FULL_CODE:-}") || die 'OVMF_FULL_CODE not found; run through nix develop'
    ovmf_vars=$(first_file "${OVMF_FULL_VARS:-}") || die 'OVMF_FULL_VARS not found; run through nix develop'
    mkdir -p -- "$work"
    need_command flock
    exec 8>"$work/windows-test.lock"
    flock -n 8 || die "another Windows test is using $work"
    if [[ -f "$hyperv_disk" && "$mode" != monitor && "$mode" != hyperv && \
        "$mode" != monitor-hyperv && "$mode" != trusted-kvm-hyperv && \
        "$mode" != trusted-kvm-wsl && "$mode" != trusted-kvm-wsl-soak && \
        "$mode" != trusted-kvm-s4 && \
        "$mode" != wsl && "$mode" != wsl-s4 && "$mode" != check-physical-status ]]; then
        # ponytail: keep the raw backing immutable instead of duplicating its
        # allocated blocks; remove all Hyper-V state before changing the base.
        die "Hyper-V overlay exists; remove its disk, vars, TPM, and ready marker together before changing the base"
    fi

    if ((is_physical_test)); then
        need_command iconv
        need_command base64
        [[ -f "$disk" && -f "$vars" && -d "$base_tpm_dir" ]] || \
            die 'physical-status self-test requires the existing QEMU evaluation disk, vars, and TPM'
        timeout_seconds=${WINDOWS_PHYSICAL_TEST_TIMEOUT_SECONDS:-600}
        disk_snapshot=on
        network_args=(-netdev user,id=net0,restrict=on)
        rm -f -- "$work/check-physical-status-command.ppm" "$work/check-physical-status-failure.ppm"
        physical_state=$(mktemp -d "$work/physical-status.XXXXXX")
        # This early trap also covers failures while preparing private test state.
        trap 'rm -rf -- "$physical_state"' EXIT
        mkdir -p -- "$physical_state/media" "$physical_state/tpm"
        install -m 0600 -- "$vars" "$physical_state/vars.fd"
        cp -a -- "$base_tpm_dir/." "$physical_state/tpm/"
        install -m 0644 -- "$repo_root/scripts/x86_64/windows/physical-status.ps1" \
            "$physical_state/media/physical-status.ps1"
        install -m 0644 -- "$repo_root/scripts/x86_64/windows/physical-status-test.ps1" \
            "$physical_state/media/thin-hv-physical-status-test.ps1"
        active_vars="$physical_state/vars.fd"
        tpm_dir="$physical_state/tpm"
        tpm_instance=physical-status
        media_args=(
            -drive "if=none,id=status-media,format=raw,readonly=on,file=fat:ro:$physical_state/media"
            -device "usb-storage,bus=xhci.0,drive=status-media,removable=on"
            -boot "menu=off"
        )
    elif [[ "$mode" == install ]]; then
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
        # ponytail: QEMU's temporary overlay keeps the raw backing immutable
        # while the persistent Hyper-V qcow2 exists.
        disk_snapshot=on
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
    elif ((is_wsl)); then
        need_command qemu-img
        prepare_hyperv_media
        [[ -f "$hyperv_ready" ]] || die "outer-kvm/reference Hyper-V PASS missing; run '$0 hyperv' first"
        prepare_wsl_media "$daily_soak_run_id"
        wsl_media_stamp=$(sha256sum \
            "$hyperv_media/wsl-enable.ps1" \
            "$hyperv_media/wsl-verify.ps1" \
            "$hyperv_media/wsl-soak.ps1" \
            "$hyperv_media/hyperv-verify.ps1" \
            "$hyperv_media/thin-hv-wsl-rootfs.tar" \
            "$wsl_msi" | cut -d' ' -f1 | sha256sum | cut -d' ' -f1)
        printf '%s\n' "$wsl_media_stamp" >"$hyperv_media/wsl-media-stamp.txt"
        if [[ -f "$wsl_ready" && "$(<"$wsl_ready")" == "$wsl_media_stamp" ]]; then
            wsl_ready_matches=1
        fi
        if ((is_s4 && wsl_ready_matches == 0)); then
            if ((is_trusted)); then
                die "WSL2 PASS missing for this media; run '$0 trusted-kvm-wsl' first"
            fi
            die "WSL2 PASS missing for this media; run '$0 wsl' first"
        fi
        if ((is_daily_soak)); then
            daily_soak_minutes=${WINDOWS_DAILY_SOAK_MINUTES:-60}
            daily_soak_rounds=${WINDOWS_DAILY_SOAK_ROUNDS:-2}
            [[ "$daily_soak_minutes" =~ ^[0-9]+$ && \
                "$daily_soak_rounds" =~ ^[0-9]+$ ]] || \
                die 'daily-soak minutes and rounds must be non-negative integers'
            ((daily_soak_minutes <= 10080 && daily_soak_rounds <= 10000 && \
                (daily_soak_minutes > 0 || daily_soak_rounds > 0))) || \
                die 'daily-soak target must be positive and within 10080 minutes/10000 rounds'
            daily_soak_timeout=$((daily_soak_minutes * 60 + daily_soak_rounds * 300 + 1800))
            timeout_seconds=${WINDOWS_DAILY_SOAK_TIMEOUT_SECONDS:-$daily_soak_timeout}
        else
            timeout_seconds=${WINDOWS_WSL_TIMEOUT_SECONDS:-1800}
        fi
        active_vars=$hyperv_vars
        disk_image=$hyperv_disk
        disk_format=qcow2
        tpm_dir=$hyperv_tpm_dir
        tpm_instance=hyperv
        if ((is_trusted)); then
            prepare_monitor_media "$trusted_kvm_loader" ''
            for media_file in "$hyperv_media"/*; do
                cp -fL --remove-destination --reflink=auto -- \
                    "$media_file" "$monitor_esp/${media_file##*/}"
            done
            media_args=(
                -drive "if=none,id=monitor-esp,format=raw,snapshot=on,file=fat:ro:$monitor_esp"
                -device "ide-hd,bus=ide.1,drive=monitor-esp,bootindex=1"
                -boot "menu=off,strict=on"
            )
        else
            media_args=(
                -drive "if=none,id=hyperv-media,format=raw,readonly=on,file=fat:$hyperv_media"
                -device "usb-storage,bus=xhci.0,drive=hyperv-media,removable=on"
                -boot "menu=off"
            )
        fi
    elif [[ "$mode" == monitor-hyperv ]]; then
        need_command qemu-img
        prepare_hyperv_media
        [[ -f "$hyperv_ready" ]] || die "outer-kvm/reference Hyper-V PASS missing; run '$0 hyperv' first"
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
    elif [[ "$mode" == trusted-kvm-hyperv ]]; then
        need_command qemu-img
        prepare_hyperv_media
        [[ -f "$hyperv_ready" ]] || die "outer-kvm/reference Hyper-V PASS missing; run '$0 hyperv' first"
        prepare_monitor_media "$trusted_kvm_loader" ''
        timeout_seconds=${WINDOWS_HYPERV_TIMEOUT_SECONDS:-600}
        active_vars=$hyperv_vars
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
    monitor_fifo="$work/qemu-monitor-$mode${s4_phase:+-$s4_phase}.in"
    serial_log="$work/$mode${s4_phase:+-$s4_phase}-serial.log"
    desktop_serial_log="$work/$mode${s4_phase:+-$s4_phase}-desktop-serial.log"
    qemu_log="$work/$mode${s4_phase:+-$s4_phase}-qemu.log"
    stop_pid_file "$tpm_pid_file"
    rm -f -- "$tpm_socket" "$tpm_pid_file" "$monitor_fifo"
    : >"$serial_log"
    : >"$desktop_serial_log"
    : >"$qemu_log"
    if ((is_physical_test)); then
        expected_marker=$physical_test_marker
        marker_log=$desktop_serial_log
    elif ((is_s4)) && [[ "$s4_phase" == resume ]]; then
        expected_marker=$s4_pass_marker
        marker_log=$desktop_serial_log
    elif ((is_daily_soak && wsl_ready_matches == 0)); then
        expected_marker="$wsl_marker stamp=$wsl_media_stamp"
        marker_log=$desktop_serial_log
    elif ((is_daily_soak)); then
        expected_marker="$daily_soak_marker stamp=$wsl_media_stamp run_id=$daily_soak_run_id target_minutes=$daily_soak_minutes target_rounds=$daily_soak_rounds"
        marker_log=$desktop_serial_log
    elif ((is_wsl)); then
        expected_marker="$wsl_marker stamp=$wsl_media_stamp"
        marker_log=$desktop_serial_log
    elif [[ "$mode" == hyperv || "$mode" == monitor-hyperv || \
        "$mode" == trusted-kvm-hyperv ]]; then
        expected_marker=$hyperv_marker
        marker_log=$desktop_serial_log
    elif [[ "$mode" == monitor ]]; then
        expected_marker=$desktop_marker
        if [[ "$direct_mode" == smp-uefi ]]; then
            expected_marker="thin-hv: windows SMP PASS cpus=$smp mask=$(((1 << smp) - 1))"
        fi
        marker_log=$desktop_serial_log
    else
        expected_marker=$marker
        marker_log=$serial_log
    fi

    qemu_is_owned() {
        [[ -n "$qemu_pid" ]] && kill -0 "$qemu_pid" 2>/dev/null || return 1
        if ((is_physical_test || is_direct)); then
            # Bind cleanup to this invocation's QEMU serial argument, not a reused PID.
            [[ -r "/proc/$qemu_pid/cmdline" ]] || return 1
            tr '\0' '\n' <"/proc/$qemu_pid/cmdline" | \
                grep -Fx -- "file:$serial_log" >/dev/null || return 1
        fi
    }

    direct_monitor_state() {
        local offset state attempt

        qemu_is_owned && ((monitor_fd_open)) || return 1
        offset=$(stat -c %s -- "$qemu_log") || return 1
        [[ "$offset" =~ ^[0-9]+$ ]] || return 1
        printf 'info status\n' >&9 || return 1
        for ((attempt = 0; attempt < 50; attempt++)); do
            qemu_is_owned || return 1
            state=$(tail -c "+$((offset + 1))" -- "$qemu_log" | direct_status_records) || return 1
            if [[ "$state" == running || "$state" == paused || "$state" == shutdown ]]; then
                printf '%s\n' "$state"
                return 0
            fi
            [[ -z "$state" ]] || return 1
            sleep 0.1
        done
        return 1
    }

    capture_direct_diagnostics() {
        local reason=$1 resume=$2 directory screen record json_file quoted_record
        local decoder="$repo_root/scripts/x86_64/decode-vmx-diagnostics.py"
        local initial_state='' stopped=0 must_resume=0 address='' extent='' bytes='' result=1
        local slot count=1 captured=0

        ((is_direct && monitor_fd_open)) && qemu_is_owned || return 1
        [[ "$reason" == failure || "$reason" == pre-success ]] || return 1
        [[ "$resume" == 0 || "$resume" == 1 ]] || return 1
        # Never allow a host path to add a second HMP command.
        [[ "$work" != *$'\n'* && "$work" != *$'\r'* ]] || return 1
        directory=$(mktemp -d "$work/$mode-$reason-diagnostics.XXXXXX") || return 1
        screen="$directory/screen.ppm"
        [[ "$direct_mode" != smp-uefi ]] || count=$smp
        initial_state=$(direct_monitor_state) || initial_state=
        if [[ "$initial_state" == running ]]; then
            must_resume=$resume
            if printf 'stop\n' >&9 && [[ $(direct_monitor_state) == paused ]]; then
                stopped=1
            fi
        elif [[ "$initial_state" == paused || "$initial_state" == shutdown ]]; then
            stopped=1
        fi

        # The screen remains useful when the monitor never reached publication.
        capture_physical_test_screen "$screen" || true
        if [[ -s "$screen" ]]; then
            printf 'Windows x86 test: direct diagnostics screen=%s reason=%s\n' "$screen" "$reason"
        fi
        if ((stopped)) && command -v python3 >/dev/null && [[ -f "$decoder" ]]; then
            for ((slot = 0; slot < count; slot++)); do
                record="$directory/counters.bin"
                json_file="$directory/counters.json"
                if [[ "$direct_mode" == smp-uefi ]]; then
                    record="$directory/cpu-$slot.bin"
                    json_file="$directory/cpu-$slot.json"
                fi
                extent=$(python3 "$decoder" --cpu "$slot" extent "$serial_log") || extent=
                read -r address bytes <<<"$extent"
                if [[ "$address" =~ ^0x[0-9a-f]{16}$ && ( "$bytes" == 176 || "$bytes" == 1216 || "$bytes" == 1344 || "$bytes" == 1408 || "$bytes" == 1472 ) ]]; then
                    quoted_record=${record//\\/\\\\}
                    quoted_record=${quoted_record//\"/\\\"}
                    # The decoder validated this CPU's unique owned publication.
                    # All CPUs are stopped; only compiled ABI extents are accepted.
                    if printf 'pmemsave %s %s "%s"\n' "$address" "$bytes" "$quoted_record" >&9 && \
                        [[ $(direct_monitor_state) == paused || $(direct_monitor_state) == shutdown ]] && \
                        python3 "$decoder" --cpu "$slot" decode "$serial_log" "$record" >"$json_file"; then
                        printf 'Windows x86 test: direct diagnostics counters=%s reason=%s\n' "$json_file" "$reason"
                        cat -- "$json_file"
                        ((captured += 1))
                    else
                        rm -f -- "$json_file"
                    fi
                fi
                # Keep only validated bounded JSON, never raw counter dumps.
                rm -f -- "$record"
            done
            ((captured == count)) && result=0
        fi
        if ((must_resume)); then
            if ! printf 'cont\n' >&9 || [[ $(direct_monitor_state) != running ]]; then
                printf 'Windows x86 test: direct diagnostics could not resume QEMU\n' >&2
                return 2
            fi
        fi
        if ((resume)) && [[ "$initial_state" != running ]]; then
            # Preserve an existing pause, but never turn it into functional PASS.
            printf 'Windows x86 test: QEMU was not running at direct pre-success capture\n' >&2
            return 2
        fi
        if ((result)); then
            printf 'Windows x86 test: direct diagnostics counters unavailable reason=%s\n' "$reason" >&2
        fi
        return "$result"
    }

    cleanup() {
        if qemu_is_owned; then
            if ((is_direct && monitor_fd_open && !diagnostics_failure_captured)); then
                diagnostics_failure_captured=1
                capture_direct_diagnostics failure 0 || true
            fi
            if ((is_physical_test && monitor_fd_open)); then
                capture_physical_test_screen "$work/check-physical-status-failure.ppm" || true
            fi
            # Capture may take several seconds; do not act on its old PID check.
            if qemu_is_owned; then
                kill "$qemu_pid" 2>/dev/null || true
                if ((is_physical_test || is_direct)); then
                    for _ in {1..20}; do
                        qemu_is_owned || break
                        sleep 0.1
                    done
                    qemu_is_owned && kill -KILL "$qemu_pid" 2>/dev/null || true
                fi
            fi
            wait "$qemu_pid" 2>/dev/null || true
        fi
        stop_pid_file "$tpm_pid_file"
        if ((monitor_fd_open)); then
            exec 9>&- 9<&-
        fi
        rm -f -- "$tpm_socket" "$tpm_pid_file" "$monitor_fifo"
        [[ -z "$physical_state" ]] || rm -rf -- "$physical_state"
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
        -global ICH9-LPC.disable_s3=1 \
        -global "ICH9-LPC.disable_s4=$disable_s4" \
        "${pci_args[@]}" \
        "${reset_args[@]}" \
        -cpu "$cpu" \
        -smp "cpus=$smp,sockets=1,cores=$smp,threads=1" \
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
        -drive "if=none,id=windisk,format=$disk_format,file=$disk_image,snapshot=$disk_snapshot,cache=writeback,discard=unmap,detect-zeroes=unmap" \
        -device ide-hd,bus=ide.0,drive=windisk,bootindex=2 \
        "${network_args[@]}" \
        -device e1000e,netdev=net0 \
        "${media_args[@]}" \
        <"$monitor_fifo" >"$qemu_log" 2>&1 &
    qemu_pid=$!
    boot_started=$(host_uptime_seconds)
    ((is_physical_test)) && physical_started=$(host_uptime_seconds)
    printf 'Windows x86 test: %s running as PID %s; VNC %s\n' \
        "$mode" "$qemu_pid" "${WINDOWS_VNC:-127.0.0.1:1}"
    if [[ "$mode" == install ]]; then
        # Microsoft's UEFI DVD loader waits for "Press any key" before Setup.
        for _ in {1..20}; do
            sleep 1
            printf 'sendkey spc\n' >&9
        done
    fi

    # Keyboard injection also consumes the boot deadline, not only sleep 2.
    while ((elapsed < timeout_seconds && $(host_uptime_seconds) - boot_started < timeout_seconds)); do
        if serial_has_exact_marker 'thin-hv: windows SMP FAIL' "$marker_log"; then
            die 'Windows per-CPU execution probe failed'
        fi
        if ((is_direct)); then
            if ((reset_diagnostic)) && [[ $(direct_monitor_state) == shutdown ]]; then
                die "diagnostic first reset/shutdown captured; not a qualification PASS"
            fi
            if serial_has_direct_failure "$serial_log"; then
                die "terminal Direct-VMX failure; logs: $serial_log $qemu_log"
            else
                (($? == 1)) || die "cannot read Direct-VMX failure log: $serial_log"
            fi
        fi
        if [[ "$mode" == monitor && "$direct_mode" == smp-uefi ]] && \
            ((setup_probe_sent == 0)) && serial_has_exact_marker "$desktop_marker" "$marker_log"; then
            # Wait for the desktop and command console, then start exactly once.
            # Repeated launches can steal focus or contend for COM2 mid-probe.
            run_console_command "$(cpu_test_command "$smp")"
            setup_probe_sent=1
        fi
        if ((is_physical_test)); then
            (($(host_uptime_seconds) - physical_started < timeout_seconds)) || break
            if grep -Fq -- "$physical_test_failure" "$marker_log"; then
                die 'physical-status self-test failed in disposable Windows'
            fi
            if ((physical_probe_started < 0 && elapsed >= 150)) && serial_has_exact_marker "$marker" "$serial_log"; then
                if ((physical_desktop_probe_sent == 0)); then
                    probe_windows_desktop
                    physical_desktop_probe_sent=1
                elif serial_has_exact_marker "$desktop_marker" "$marker_log"; then
                    physical_probe_started=$(host_uptime_seconds)
                    run_console_command "$(physical_test_command)"
                    sleep 3
                    capture_physical_test_screen "$work/check-physical-status-command.ppm"
                fi
            elif ((physical_probe_started >= 0 && $(host_uptime_seconds) - physical_probe_started >= 180)); then
                serial_has_exact_marker "$expected_marker" "$marker_log" || \
                    die 'physical-status self-test did not complete within 180 seconds'
            fi
        fi
        # Console startup/input can span the deadline within this iteration.
        (($(host_uptime_seconds) - boot_started < timeout_seconds)) || break
        marker_seen=0
        if ((is_daily_soak && wsl_ready_matches == 1)); then
            if ((soak_probe_sent)) && \
                tail -c "+$((wsl_probe_offset + 1))" -- "$marker_log" | \
                    serial_has_exact_marker "$expected_marker"; then
                marker_seen=1
            fi
        elif ((is_wsl && wsl_ready_matches == 0)); then
            if ((setup_probe_sent)) && \
                tail -c "+$((wsl_probe_offset + 1))" -- "$marker_log" | \
                    serial_has_exact_marker "$expected_marker"; then
                marker_seen=1
            fi
        elif serial_has_exact_marker "$expected_marker" "$marker_log"; then
            marker_seen=1
        fi
        if ((is_s4 && marker_seen && !setup_probe_sent)) && \
            [[ "$s4_phase" == request ]]; then
            printf 'Windows x86 test: observed %s; requesting S4\n' "$expected_marker"
            probe_s4
            setup_probe_sent=1
            s4_probe_elapsed=$elapsed
            expected_marker=$s4_request_marker
            marker_seen=0
        fi
        if ((is_daily_soak && marker_seen && wsl_ready_matches == 0)); then
            printf 'Windows x86 test: observed %s; starting daily soak\n' "$expected_marker"
            wsl_ready_matches=1
            wsl_monitor_offset=$(stat -c %s -- "$serial_log")
            wsl_probe_offset=$(stat -c %s -- "$marker_log")
            probe_wsl_soak "$daily_soak_minutes" "$daily_soak_rounds"
            soak_started_uptime=$(host_uptime_seconds)
            soak_probe_sent=1
            soak_probe_elapsed=$elapsed
            expected_marker="$daily_soak_marker stamp=$wsl_media_stamp run_id=$daily_soak_run_id target_minutes=$daily_soak_minutes target_rounds=$daily_soak_rounds"
            marker_seen=0
        fi
        wsl_failed=0
        if ((is_wsl && wsl_ready_matches == 1)) && \
            grep -Fq -- "$wsl_fail_marker" "$marker_log"; then
            wsl_failed=1
        elif ((is_wsl && setup_probe_sent)) && \
            tail -c "+$((wsl_probe_offset + 1))" -- "$marker_log" | \
                grep -F -- "$wsl_fail_marker" >/dev/null; then
            wsl_failed=1
        fi
        if grep -Fq -- "$s4_fail_marker" "$marker_log"; then
            tail -n 80 -- "$marker_log" >&2
            die "guest reported $s4_fail_marker; logs: $serial_log $marker_log $qemu_log"
        fi
        if ((is_s4)) && [[ "$s4_phase" == resume ]] && \
            grep -Fq -- "$wsl_marker stamp=$wsl_media_stamp" "$marker_log"; then
            die "normal WSL boot completed before S4 resume; logs: $serial_log $marker_log $qemu_log"
        fi
        if ((wsl_failed)); then
            tail -n 80 -- "$marker_log" >&2
            die "guest reported $wsl_fail_marker; logs: $serial_log $marker_log $qemu_log"
        fi
        if ((is_daily_soak && wsl_ready_matches == 1 && !soak_probe_sent)) && \
            grep -Fq -- "$wsl_marker stamp=$wsl_media_stamp" "$marker_log"; then
            printf 'Windows x86 test: current WSL media is ready; starting daily soak\n'
            wsl_monitor_offset=$(stat -c %s -- "$serial_log")
            wsl_probe_offset=$(stat -c %s -- "$marker_log")
            probe_wsl_soak "$daily_soak_minutes" "$daily_soak_rounds"
            soak_started_uptime=$(host_uptime_seconds)
            soak_probe_sent=1
            soak_probe_elapsed=$elapsed
        fi
        if ((is_daily_soak && soak_probe_sent && !soak_phase1_seen)); then
            if tail -c "+$((wsl_probe_offset + 1))" -- "$marker_log" | \
                grep -F -- 'thin-hv: windows daily soak phase=1 PASS' >/dev/null; then
                soak_phase1_seen=1
            elif ((elapsed >= soak_probe_elapsed + 180)); then
                die "daily-soak phase 1 did not start within 180 seconds; logs: $serial_log $marker_log $qemu_log"
            fi
        fi
        if ((marker_seen)); then
            printf 'Windows x86 test: backend=%s environment=QEMU/KVM observed %s\n' \
                "$backend_label" "$expected_marker"
            break
        fi
        if ((is_s4 && setup_probe_sent && s4_probe_elapsed >= 0 && \
            elapsed >= s4_probe_elapsed + 120)) && [[ "$s4_phase" == request ]]; then
            die "guest did not request S4 within 120 seconds; logs: $serial_log $marker_log $qemu_log"
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
        if [[ "$mode" == monitor ]] && ((setup_probe_sent == 0 && elapsed >= 150 && elapsed % 30 == 0)); then
            probe_windows_desktop
        fi
        if [[ "$mode" == hyperv ]] && ((elapsed >= 150 && !setup_probe_sent)); then
            probe_hyperv_enable
            setup_probe_sent=1
        fi
        if ((is_wsl && wsl_ready_matches == 0 && elapsed >= 150 && !setup_probe_sent)); then
            wsl_monitor_offset=$(stat -c %s -- "$serial_log")
            wsl_probe_offset=$(stat -c %s -- "$marker_log")
            probe_wsl_enable
            setup_probe_sent=1
        fi
        if ((is_daily_soak && wsl_ready_matches == 1 && elapsed >= 150 && \
            !soak_probe_sent)); then
            wsl_monitor_offset=$(stat -c %s -- "$serial_log")
            wsl_probe_offset=$(stat -c %s -- "$marker_log")
            probe_wsl_soak "$daily_soak_minutes" "$daily_soak_rounds"
            soak_started_uptime=$(host_uptime_seconds)
            soak_probe_sent=1
            soak_probe_elapsed=$elapsed
        fi
        if ((elapsed % 30 == 0)); then
            printf 'Windows x86 test: waiting for marker (%ss/%ss)\n' \
                "$(($(host_uptime_seconds) - boot_started))" "$timeout_seconds"
        fi
    done
    ((marker_seen)) || \
        die "marker timeout; logs: $serial_log $marker_log $qemu_log"
    ((reset_diagnostic == 0)) || die 'diagnostic run reached its marker; not a qualification PASS'
    if ((is_physical_test)); then
        serial_has_exact_marker "$physical_test_json" "$marker_log" || die 'physical-status exact JSON missing'
    fi
    if ((is_daily_soak)); then
        soak_elapsed_seconds=$(($(host_uptime_seconds) - soak_started_uptime))
        ((soak_started_uptime >= 0 && soak_elapsed_seconds >= daily_soak_minutes * 60)) || \
            die "daily-soak PASS arrived after ${soak_elapsed_seconds}s; requested ${daily_soak_minutes}m"
        soak_completed_rounds=$(tail -c "+$((wsl_probe_offset + 1))" -- "$marker_log" | \
            sed -n 's/^thin-hv: windows daily soak rounds=\([0-9][0-9]*\) target_minutes=[0-9][0-9]* elapsed_ms=[0-9][0-9]*$/\1/p')
        [[ "$soak_completed_rounds" =~ ^[0-9]+$ ]] || \
            die "daily-soak actual round count is missing or ambiguous"
        soak_required_rounds=$daily_soak_rounds
        ((soak_required_rounds >= 2)) || soak_required_rounds=2
        ((soak_completed_rounds >= soak_required_rounds)) || \
            die "daily-soak completed $soak_completed_rounds rounds; required $soak_required_rounds"
        trusted_boot_count=$(tail -c "+$((wsl_monitor_offset + 1))" -- "$serial_log" | \
            grep -Fc -- "$trusted_marker" || true)
        ((trusted_boot_count == 1)) || \
            die "daily-soak observed $trusted_boot_count trusted reboots; expected exactly 1"
    fi
    if [[ "$mode" == hyperv ]]; then
        : >"$hyperv_ready"
    elif ((is_wsl)); then
        printf '%s\n' "$wsl_media_stamp" >"$wsl_ready"
    fi

    if [[ "$mode" == monitor || "$mode" == monitor-hyperv ]]; then
        bash "$repo_root/scripts/x86_64/run-uefi-smoke.sh" \
            --check-backend-log direct-vmx "$serial_log" || die 'Direct-VMX backend provenance failed'
        bash "$repo_root/scripts/x86_64/run-uefi-smoke.sh" \
            --check-direct-mode-log "$direct_mode" "$serial_log" || die 'Direct mode/overlay provenance failed'
        bash "$repo_root/scripts/x86_64/run-uefi-smoke.sh" \
            --check-nested-mode-log "${THIN_HV_NESTED_MODE:-current}" "$serial_log" || die 'Direct nested mode provenance failed'
        local high_pci_required=0
        [[ "$pci_profile" != firmware-default ]] || high_pci_required=1
        bash "$repo_root/scripts/x86_64/run-uefi-smoke.sh" \
            --check-direct-platform-log "$high_pci_required" "$serial_log" "$smp" || die 'Direct platform/high-PCI map evidence failed'
        if [[ "$direct_mode" == smp-uefi ]]; then
            serial_has_exact_marker "thin-hv: firmware handoff PASS exit_boot_services=success cpus=$smp ap_takeover=$((smp - 1))" "$serial_log" || \
                die 'all-CPU carrier handoff barrier evidence missing'
        fi
        grep -Fq -- 'thin-hv: runtime monitor active' "$serial_log" || \
            die "monitor marker missing from $serial_log"
        [[ $(direct_monitor_state) == running ]] || die 'QEMU is not running before direct success validation'
        if capture_direct_diagnostics pre-success 1; then
            :
        else
            local diagnostics_status=$?
            [[ "$direct_mode" != smp-uefi && "$diagnostics_status" != 2 ]] || \
                die 'Direct per-CPU diagnostics missing or QEMU not running after capture'
        fi
    elif ((is_trusted)); then
        if ((wsl_monitor_offset >= 0)); then
            tail -c "+$((wsl_monitor_offset + 1))" -- "$serial_log" | \
                grep -F -- "$trusted_marker" >/dev/null || \
                die "trusted outer KVM marker missing after WSL reboot in $serial_log"
        else
            grep -Fq -- "$trusted_marker" "$serial_log" || \
                die "trusted outer KVM marker missing from $serial_log"
        fi
        if grep -Fq -- 'thin-hv: L1 ' "$serial_log"; then
            die "direct nested VMX unexpectedly active in $serial_log"
        fi
    fi

    if ((is_s4)) && [[ "$s4_phase" == request ]]; then
        for _ in {1..120}; do
            kill -0 "$qemu_pid" 2>/dev/null || break
            if grep -Fq -- "$s4_fail_marker" "$marker_log"; then
                tail -n 80 -- "$marker_log" >&2
                die "guest reported $s4_fail_marker; logs: $serial_log $marker_log $qemu_log"
            fi
            sleep 1
        done
        kill -0 "$qemu_pid" 2>/dev/null && \
            die "QEMU did not exit after S4 request; logs: $serial_log $marker_log $qemu_log"
        set +e
        wait "$qemu_pid"
        qemu_status=$?
        set -e
        qemu_pid=
        if grep -Fq -- "$s4_fail_marker" "$marker_log"; then
            tail -n 80 -- "$marker_log" >&2
            die "guest reported $s4_fail_marker during S4 poweroff; logs: $serial_log $marker_log $qemu_log"
        fi
        ((qemu_status == 0)) || die "QEMU S4 exit status $qemu_status; log: $qemu_log"
        printf 'Windows x86 test: S4 powered off cleanly; cold restarting\n'
        cleanup
        trap - EXIT INT TERM
        return
    fi

    # These verifiers request S5 themselves; an ACPI button can re-hibernate.
    if ((!is_s4 && !is_physical_test && !is_daily_soak)); then
        printf 'system_powerdown\n' >&9
    fi
    printf 'Windows x86 test: waiting for final poweroff timeout_seconds=%s\n' "$poweroff_timeout_seconds"
    for ((elapsed = 0; elapsed < poweroff_timeout_seconds; elapsed++)); do
        kill -0 "$qemu_pid" 2>/dev/null || break
        sleep 1
    done
    if kill -0 "$qemu_pid" 2>/dev/null; then
        if ((is_daily_soak || is_s4 || is_physical_test)); then
            die "Windows did not shut down within $poweroff_timeout_seconds seconds after $mode PASS"
        fi
        printf 'quit\n' >&9
    fi
    set +e
    wait "$qemu_pid"
    qemu_status=$?
    set -e
    qemu_pid=
    if ((is_direct)); then
        ((qemu_status == 0)) || die "Direct-VMX QEMU exit status $qemu_status after guest marker"
        bash "$repo_root/scripts/x86_64/run-uefi-smoke.sh" \
            --check-backend-log direct-vmx "$serial_log" || die 'Direct-VMX backend/fatal gate failed after QEMU exit'
    fi
    if ((is_physical_test)); then
        ! grep -Fq -- "$physical_test_failure" "$marker_log" || die 'physical-status late failure'
        ((qemu_status == 0)) || die "physical-status QEMU exit status $qemu_status"
        printf 'Windows x86 test: physical-status SelfTest PASS environment=QEMU/KVM hardware_queries=0\n'
    fi
    if ((is_daily_soak || is_s4)); then
        for fail_marker in "$wsl_fail_marker" "$daily_soak_fail_marker" "$s4_fail_marker"; do
            if grep -Fq -- "$fail_marker" "$marker_log"; then
                tail -n 80 -- "$marker_log" >&2
                die "guest reported $fail_marker after PASS; logs: $serial_log $marker_log $qemu_log"
            fi
        done
        ((qemu_status == 0)) || die "QEMU exited with status $qemu_status after $mode PASS"
        qemu-img check "$disk_image" >/dev/null || die "$mode disk check failed: $disk_image"
    fi
    tail -n 40 -- "$serial_log"
    cleanup
    trap - EXIT INT TERM
}

run_windows_s4() {
    local mode=$1

    run_windows "$mode" request
    sleep 1
    run_windows "$mode" resume
}

check_wsl_soak() {
    local source="$repo_root/scripts/x86_64/windows"
    local verifier="$source/wsl-verify.ps1" hibernate="$source/hibernate-verify.ps1"
    local launcher="$source/wsl-soak.ps1" needle

    bash -n "$0"
    for needle in \
        '[int]$DailySoakMinutes = 0' \
        '[int]$DailySoakRounds = 0' \
        'Restart-Computer -Force' \
        'phase-1 disk hash did not survive reboot' \
        'daily soak media stamp changed during resume' \
        'DailySoakRunId' \
        '& shutdown.exe /s /t 0 /f' \
        'daily soak shutdown request failed' \
        'run_id=' \
        'daily soak PASS stamp=$Stamp run_id=$RunId target_minutes=$Minutes target_rounds=$Rounds' \
        'IncrementalHash' \
        '$diskHash -eq $expectedDiskHash' \
        "\$mediaRunIdFile = 'D:\\daily-soak-run-id.txt'" \
        'daily soak media run ID is unavailable' \
        'daily soak media run ID is invalid' \
        '$savedRunId -ne $mediaRunId' \
        'Remove-Item -LiteralPath $phaseFile -Force' \
        '3b6a07d0d404fab4e23b6d34bc6696a6a312dd92821332385e5af7c01c421351' \
        'timeout -s KILL 45' \
        'NoMatchingEventsFound' \
        'Microsoft-Windows-WHEA-Logger' \
        'Microsoft-Windows-Hyper-V-Hypervisor-Admin' \
        'memory_sha256' 'disk_sha256' 'tcp_bytes' \
        'wsl_cpu_sha256' 'wsl_memory_sha256' 'external_sha256' \
        'phaseTwoExternalHash'; do
        grep -Fq -- "$needle" "$verifier" || die "daily-soak verifier check missing: $needle"
    done
    for needle in \
        'set -eu; uname -r' \
        'function Get-WinEventsOrEmpty' \
        'NoMatchingEventsFound' \
        'Level = @(1, 2, 3)' \
        '& shutdown.exe /s /t 0 /f'; do
        grep -Fq -- "$needle" "$hibernate" || die "S4 verifier check missing: $needle"
    done
    for needle in \
        'Windows did not shut down within $poweroff_timeout_seconds seconds after $mode PASS' \
        'QEMU exited with status $qemu_status after $mode PASS' \
        'qemu-img check "$disk_image"' \
        'cargo xbuild x86 --release' \
        'host_uptime_seconds' \
        'trusted_boot_count == 1' \
        'soak_completed_rounds >= soak_required_rounds' \
        'target_minutes=$daily_soak_minutes target_rounds=$daily_soak_rounds' \
        'serial_has_exact_marker "$expected_marker"' \
        'during S4 poweroff' \
        'guest reported $fail_marker after PASS'; do
        grep -Fq -- "$needle" "$0" || die "daily-soak/S4 exit check missing: $needle"
    done
    grep -Fq -- 'daily-soak phase 1 did not start within 180 seconds' "$0" || \
        die 'daily-soak launch gate is missing'
    grep -Fxq -- '    if ((!is_s4 && !is_physical_test && !is_daily_soak)); then' "$0" || \
        die 'daily-soak must not receive an additional ACPI power-button request'
    grep -Fq -- '[int]$Minutes = 60' "$launcher" || die 'daily-soak launcher default is not 60 minutes'
    grep -Fq -- '[int]$Rounds = 2' "$launcher" || die 'daily-soak launcher lacks two-boot rounds'
    printf 'Windows x86 test: daily-soak/S4 static checks PASS\n'
}

usage() {
    printf 'usage: %s download|verify|download-wsl|verify-wsl|check-wsl-soak|install|boot|monitor|hyperv|wsl|wsl-s4|monitor-hyperv|trusted-kvm-hyperv|trusted-kvm-wsl|trusted-kvm-wsl-soak|trusted-kvm-s4\n' "$0"
    printf '       %s check-physical-status (disposable QEMU eval SelfTest only)\n' "$0"
    printf '       WINDOWS_PCI_PROFILE=firmware-default (default) or q35-smoke-1g (explicit QEMU A/B fixture)\n'
    printf '       WINDOWS_DIRECT_MODE=qemu-research (default), physical-uefi, or smp-uefi (no-overlay same-ESP QEMU fixtures)\n'
    printf '       WINDOWS_SMP=1/2/4/8 for explicit smp-uefi; other Direct requires 1, Hyper-V reference 1/2, WSL/S4/soak 2\n'
    printf '       WINDOWS_STOP_ON_RESET=1 freezes the first Direct reset/shutdown for diagnostics, never qualification PASS\n'
}

case ${1:-} in
    download) download_iso ;;
    verify) verify_iso ;;
    download-wsl) download_wsl_msi ;;
    verify-wsl) verify_wsl_msi ;;
    check-wsl-soak) check_wsl_soak ;;
    check-physical-status) run_windows check-physical-status ;;
    physical-test-command) physical_test_command ;;
    cpu-test-command)
        (($# == 2)) || die 'cpu-test-command requires CPU_COUNT'
        cpu_test_command "$2"
        ;;
    check-direct-failure)
        (($# == 1 || $# == 2)) || die 'check-direct-failure takes an optional LOG'
        shift
        serial_has_direct_failure "$@"
        ;;
    print-pci-args)
        (($# == 1)) || die 'print-pci-args takes no arguments'
        configure_pci_profile
        if ((${#pci_args[@]})); then printf '%s\0' "${pci_args[@]}"; fi
        ;;
    print-direct-images)
        (($# == 1)) || die 'print-direct-images takes no arguments'
        configure_direct_mode "$1"
        printf '%s\0%s\0' "$loader" "$runtime_monitor"
        ;;
    print-reset-args)
        (($# == 2)) || die 'print-reset-args requires MODE'
        configure_reset_diagnostic "$2"
        if ((${#reset_args[@]})); then printf '%s\0' "${reset_args[@]}"; fi
        ;;
    check-direct-status)
        (($# == 1)) || die 'check-direct-status reads HMP records from stdin'
        direct_status_records
        ;;
    check-poweroff-timeout)
        (($# == 2)) || die 'check-poweroff-timeout requires SECONDS'
        valid_poweroff_timeout "$2"
        ;;
    print-smp)
        (($# == 2)) || die 'print-smp requires MODE'
        windows_smp "$2" "${WINDOWS_SMP-}"
        ;;
    check-serial-marker)
        (($# == 2 || $# == 3)) || die 'check-serial-marker requires MARKER and optional LOG'
        shift
        serial_has_exact_marker "$@"
        ;;
    install) run_windows install ;;
    boot) run_windows boot ;;
    monitor) run_windows monitor ;;
    hyperv) run_windows hyperv ;;
    wsl) run_windows wsl ;;
    wsl-s4) run_windows_s4 wsl-s4 ;;
    monitor-hyperv) run_windows monitor-hyperv ;;
    trusted-kvm-hyperv) run_windows trusted-kvm-hyperv ;;
    trusted-kvm-wsl) run_windows trusted-kvm-wsl ;;
    trusted-kvm-wsl-soak) run_windows trusted-kvm-wsl-soak ;;
    trusted-kvm-s4) run_windows_s4 trusted-kvm-s4 ;;
    -h | --help | help) usage ;;
    *) usage >&2; exit 2 ;;
esac
