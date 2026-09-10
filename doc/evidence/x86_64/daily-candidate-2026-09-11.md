# Trusted Direct daily-candidate work log (2026-09-11)

Status: implementation in progress, **not QEMU-qualified or physical-ready**.

## Starting point

- Branch: `feat/x86-thin-monitor`; initial HEAD: `01564be384d7b6bca2aa77359dd5e9ce0485c94e`.
- Recorded `git branch --show-current`, `git status --short`, and `git log --oneline -30`
  before edits. The existing `AGENTS.md` change belongs to the user and is excluded.
- Local checked-out code is authoritative. No branch switch or public-main reconstruction.
- No subagents, Claude, physical OEM Windows test, activation/key query or device emulation.

## Confirmed existing architecture

The nested implementation already selects L1's actual hardware VMCS and EPTP;
there is no VMCS02/EPT02 to remove. Platform-derived carrier EPT and private host
page tables are active. CPUID/XSETBV, original host-field validation, guest operand
faults, CR2/FXSAVE scratch, bounded MSR lists, control provenance, VPID lease and
2-MiB nested EPT already exist. They are not reimplemented under the new threat model.

`CpuMonitor` is GS-bound but only initialized for the BSP. Physical selection still
rejects additional processors. AP takeover, S3 reconstruction and root NMI delivery
remain required work; no safety gate has been removed. Current/UnsafeDirect selection
and A/B measurements are not yet implemented.

The overlay is **already firmware-backed**, not memory-only. It renames only
global boot/driver variables to `P<8-hex-ID>:<name>` under the project GUID, retaining
the original firmware SetVariable, GetVariable and enumeration entry pointers.
QueryVariableInfo is firmware pass-through. Firmware stores/updates each variable;
there is no filesystem dependency after ExitBootServices and no second persistent store.
BootCurrent remains firmware-provided. Secure Boot/identity variables are shared.
The research loader still picks the profile from image presence; an independent
persistent primary-OS selector and full reboot contract fixture are pending.

## Increment 1: profile names and adapter contract

- `uefi_variable_overlay::UefiProfile::{Windows,Linux}` names the selected primary OS
  while preserving existing persistent IDs 1/2 and keys. Unknown IDs do not fall back.
- `RuntimeVariableOverlay::get_variable` now returns attributes on BUFFER_TOO_SMALL.
  Actual ABI hooks already delegated this correctly; the pure adapter/test was wrong.
- The pure enumeration adapter now distinguishes an invalid/foreign cursor from
  end-of-enumeration, without modifying outputs on error. Actual ABI hooks already
  delegate missing physical-cursor validation to firmware.
- Documented backend-owned append/deletion and atomic durable-update obligations.
  In particular an empty append is not ordinary zero-size deletion.
- Existing runtime profile constants/validation consume the named type.

The error distinctions and per-variable old-or-new persistence guarantee come from
[UEFI 2.11 Runtime Services, sections 8.2.1–8.2.3](https://uefi.org/specs/UEFI/2.11/08_Services_Runtime_Services.html).
They do not imply transactionality across several Boot####/BootOrder writes.

Validation: `/tmp/x86-profile-types-regression.log`, command exit 0.
Host: overlay 8 PASS / 0 FAIL; loader 211 PASS / 0 FAIL across its existing six
feature entries. Standard release QEMU suite: 11 PASS / 0 FAIL (9 QEMU/KVM,
2 QEMU TCG); expected root/ownership rejection fixtures count as negative-test
PASS, not successful OS boots. Its outer-KVM cases are reference evidence only.
Commands: `nix develop --accept-flake-config --command bash -c 'set -e; cargo fmt;
cargo xtest -p uefi_variable_overlay; cargo xtest -p x86_uefi_loader;
cargo xrun x86 --release'`.

## Unchanged-HEAD Hyper-V reproduction

Built initial HEAD using `nix develop --accept-flake-config --command cargo xbuild
x86 --release` (PASS; `/tmp/x86-profile-baseline-build.log`). Before any runtime
behavior edits, launched the existing default 600-second Direct Hyper-V gate with
a new qcow2 child, copied OVMF/TPM state and recorded EFI hashes:

```sh
env WINDOWS_TEST_DIR=/tmp/x86-profile-current-hyperv.IwZGSp/guest \
    WINDOWS_DIRECT_MODE=physical-uefi WINDOWS_MEMORY=4G \
    WINDOWS_VNC=127.0.0.1:49 \
    bash scripts/x86_64/windows/windows-test.sh monitor-hyperv
```

QEMU/KVM, default q35 PCI aperture, `host,+vmx,-hypervisor`, one L1 CPU, no overlay.
This is project Direct-VMX evidence, not physical hardware. The immutable seed is
`/tmp/thin-hv-nested-windows-reference.QaTUrn`; no original disk is test-written.
Run directory retains command provenance, hashes, logs and bounded diagnostics.
Result pending. Outer-KVM results remain reference evidence only.

## Lifecycle reference audit (no copied code)

- [BitVisor ap.c](https://github.com/matsu/bitvisor/blob/master/core/x86/ap.c):
  UEFI delays AP startup through the local-APIC hook rather than claiming firmware APs.
- [BitVisor wakeup.c](https://github.com/matsu/bitvisor/blob/master/core/x86/wakeup.c)
  and [acpi_iohook.c](https://github.com/matsu/bitvisor/blob/master/core/x86/acpi_iohook.c):
  quiesce, preserve waking vector, reconstruct CPU/VMX state, synchronize CPUs and
  cancel when a sleep write returns. Do not copy its low-memory address-zero patching.
- [Xen power.c](https://github.com/xen-project/xen/blob/master/xen/arch/x86/acpi/power.c):
  freeze execution, take CPUs down, restore feature/MSR/MTRR assumptions, bring CPUs
  back before thawing guests; failure unwinds before guest execution resumes.
- [HyperPlatform power_callback.cpp](https://github.com/tandasat/HyperPlatform/blob/master/HyperPlatform/power_callback.cpp):
  devirtualize on departure from S0 and reinitialize on return, relying on Windows
  callbacks unavailable to a standalone UEFI L0.

The three requested BitVisor commit URLs returned 404 from the `matsu/bitvisor`
mirror; exact-commit verification is pending locating the authoritative repository.

## Qualification still required

Persistent profile selector and live-runtime reboot/full-store/failure fixture;
1/2/4/8 actual L1 CPUs; AP handoff; S3/cancellation/time/NMI; Direct Hyper-V/WSL2
and S4; short/extended daily suite; measured Current/Unsafe A/B and default decision.
Existing Linux test failures in `correctness-2026-09-09.md` are not waived.

Physical-only: firmware/SMM/EC/AML quirks, genuine S0ix residency, physical device
power loss, Wi-Fi/Thunderbolt/USB-C dock resume, AP firmware ownership and battery/AC
transitions. No physical laptop has been tested in this iteration.
