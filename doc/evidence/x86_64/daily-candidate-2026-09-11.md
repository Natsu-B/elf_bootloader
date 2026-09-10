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
Result: **FAIL**, command exit 1 at the unchanged 600-second bound, screen still
`Please wait`. L2 entries/reflections 4,577,759; nested entry failures 0;
L1 exits 39,970,515; VMREAD 786,864,838; VMWRITE 167,969,630; VMPTRLD 14,699,806.
L2 RDMSR 3,633,800 and WRMSR 438,463 dominate. This is evidence of continuing
nested activity, not a diagnosed root cause or proof of correct state/interrupts.
`failure.png` was visually inspected; no bugcheck is visible. Outer-KVM results
remain reference evidence only.

Finished-run cleanup removed exactly its qcow2 child, three copied variable stores,
two raw/MSI symlinks (not their targets), copied TPM directories, generated media
and copied ESP. These disposable copies are not recoverable; reusable original
seed/backing files, hashes, screenshots and diagnostics remain intact.

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

The requested commits were subsequently found and inspected in `igel-oss/bitvisor`:
[924b9c3](https://github.com/igel-oss/bitvisor/commit/924b9c3d7d778bcdeb03cfce7d83ffb95d5845f2)
adds the all-CPU barrier before guest resume;
[8b18a60](https://github.com/igel-oss/bitvisor/commit/8b18a6084e1cb0b7f6c8caafbb9acb68c9501d35)
updates per-CPU time correction as well as the TSC base;
[ed3274a](https://github.com/igel-oss/bitvisor/commit/ed3274a2b03769f601fb121d3ac5086c2c615ba2)
restores hypervisor-owned xHCI address state. The latter complexity is unnecessary
here while device state stays guest-owned; no xHCI save/restore was added.

## Increment 2: live firmware profile fixture

`cargo xrun x86 --uefi-profiles --release` uses the existing x86 runner with a
new standalone fixture backend, no Windows/Linux image and VMX hidden. The boot
application loads its runtime-driver copy using shared current-ESP image helpers;
the fixture directly uses production `runtime_variables` hooks, then rolls them
back before every firmware reset. A runtime driver directly at BOOTX64 was rejected
by OVMF's boot discovery in the first attempt; normal LoadImage/StartImage fixed the
fixture boot path, without a test timeout increase.

Second run: **PASS**, QEMU/KVM default q35, 256 MiB, one CPU, 60-second total bound.
Log `/tmp/x86-profile-contract-second-run.log`. Cold then warm ResetSystem produced
three fresh driver entries and 11 profile views. Both firmware-backed namespaces
survived; append, empty append, deletion, buffer-too-small attributes and complete
filtered enumeration passed. Nine security/BootCurrent values were compared in
memory without printing/persisting their contents. Store exhaustion occurred after
230 filler variables; rejected growth retained the old value, then fillers were
deleted. This is firmware/profile evidence, **not Direct-VMX or OS lifecycle proof**.

The independent `SelectedProfile` record has eight bytes, magic/version/reserved
fields and a supported OS ID; malformed records do not fall back. NV+BS attributes
keep this boot-time selection outside normal OS runtime writes. Guest BootNext
does not alter it. Production boot-path selection wiring remains pending.

The selector is now explicitly changed Windows -> Linux before cold reset and
Linux -> Windows before warm reset; each new driver validates the stored selection
independently of its two profile views. Same fixture passes under both QEMU/KVM
(`host,-vmx,-hypervisor`) and QEMU TCG (`qemu64`). Final logs:
`/tmp/x86-profile-contract-selector-reset.log` and `/tmp/x86-profile-contract-tcg.log`.
QueryVariableInfo remains unchanged across hook installation, and a mis-staged
application in the runtime-driver position is rejected/unloaded before StartImage.

Host regression: **362 PASS / 0 FAIL / 0 SKIP** (nested_vmx 32, x86_64_hal 59,
uefi_variable_overlay 9, x86_uefi_loader 213, x86_guest_uefi_test 10, xtask 39).
`/tmp/x86-profile-contract-all-regression.log` records all six `cargo xtest -p`
commands, `cargo xbuild x86`, `cargo xrun x86 --release`, and `cargo fmt --check`.
The standard release run now includes the strict profile gate: **12 PASS / 0 FAIL**
(10 QEMU/KVM, 2 TCG). Further standalone profile reruns are not added to that total.

`nix develop --accept-flake-config --command cargo xrun x86 --nested --release`:
**15 PASS / 5 FAIL / 0 SKIP**; command exit 1. All 14 Direct configurations pass
(8 native/negative contracts, 6 real Linux KVM configurations including 12 GiB).
The additional Linux reference passes. Five outer-KVM/reference instruction
contracts remain FAIL: native, readonly-vmcs, msr, msr-abort-store, msr-abort-load.
These match the pre-existing differences, are not waived, and do not indicate
successful Direct SMP (the 2-CPU rejection is deliberately a negative test).
Log: `/tmp/x86-profile-contract-nested-release.log`.

Intermediate test-development failures retained: OVMF did not discover a runtime
driver as a boot application; a host gate test initially used a wrong test-module
path; a fmt check overlapped subsequent edits. Those were fixture/integration
errors, not silently retried Hyper-V success. Later frozen regression commands
passed as listed above. No timeout was increased.

Latest-selector debug fixture: **PASS**, `/tmp/x86-profile-contract-selector-debug.log`.
Mis-staged runtime negative: **PASS** (expected runner exit 1, no phase entry or
variable write), `/tmp/x86-profile-contract-reject-app.log`; it reports bounded
type-rejection diagnostics instead of recursively loading itself. The earlier
`/tmp/x86-profile-contract-image-guard.log` reran loader host tests and the release
fixture after adding the pre-StartImage image-type guard.

This increment deliberately does not claim post-ExitBootServices/virtual-address
transition coverage from the standalone fixture: its resets occur before EBS.
Full-store enumeration, post-EBS contract coverage, production selector/path wiring
and additional lifecycle transitions remain subsequent implementation work.

## Increment 3: full-store and post-EBS runtime contract

`profile_contract::Fixture::{enumeration,full_store,memory_map,runtime_variables,
exit_boot_contract,run}` now checks all 230 successfully created filler variables,
twice, including buffer-size retry stability. A bounded bitset replaces the small
expected-name mask; no heap or new store is introduced.

On the third boot the actual runtime image retains the production hooks, obtains
the firmware memory map and exits Boot Services. Only bounded GetMemoryMap/EBS
retries are possible after the first attempt; no hook destructor or StartImage
return may reference partially shut-down firmware. It exercises physical Runtime
Services, then calls SetVirtualAddressMap with the complete firmware map and
identity virtual addresses. Variable read/write/delete/enumeration, capacity and
security pass-through are checked in both phases. Runtime updates are then verified
after a third real ResetSystem on a fourth boot, together with the unchanged selector
and the other profile. This is **identity-virtual firmware coverage**, not proof of
nonidentity OS mappings, Direct nested operation, or S3/S4.

The first post-EBS test failed because it incorrectly expected INVALID_PARAMETER
when modifying an existing boot-only variable. Firmware correctly returned
WRITE_PROTECTED; a new boot-only variable instead returns INVALID_PARAMETER.
The expectation was corrected after checking EDK II's existing-variable path in
[Variable.c](https://github.com/tianocore/edk2/blob/master/MdeModulePkg/Universal/Variable/RuntimeDxe/Variable.c).
Runtime hooks were not changed to hide that failure. Log
`/tmp/x86-profile-contract-runtime-first.log` retains the failed assertion.
No timeout increase occurred (the initial progress interpretation as timeout was
corrected once the final log was available).

`/tmp/x86-profile-contract-runtime-second.log`: physical/identity-virtual runtime
checks PASS. `/tmp/x86-profile-contract-runtime-persistence.log`: final four-boot,
three-reset release QEMU/KVM fixture PASS at the unchanged 60-second total bound.
The runner gate and its existing xtask test now require all runtime milestones,
the third reset, and the final two-profile verification, not merely a PASS string.

Frozen regression `/tmp/x86-profile-contract-runtime-regression.log`: command exit
0, **362 host PASS / 0 FAIL / 0 SKIP**, the same package counts as increment 2.
Ran all six `cargo xtest -p` commands listed above, release standalone TCG with
the same 256-MiB/one-CPU/60-second fixture configuration, debug
`cargo xrun x86 --uefi-profiles`, `cargo xbuild x86`, and
`cargo xrun x86 --release`; then `cargo fmt --check` and `git diff --check`.
The normal release matrix is **12 PASS / 0 FAIL / 0 SKIP**; with the two standalone
TCG/debug cases this invocation has **14 QEMU PASS / 0 FAIL / 0 SKIP** (11 KVM,
3 TCG). Its three outer-KVM smoke cases remain reference evidence only. The
pre-existing nested reference failures and Direct Hyper-V failure above remain
open; this fixture-only increment does not change VMX runtime behavior.

## Increment 4: explicit profile Direct boot path (in progress)

The opt-in `profile-direct-vmx` feature adds `direct mode=profile-uefi`; it is not
a new nested execution implementation. It uses the same platform map, private L0,
Direct hardware VMCS/EPTP and conservative single-BSP ownership gate. Mode cookie
2 distinguishes its runtime image/handoff from physical no-overlay mode 1 and
research mode 0. Existing backends remain selectable without silent fallback.

`profile_boot::{explicit_profile,configured_path,load_selected,SelectedBoot::commit}`
accepts exact terminated UTF-16 `windows` / `linux`, or empty options to retain the
existing eight-byte firmware selector. Absence/corruption without explicit selection
is an error. The selector is committed only after loading the selected OS and
validating the runtime-monitor image, before starting that monitor. Repeated
persistent boots do not rewrite the selector. Firmware SetVariable failure retains
its native status and existing cleanup path. No activation/identity data is accessed.

`physical_chainload::{load_path,load_profile_path}` reuses current-ESP protocol,
path and self-reference checks. Windows selects the original bootmgfw path; Linux
uses `THIN_HV_LINUX_EFI_PATH` from the build environment and rejects missing or
invalid configuration. The no-overlay physical path retains its existing LoadOptions
path contract. Profile-mode hooks are installed only in the original retained
runtime image, after allocations; the L0-private copy never owns their entry points.

`cargo xrun x86 --profile-direct --release`: **4 QEMU Direct PASS / 0 FAIL / 0 SKIP**,
`/tmp/x86-profile-direct-second.log`. Each case has default q35, one CPU, 256 MiB,
KVM `host,+vmx,-hypervisor`, the same native UEFI guest and a 60-second bound.
The separate selection driver supplies explicit Windows/Linux commands over an
opposite stored selector, or empty commands over each stored selector. The actual
L0 loads the current-ESP path and its L1 payload verifies the active variable namespace.
The guest's previous Linux-only namespace probe was corrected to seed/check the
selected namespace; the legacy absent-selector research fixture still explicitly
uses Linux. No Windows/Linux OS boot is inferred from this native UEFI payload.

Debug `cargo xrun x86 --profile-direct` also passes all four cases. The same command
log `/tmp/x86-profile-direct-regression.log` then records the existing release
matrix **12 PASS / 0 FAIL / 0 SKIP**, fmt/diff checks and xtask **39 PASS**. Total
QEMU for that invocation is **16 PASS / 0 FAIL / 0 SKIP** (14 KVM, 2 TCG).
Package host log `/tmp/x86-profile-direct-host-regression.log` has loader **275 PASS**
and guest **10 PASS**; its first gate test correctly caught a missing backend
marker that was not rejected. The strengthened checker requires both ordered Direct
entries; the subsequent xtask rerun above passes. Initial fixture build also needed
an explicit Result type; `/tmp/x86-profile-direct-first.log` retains that compiler
failure. The earlier `/tmp/x86-profile-direct-host-first.log` found the old two-mode
handoff test expectation; the three-mode expectation is now exercised.

This increment still selects the configured primary boot-manager path, not an
arbitrary private BootNext/BootOrder load option. Those variables are isolated and
persistent but their cold-boot execution policy is **not complete**. Do not treat
this opt-in mode as an OS-update/S4/daily-use qualification. Driver options remain
namespaced; firmware BootCurrent and all security/identity state remain shared.

Nested regression `/tmp/x86-profile-direct-nested-regression.log` repeats all 20
release cases: **15 PASS / 5 FAIL / 0 SKIP**. All 14 Direct cases pass, including
the six Linux KVM configurations and both 12-GiB/default-high-PCI runs. The five
outer-KVM/reference contracts fail in the same categories listed in increment 2;
the outer Linux reference passes. No unexpected Direct regression was observed.
The same invocation passes nested_vmx 32, x86_64_hal 59 and overlay 9 host tests;
combined with loader 275, guest 10 and the latest xtask 39, affected host coverage
is **424 PASS / 0 FAIL / 0 SKIP**. These counts refer to the latest successful
package runs, not an assertion that the earlier intermediate gate failure passed.

## Qualification still required

Full profile BootNext/BootOrder execution and failure injection;
1/2/4/8 actual L1 CPUs; AP handoff; S3/cancellation/time/NMI; Direct Hyper-V/WSL2
and S4; short/extended daily suite; measured Current/Unsafe A/B and default decision.
Existing Linux test failures in `correctness-2026-09-09.md` are not waived.

Physical-only: firmware/SMM/EC/AML quirks, genuine S0ix residency, physical device
power loss, Wi-Fi/Thunderbolt/USB-C dock resume, AP firmware ownership and battery/AC
transitions. No physical laptop has been tested in this iteration.
