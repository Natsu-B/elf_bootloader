#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
output=${1:-${LINUX_UKI_OUTPUT:-"$repo_root/bin/x86_64/linux-l1.efi"}}
kernel_override=${2:-${LINUX_KERNEL:-}}
busybox_override=${3:-${BUSYBOX_STATIC:-}}
stub_override=${4:-${LINUX_EFI_STUB:-}}
cmdline=${LINUX_L1_CMDLINE:-'console=ttyS0,115200n8 earlycon=uart8250,io,0x3f8,115200n8 rdinit=/init maxcpus=1 panic=-1'}
read -r -a extra_modules <<<"${LINUX_L1_EXTRA_MODULES:-}"

die() {
    printf 'linux L1 UKI: %s\n' "$*" >&2
    exit 1
}

if (( $# > 4 )); then
    die "usage: $0 [output [kernel [static-busybox [linuxx64.efi.stub]]]]"
fi

first_file() {
    local candidate
    for candidate in "$@"; do
        if [[ -n "$candidate" && -f "$candidate" && -r "$candidate" ]]; then
            printf '%s\n' "$candidate"
            return 0
        fi
    done
    return 1
}

first_command() {
    local candidate resolved
    for candidate in "$@"; do
        [[ -n "$candidate" ]] || continue
        if [[ "$candidate" == */* ]]; then
            [[ -x "$candidate" ]] && printf '%s\n' "$candidate" && return 0
        elif resolved=$(command -v -- "$candidate" 2>/dev/null); then
            printf '%s\n' "$resolved"
            return 0
        fi
    done
    return 1
}

file_cmd=$(first_command "${FILE:-}" file) || die 'file not found; set FILE'
objdump=$(first_command "${OBJDUMP:-}" objdump llvm-objdump) || die 'objdump not found; set OBJDUMP'

valid_selftest() {
    local candidate=$1 description headers bytes
    # llvm-objdump does not accept "--"; make every input an absolute operand.
    [[ "$candidate" == /* ]] || candidate="$PWD/$candidate"
    [[ -f "$candidate" && -r "$candidate" && -x "$candidate" ]] || return 1
    bytes=$(wc -c <"$candidate") || return 1
    [[ "$bytes" =~ ^[0-9]+$ ]] && ((bytes > 0 && bytes <= 67108864)) || return 1
    description=$(LC_ALL=C "$file_cmd" -Lb -- "$candidate") || return 1
    [[ "$description" == 'ELF 64-bit LSB executable, x86-64,'* &&
       "$description" == *'statically linked'* ]] || return 1
    headers=$("$objdump" -p "$candidate") || return 1
    ! grep -Eq '^[[:space:]]*(INTERP|DYNAMIC)[[:space:]]' <<<"$headers"
}

if [[ ${1:-} == --check-selftest-elf ]]; then
    [[ $# == 2 ]] || die 'usage: --check-selftest-elf ELF'
    valid_selftest "$2" || die 'selftest must be a bounded static non-PIE x86-64 ELF executable'
    exit 0
fi

objcopy=$(first_command "${OBJCOPY:-}" objcopy llvm-objcopy) || die 'objcopy not found; set OBJCOPY'
cpio=$(first_command "${CPIO:-}" cpio) || die 'cpio not found; set CPIO'
gzip=$(first_command "${GZIP:-}" gzip) || die 'gzip not found; set GZIP'
modprobe=$(first_command "${MODPROBE:-}" modprobe) || die 'modprobe not found; set MODPROBE'
depmod=$(first_command "${DEPMOD:-}" depmod) || die 'depmod not found; set DEPMOD'
cc=$(first_command "${CC:-}" cc gcc clang) || die 'C compiler not found; set CC'

valid_busybox() {
    local candidate=$1 applet applets description
    [[ -f "$candidate" && -x "$candidate" ]] || return 1
    description=$($file_cmd -Lb -- "$candidate")
    [[ "$description" == *x86-64* && "$description" == *'statically linked'* ]] || return 1
    applets=$($candidate --list 2>/dev/null) || return 1
    for applet in sh mount grep modprobe setsid cttyhack; do
        grep -Fxq -- "$applet" <<<"$applets" || return 1
    done
}

find_busybox() {
    local candidate store
    for candidate in "$busybox_override" "$(command -v busybox 2>/dev/null || true)"; do
        if valid_busybox "$candidate"; then
            printf '%s\n' "$candidate"
            return 0
        fi
    done
    while IFS= read -r candidate; do
        if valid_busybox "$candidate"; then
            printf '%s\n' "$candidate"
            return 0
        fi
    done < <(find /nix/store -maxdepth 3 -type f -path '*/busybox-static-*/bin/busybox' -print 2>/dev/null | sort)
    if command -v nix >/dev/null; then
        store=$(nix build --no-link --print-out-paths nixpkgs#pkgsStatic.busybox | tail -n 1)
        candidate=$store/bin/busybox
        if valid_busybox "$candidate"; then
            printf '%s\n' "$candidate"
            return 0
        fi
    fi
    return 1
}

kernel=$(first_file "$kernel_override" /run/current-system/kernel) || die 'x86 bzImage not found; pass arg 2 or set LINUX_KERNEL'
stub=$(first_file "$stub_override" /run/current-system/sw/lib/systemd/boot/efi/linuxx64.efi.stub \
    "$(find /nix/store -maxdepth 6 -type f -path '*/lib/systemd/boot/efi/linuxx64.efi.stub' -print -quit 2>/dev/null)") \
    || die 'systemd linuxx64.efi.stub not found; pass arg 4 or set LINUX_EFI_STUB'
busybox=$(find_busybox) || die 'full static x86-64 BusyBox not found; pass arg 3 or set BUSYBOX_STATIC'
init_source=$(first_file "${LINUX_L1_INIT:-}" "$repo_root/scripts/x86_64/linux-l1-init") \
    || die 'init source not found; set LINUX_L1_INIT'
kvm_probe_source=$(first_file "${LINUX_L1_KVM_PROBE:-}" "$repo_root/scripts/x86_64/linux-l1-kvm-probe.c") \
    || die 'KVM probe source not found; set LINUX_L1_KVM_PROBE'
selftest=${LINUX_L1_KVM_SELFTEST:-}
if [[ -n "$selftest" ]]; then
    valid_selftest "$selftest" || die 'selftest must be a bounded static non-PIE x86-64 ELF executable'
    for applet in awk timeout wc cat mkdir poweroff sleep; do
        "$busybox" --list | grep -Fxq -- "$applet" || die "static BusyBox lacks $applet"
    done
fi
kernel_release=${LINUX_KERNEL_RELEASE:-$(uname -r)}
modules_prefix=${LINUX_MODULES_PREFIX:-/run/current-system/kernel-modules}
modules_root=$modules_prefix/lib/modules/$kernel_release

[[ "$($file_cmd -Lb -- "$kernel")" == *'Linux kernel x86 boot executable'* ]] || die "not an x86 bzImage: $kernel"
[[ "$($file_cmd -Lb -- "$stub")" == *'PE32+ executable (EFI application) x86-64'* ]] || die "not an x86-64 EFI stub: $stub"
[[ -n "$cmdline" && "$cmdline" != *$'\n'* ]] || die 'LINUX_L1_CMDLINE must be one non-empty line'
[[ -r "$modules_root/modules.dep" ]] || die "kernel modules not found: $modules_root"
for input in "$kernel" "$stub" "$busybox" "$init_source" "$kvm_probe_source" "$selftest"; do
    [[ ! -e "$output" || ! "$output" -ef "$input" ]] || die "output would overwrite input: $input"
done

work=$(mktemp -d)
trap 'rm -rf -- "$work"' EXIT
root=$work/root
mkdir -p -- "$root/bin" "$root/dev" "$root/proc" "$root/sys" "$(dirname -- "$output")"
install -m 0755 -- "$busybox" "$root/bin/busybox"
for applet in sh mount grep modprobe setsid cttyhack; do
    ln -s busybox "$root/bin/$applet"
done
install -m 0755 -- "$init_source" "$root/init"
if [[ -n "$selftest" ]]; then
    install -m 0755 -- "$selftest" "$root/bin/kvm-selftest"
fi
"$cc" -Os -Wall -Wextra -Werror -ffreestanding -fno-pie -fno-stack-protector \
    -fno-asynchronous-unwind-tables -fno-unwind-tables -nostdlib -static -no-pie -s \
    -Wl,--build-id=none,-e,_start "$kvm_probe_source" -o "$root/bin/kvm-probe"
[[ "$($file_cmd -Lb -- "$root/bin/kvm-probe")" == *x86-64*static* ]] \
    || die 'KVM probe is not a static x86-64 executable'

for requested_module in kvm_intel efivarfs "${extra_modules[@]}"; do
    [[ "$requested_module" =~ ^[a-zA-Z0-9_-]+$ ]] || \
        die "invalid module name in LINUX_L1_EXTRA_MODULES: $requested_module"
    module_deps=$("$modprobe" -d "$modules_prefix" -S "$kernel_release" \
        --show-depends "$requested_module") || die "module not found: $requested_module"
    while read -r action module_path _; do
        [[ "$action" == insmod ]] || continue
        [[ "$module_path" == "$modules_root/"* ]] || \
            die "module outside $modules_root: $module_path"
        install -Dm 0644 -- "$module_path" \
            "$root/lib/modules/$kernel_release/${module_path#"$modules_root/"}"
    done <<<"$module_deps"
done
find "$root/lib/modules/$kernel_release" -name 'kvm-intel.ko*' -print -quit | grep -q . \
    || die "kvm-intel module not found for $kernel_release"
find "$root/lib/modules/$kernel_release" -name 'efivarfs.ko*' -print -quit | grep -q . \
    || die "efivarfs module not found for $kernel_release"
install -m 0644 -- "$modules_root"/modules.{order,builtin,builtin.modinfo} "$root/lib/modules/$kernel_release/"
"$depmod" -b "$root" "$kernel_release"
find "$root" -exec touch -h -d '@0' -- {} +

(
    cd -- "$root"
    find . -print0 | sort -z | "$cpio" --null --create --format=newc --owner=0:0 --reproducible 2>/dev/null
) | "$gzip" -9n >"$work/initrd.cpio.gz"

printf '%s' "$cmdline" >"$work/cmdline"
printf 'ID=thin-hv\nNAME="thin-hv Linux L1"\nVERSION_ID=1\n' >"$work/os-release"

image_base_hex=
image_size_hex=
while read -r key value _; do
    case $key in
        ImageBase) image_base_hex=$value ;;
        SizeOfImage) image_size_hex=$value ;;
    esac
done < <("$objdump" -p "$stub")
[[ "$image_base_hex" =~ ^[0-9a-fA-F]+$ && "$image_size_hex" =~ ^[0-9a-fA-F]+$ ]] \
    || die "could not read PE image layout from $stub"
image_base=$((16#$image_base_hex))
next_rva=$((((16#$image_size_hex + 0xfff) / 0x1000) * 0x1000))
osrel_vma=$(printf '0x%x' "$((image_base + next_rva))")
cmdline_vma=$(printf '0x%x' "$((image_base + next_rva + 0x1000))")
linux_vma=$(printf '0x%x' "$((image_base + next_rva + 0x2000))")
kernel_size=$(stat -Lc %s -- "$kernel")
initrd_rva=$((((next_rva + 0x2000 + kernel_size + 0xfff) / 0x1000) * 0x1000))
initrd_vma=$(printf '0x%x' "$((image_base + initrd_rva))")

"$objcopy" \
    --add-section .osrel="$work/os-release" --change-section-vma .osrel="$osrel_vma" \
    --add-section .cmdline="$work/cmdline" --change-section-vma .cmdline="$cmdline_vma" \
    --add-section .linux="$kernel" --change-section-vma .linux="$linux_vma" \
    --add-section .initrd="$work/initrd.cpio.gz" --change-section-vma .initrd="$initrd_vma" \
    --set-section-flags .osrel=contents,alloc,load,readonly,data \
    --set-section-flags .cmdline=contents,alloc,load,readonly,data \
    --set-section-flags .linux=contents,alloc,load,readonly,data \
    --set-section-flags .initrd=contents,alloc,load,readonly,data \
    "$stub" "$work/linux-l1.efi"

install -m 0644 -- "$work/linux-l1.efi" "$output"
description=$($file_cmd -Lb -- "$output")
[[ "$description" == *'PE32+ executable (EFI application) x86-64'* ]] || die "generated artifact is not an x86-64 EFI application: $description"

printf 'linux L1 UKI: artifact=%s\n' "$output"
printf 'linux L1 UKI: file=%s\n' "$description"
printf 'linux L1 UKI: size=%s bytes\n' "$(stat -Lc %s -- "$output")"
