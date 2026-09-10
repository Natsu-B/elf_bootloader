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

## Increment 5: execute selected-profile boot options

Verified `ddf39cb` still loaded only configured managers. The existing firmware
backing namespace is now used directly for BootNext/BootOrder/Boot#### selection;
no second store, new dependency, device model, or nested translation layer exists.

Changed implementation surfaces:

* `profile_boot.rs`: bounded `BootOption` decoding, `load_ordered`/`load_option`,
  retained EFI optional-data ownership, `SelectedBoot::commit/release`, native
  BootCurrent publication and failed-handoff restoration.
* `runtime_variables.rs`: `read_profile_boot_variable` and
  `consume_profile_boot_next`, before hooks and only in the selected namespace.
* `physical_chainload.rs`: `boot_file_path`, `same_boot_device`, and
  `load_profile_device_path`; full/current-ESP and partition-signature short forms
  reuse the existing image-path and firmware LoadImage checks.
* `vmx_smoke.rs::start_runtime_monitor`: commit/release selected boot metadata at
  the existing validated runtime-image handoff/cleanup boundaries.
* `profile_selection_test.rs::seed_boot_option`, guest `boot_option_fixture`,
  `run-uefi-smoke.sh::check_profile_selection_log`, and xtask
  `run_x86_profile_selection`: two additional actual-Direct cases and strict gates.

Windows BootNext explicitly selects an inactive Boot0042 with a file-only path.
Linux consumes a missing BootNext, skips inactive/missing BootOrder candidates,
and selects active Boot0042 by a full current-device path. The actual selected
payload (a disposable EFI application, not Windows/Linux) verifies its FilePath,
eight retained optional bytes, BootCurrent/attributes, and absent consumed BootNext.
Host tests additionally cover truncated/oversized load options, category filtering,
short GPT path identity, malformed nodes, and mismatched partitions.

BootNext consumption follows native one-shot semantics and is not rolled back on
an unsuccessful target attempt. The independent persistent selector is unaffected.
BootCurrent is native volatile boot metadata, not a fake security/identity value.
Unsupported cross-ESP paths fail rather than searching another filesystem.
Only missing BootOrder permits the configured same-profile default; an exhausted
recorded order fails. Explicit profile UI bypasses private boot order for its
configured manager. StartImage-return retry among multiple candidates and automatic
Driver#### execution remain unsupported; this is not a full BDS replacement.

Reference behavior inspected without copying code:
[EDK II BdsEntry.c](https://github.com/tianocore/edk2/blob/master/MdeModulePkg/Universal/BdsDxe/BdsEntry.c)
consumes BootNext before launch and applies active/boot-category filtering to
automatic BootOrder attempts. Firmware remains responsible for PE/Secure Boot
validation and atomic per-variable persistence.

`/tmp/x86-profile-boot-options-regression.log` records exit 0 for:

```sh
nix develop --accept-flake-config --command bash -c 'set -e; cargo fmt; cargo xtest -p x86_uefi_loader; cargo xtest -p x86_guest_uefi_test; cargo xtest -p xtask; cargo xrun x86 --profile-direct --release'
```

Host: loader 277, guest 10, xtask 39 = **326 PASS / 0 FAIL / 0 SKIP**.
QEMU/KVM Direct profile suite: **6 PASS / 0 FAIL / 0 SKIP**, default q35, host VMX,
one L1 CPU, 256 MiB, 60 seconds per case. This is not SMP or real-OS evidence.

`/tmp/x86-profile-boot-options-direct-regression.log` records:

```sh
nix develop --accept-flake-config --command bash -c 'set -e; cargo xrun x86 --profile-direct; cargo xrun x86 --release; cargo fmt --check; git diff --check; cargo xrun x86 --nested --release'
```

Debug profile Direct: **6 PASS / 0 FAIL / 0 SKIP**. Standard release:
**12 PASS / 0 FAIL / 0 SKIP**. Formatting/diff checks pass. Nested release:
**15 PASS / 5 FAIL / 0 SKIP**; all 14 Direct cases pass, the outer Linux reference
passes, and the same five outer reference contract failures remain. Therefore this
combined command exits 1, not PASS. Direct Linux again runs 64 lifecycle cycles in
each of six configurations, including 12-GiB/default high-PCI layouts. No nested
runtime behavior was optimized in this increment. No real Windows, S3/S4, physical
hardware, or Current/Unsafe A/B result is inferred from these native fixtures.

Fixture ESPs are invocation-owned and removed by the existing runner trap,
including the new OPTION.EFI. Small logs are retained as evidence; no generated
EFI image, firmware-variable store, disk image, or unrelated AGENTS.md change is
part of the commit.

## Increment 6: complete read-only firmware CPU inventory

The old physical gate queried only the current BSP's ProcessorInformation.
`cpu_inventory.rs::Inventory::collect/read` now visits every firmware slot, up to
64 fixed-capacity entries, separating firmware index from 32-bit APIC identity.
It preserves disabled CPUs, rejects duplicate IDs/inconsistent BSP or enabled
counts, and returns a query failure without publishing a partial inventory.
It never calls StartupAllAPs, StartupThisAP, SwitchBSP or EnableDisableAP.

`physical_chainload.rs::require_single_cpu` reuses this checked inventory and
retains its single-CPU rejection. `physical_preflight.rs::inventory` emits bounded
read-only per-CPU records. `main.rs` scopes the module to those backends.
The existing smoke runner adds `check_cpu_inventory_log`; xtask's standard x86
matrix runs 1/2/4/8 inventories with both KVM and TCG, preserving separate logs.
This is preparation for delayed takeover, **not implemented AP VMX ownership**.
No unused lifecycle state machine or shared global VMX lock was added.

`/tmp/x86-cpu-inventory-host-build.log` records exit 0 for:

```sh
nix develop --accept-flake-config --command bash -c 'set -e; cargo fmt; cargo xtest -p x86_uefi_loader; cargo xbuild x86'
```

Loader host tests: **283 PASS / 0 FAIL / 0 SKIP**. Debug x86 build: PASS.
`/tmp/x86-cpu-inventory-qemu-regression.log` preserves an intermediate xtask
package timeout: all 39 assertions completed successfully in 30.37 seconds, but
the existing 30-second process gate correctly failed (code 124). Its QEMU step
did not run. The new test had unnecessarily spawned 65 shell processes to check
each missing row again at the 64-CPU boundary. Every missing-row case remains
covered at 1/2/4/8; the 64-CPU bound retains positive and mutated-input checks.
No production timeout or checker rule was relaxed.

`/tmp/x86-cpu-inventory-qemu-second.log` records exit 0 for:

```sh
nix develop --accept-flake-config --command bash -c 'set -e; cargo fmt; cargo xtest -p xtask; cargo xrun x86 --release'
```

xtask: **39 PASS / 0 FAIL / 0 SKIP**, 28.08 seconds. Standard release now has
**18 PASS / 0 FAIL / 0 SKIP** (13 QEMU/KVM, 5 QEMU/TCG), including its expected
fail-closed fixtures. The eight CPU inventories all report the exact 1/2/4/8
counts, all processors enabled, BSP index 0, and no project AP startup/VMX.
The ordinary Direct smoke, isolated profile reset/runtime fixture, and existing
reference/non-VMX regressions also pass. Outer results remain reference only.

Next CPU transition design under evaluation (not implemented by this inventory
increment): reserve complete AP backing and a low-memory bootstrap before L1;
wrap the original firmware ExitBootServices call in retained L1 bootstrap code;
only on successful return notify L0, take over APs without MP Services, and hold
the BSP until all required APs own VMX/carrier/host state. A failed firmware call
must leave APs untouched and preserve normal GetMemoryMap/retry semantics.
An ExitBootServices notification alone is too early to prove the original service
has completed. APs already in VMX can subsequently receive INIT/SIPI as VM exits,
avoiding a permanent APIC-MMIO trap merely to discover their first startup.
This requires real reset-mode guest state and an all-CPU barrier before any SMP
claim; none is inferred from read-only firmware enumeration.

`/tmp/x86-cpu-inventory-direct-regression.log` records:

```sh
nix develop --accept-flake-config --command bash -c 'set -e; cargo xtest -p nested_vmx; cargo xtest -p x86_64_hal; cargo xtest -p x86_guest_uefi_test; cargo xtest -p uefi_variable_overlay; cargo xrun x86 --profile-direct --release; cargo fmt --check; git diff --check; cargo xrun x86 --nested --release'
```

Host: **110 PASS / 0 FAIL / 0 SKIP**; combined latest package coverage is
**432 PASS / 0 FAIL / 0 SKIP**. Profile Direct release: **6 PASS / 0 FAIL / 0 SKIP**.
Nested release: **15 PASS / 5 FAIL / 0 SKIP**, again all 14 Direct cases pass and
the same five outer/reference contracts fail. Overall command status is 1 because
those reference failures are not waived. Formatting and diff checks pass.
No generated artifact or unrelated AArch64/user change is included.

## Increment 7: observe the actual end of firmware Boot Services

`boot_handoff.rs::install/Installed/exit_boot_services/after_firmware` introduces
an opt-in physical/profile Direct Boot Services wrapper, not a Runtime Services
variable overlay. It retains the original firmware entry, forwards the image/map
key unchanged, and notifies L0 only after EFI_SUCCESS. Invalid map keys and other
firmware errors return unchanged without notifying L0 or allocating/calling other
Boot Services. The exact table entry and CRC are restored on pre-entry rollback,
including failed CRC calculation. No filesystem or firmware service is used after
success. The wrapper belongs to the retained registered runtime image, never a
disposable Boot Services allocation or L0's private PE copy.

`vmx_smoke.rs::start_resident_core` scopes the installation to validated runtime
preparation. `CpuMonitor::firmware_handoff` owns the one-shot observation on the
current CPU; `dispatch_l1_exit` acknowledges the dedicated VMCALL, advances its RIP,
and emits one bounded lifecycle marker. Duplicate handoff or an internal ABI
failure stops visibly instead of pretending firmware services can be retried.
The marker explicitly says `cpus=1 ap_takeover=0`: **AP VMX takeover is not yet
implemented**, and the existing multi-CPU rejection is still enforced.

`run-linux-kvm-test.sh::check_log` now requires that exact successful boundary
before a physical Direct Linux lifecycle begins. The generic UEFI backend/mode
gates reject handoff failures, mixed backends and duplicate/malformed records.
xtask's existing gate tests cover a missing, repeated, misordered or falsely
AP-enabled marker. No new runner framework, dependency or device mediation exists.

`/tmp/x86-firmware-handoff-direct-first.log` records a test-build failure because
the initial CRC mock used a const input pointer rather than r-efi's mutable ABI;
the mock was corrected, not the production signature. The next run
`/tmp/x86-firmware-handoff-direct-second.log` passes **287 loader host tests** and
observes the real post-EBS marker plus 64 completed KVM cycles in both physical
Direct Linux configurations. However, two runner processes encountered a shell
file-offset parse error because their scripts were edited while they were running.
That invocation is **13 PASS / 7 FAIL / 0 SKIP**, not a clean qualification: five
known outer contract failures, plus outer Linux and 12-GiB physical Direct runner
failures. The successful guest output does not override a failed runner.
The complete rerun freezes runner/source files for its duration.

`/tmp/x86-firmware-handoff-frozen-regression.log` records:

```sh
bash -n scripts/x86_64/run-uefi-smoke.sh
bash -n scripts/x86_64/run-linux-kvm-test.sh
nix develop --accept-flake-config --command bash -c 'set -e; cargo fmt; cargo xtest -p xtask; cargo xrun x86 --release; cargo xrun x86 --profile-direct --release; cargo fmt --check; git diff --check; cargo xrun x86 --nested --release'
```

xtask: **39 PASS / 0 FAIL / 0 SKIP** (28.05 seconds). Standard release:
**18 PASS / 0 FAIL / 0 SKIP**. Profile Direct release: **6 PASS / 0 FAIL / 0 SKIP**.
Nested release: **15 PASS / 5 FAIL / 0 SKIP**, all 14 Direct cases passing;
the same five outer contracts remain failures, so the command correctly exits 1.
Both physical Direct Linux configurations prove the new exact post-EBS boundary
before completing their 64 L2 lifecycle cycles. Overall this invocation has
**39 QEMU PASS / 5 FAIL / 0 SKIP**. Shell/format/diff checks pass. Latest combined
package host coverage is **436 PASS / 0 FAIL / 0 SKIP** (287 loader, 39 xtask,
32 nested_vmx, 59 HAL, 10 guest, 9 overlay). No Windows/S3/SMP readiness is inferred.

## Increment 8: unrestricted carrier and architectural CR0 ownership

The active carrier previously had no CR0 mask and did not enable unrestricted
guest. An AP needs visible CR0.NE=0 while VMX requires the hardware bit to remain
set. `vmx_smoke::configure_and_launch` now requires hardware EPT/unrestricted-guest
support and shadows NE. This changes the carrier, not L1's advertised nested
capabilities or the direct L1 hardware VMCS/EPTP model. Unsupported additional
fixed-bit requirements fail before VMXON; there is no q35 or outer fallback.

`l1_visible_cr0` uses the guest/mask/shadow composition, including for VMXON
validation. `handle_l1_cr0_write` uses HAL `control_state::cr0_write` to validate
the complete write before changing fields: hardwired ET, ignored low reserved
bits, high reserved bits, NW/CD, PE/PG, long-mode/PAE/PCIDE and CET/WP dependencies.
Invalid writes inject #GP(0) at the unchanged RIP. Valid mode changes update EFER
LMA and the carrier IA-32e entry control together. Legacy PAE reloads first capture
and check all four PDPTEs, including physical-width/reserved-bit rules, then publish
them; no incomplete PDPTE set is used. The HAL exports their four VMCS encodings.

`handle_l1_vmxon/vmxoff` additionally mask required PE/PG only during L1's VMX-root
lifetime. Unrestricted execution must not relax L1's own VMX fixed-bit contract.
Ordinary writes that do not change shadowed bits still execute in hardware;
no EPT walk or new device interception was introduced. The carrier remains VPID
untagged, so VM entry provides the needed linear-translation invalidation.

The existing native L1 XSTATE fixture now toggles NE four times across CPUID exits
and recovers from four invalid MOV-to-CR0 instructions. Its existing VMX-root
CR4 guard also verifies six CR0 faults (PE/NE/PG, with and without a current nested
VMCS). The strict runner and xtask fixture require these counts. These execute
the project carrier interception, not just KVM L2 emulation. Pure HAL tests cover
reset/long-mode transitions and legacy PAE reload/boundaries; they do not constitute
a hardware AP startup or legacy-PAE execution test.

Recorded commands (all logs outside Git):

```sh
# /tmp/x86-carrier-cr0-host-first.log
nix develop --accept-flake-config --command bash -c 'set -e; cargo fmt; cargo xtest -p x86_64_hal; cargo xtest -p nested_vmx; cargo xtest -p x86_uefi_loader'
# /tmp/x86-carrier-cr0-native-regression.log
nix develop --accept-flake-config --command bash -c 'set -e; cargo fmt; cargo xtest -p x86_guest_uefi_test; cargo xtest -p xtask; cargo xbuild x86; cargo xrun x86 --nested --release'
# /tmp/x86-carrier-cr0-boot-regression.log
nix develop --accept-flake-config --command bash -c 'set -e; cargo xrun x86 --release; cargo xrun x86 --profile-direct --release; cargo fmt --check; git diff --check'
# /tmp/x86-carrier-cr0-vmx-guard-regression.log (includes the final VMX-root guard)
nix develop --accept-flake-config --command bash -c 'set -e; cargo fmt; cargo xtest -p x86_uefi_loader; cargo xtest -p x86_guest_uefi_test; cargo xtest -p xtask; cargo xbuild x86; cargo xrun x86 --nested --release'
```

First host run: **380 PASS / 0 FAIL / 0 SKIP** (HAL 61, nested 32, loader 287).
First native run: **49 host PASS** (guest 10, xtask 39), debug xbuild PASS;
nested release **15 PASS / 5 FAIL / 0 SKIP**, all 14 Direct cases passing.
The first boot run passes all 18 standard and six profile Direct cases.
The final VMX-root-guard run passes **336 host tests**, debug xbuild, and again
all 14 Direct nested cases. Both nested invocations exit 1 for the same five
known outer contract failures; outer Linux passes and remains reference-only.
The six Direct Linux configurations each complete 64 KVM lifecycle cycles,
including physical-mode default q35 at 2 and 12 GiB and the post-EBS handoff.
No Windows, S3, real AP/SMP or physical-hardware result is inferred.

The final review also keeps the initial shadow equal to **original firmware CR0**,
not L0's normalized hardware CR0. The frozen final invocation records this exact
source in `/tmp/x86-carrier-cr0-original-state-regression.log`:

```sh
nix develop --accept-flake-config --command bash -c 'set -e; cargo fmt; cargo xtest -p x86_uefi_loader; cargo xbuild x86; cargo xrun x86 --release; cargo xrun x86 --profile-direct --release; cargo fmt --check; git diff --check; cargo xrun x86 --nested --release'
```

**287 loader PASS**, debug build PASS, standard **18 PASS**, profile Direct
**6 PASS**, nested **15 PASS / 5 FAIL / 0 SKIP**, with the same five outer failures.
All **14 Direct nested** cases pass. The preceding
`/tmp/x86-carrier-cr0-final-regression.log` additionally repeats HAL **61 PASS**,
nested_vmx **32 PASS**, standard **18 PASS** and profile Direct **6 PASS** using
`cargo xtest -p x86_64_hal; cargo xtest -p nested_vmx; cargo xrun x86 --release;
cargo xrun x86 --profile-direct --release; cargo fmt --check; git diff --check`
inside the same `nix develop ... --command bash -c 'set -e; ...'` environment.
Latest combined package coverage: **438 PASS / 0 FAIL / 0 SKIP** (including the
unchanged nine overlay tests). No AArch64 production path, dependency, physical
device state or firmware identity was changed.

## Increment 9 (in progress): actual AP carrier ownership

The opt-in `smp-direct-vmx` image (`smp-uefi` runner mode) now reserves one
independent monitor block per firmware CPU. The normal physical/profile images
retain their single-CPU rejection. This is experimental QEMU SMP, not a physical
or daily-use qualification claim.

`MonitorLayout`, `PreparedMonitor`, and `cpu_boot::{Handoff,CpuBoot}` separate
whole-machine reservations from each CPU's VMXON/carrier/error VMCS, stack,
GDT/TSS/IST, GS-bound nested state, XSTATE scratch, VPID namespace, diagnostics
and private host roots. Firmware enumeration does not start/disable APs. Only
healthy, enabled processors are accepted for this first milestone. A reserved
RuntimeServicesCode page below 1 MiB supplies the SIPI bootstrap; all initial
roots/monitor blocks are allocated below 4 GiB, without limiting L1 RAM/MMIO.
The bootstrap selects full CPUID APIC identities, switches to the owner's root
and stack, and retains INIT's original cache bits and PAT. No low address is
stolen and no fixed q35 EPT/host-map fallback is used.

Only after the original ExitBootServices succeeds does the BSP send targeted
INIT/SIPI. Each AP checks its capabilities, installs private descriptors,
executes its own VMXON and enters a real-mode carrier CPUID probe. The BSP waits
for every probe before acknowledging firmware handoff. A failed/missing AP
prevents OS continuation; no live allocation is freed. Initial LTR required
making the VMX-oriented busy TSS descriptor available first: the first 2-CPU
attempt stopped exactly at LTR (RIP private image + `0x1bda4`), before GS/IDT
installation. LTR itself now sets the descriptor busy again. The first
successful 2-CPU guest run exposed mixed tty/printk output; Linux status messages
now share the existing printk-based lifecycle logger, without relaxing the gate.

The existing Linux runner accepts `LINUX_KVM_DIRECT_MODE=smp-uefi` and
`LINUX_KVM_CPUS=1|2|4|8`. Fresh KVM probes rotate CPU affinity, retaining the
existing two-VM/state/memslot contract. Strict mode checks require distinct
APIC IDs/carrier addresses and an all-CPU handoff. `LINUX_KVM_HOTPLUG_CYCLES`
adds bounded (0..100) offline/online rounds and a fresh KVM probe on each returned
AP. It defaults to zero; hotplug is a separate gate, not silently counted as
passing by the cold-boot cases.

Commands below all used `nix develop --accept-flake-config --command`:

* `cargo xtest -p nested_vmx; cargo xtest -p x86_64_hal; cargo xtest -p
  x86_uefi_loader; cargo xtest -p x86_guest_uefi_test; cargo xtest -p xtask`:
  **498 PASS / 0 FAIL / 0 SKIP** (32/62/355/10/39), including the new SMP feature
  entry in xtest.txt. `cargo xbuild x86`: **PASS**.
* `env LINUX_KVM_DIRECT_MODE=smp-uefi LINUX_KVM_CPUS=N LINUX_KVM_CYCLES=64
  bash scripts/x86_64/run-linux-kvm-test.sh`, N=1,2,4,8:
  **4 cold-boot cases PASS / 0 FAIL**, each with 64 complete KVM lifecycle
  iterations and S5 poweroff. QEMU/KVM, ordinary q35 high-PCI layout,
  `host,+vmx,-hypervisor,kvm=off`, 2 GiB. These are actual Direct L1 CPUs, not
  merely multiple L2 vCPUs on one L1 CPU.
* `cargo xrun x86 --release`: **18 PASS** (13 KVM, 5 TCG).
  `cargo xrun x86 --profile-direct --release`: **6 PASS**.
  `cargo xrun x86 --nested --release`: **15 PASS / 5 FAIL**; all 14 Direct cases
  pass, outer/reference Linux passes, and the same five outer instruction/MSR
  cases fail. No reference success replaces a Direct result.
* 8 CPUs, 64 KVM cycles, `LINUX_KVM_HOTPLUG_CYCLES=2`: **FAIL**.
  Original full-reset handling failed at round 1 / CPU1. Entering WAIT sooner
  and completing reset on SIPI improved progress but still failed at round 1 /
  CPU4, then round 1 / CPU6 on a separate run. No timeout or Linux INIT/SIPI
  timing was increased. These failures remain mandatory regressions.

The hotplug failure now has per-CPU evidence, not just a guest timeout. Existing
1344-byte diagnostic v4 records use explicit AP scope 2, with separately bounded
nonoverlapping per-CPU publications. `decode-vmx-diagnostics.py --cpu SLOT
extent|decode ...` validates the selected owner and binary scope. Its **13 host
self-tests PASS**. The existing QEMU runner stops the failed guest and captures
only these published L0 records, then preserves the original FAIL verdict.
Successful snapshots are decoded to JSON and raw counter copies are deleted.

In `bin/x86_64/smp-failure.TYP5te/cpu-{0..7}.json`, CPUs1..5 each observed
`INIT=1, SIPI=2`; failed CPU6 observed `INIT=1, SIPI=1`, with last exit INIT.
CPU7 (not yet offlined) observed `INIT=0, SIPI=1`. Thus the failed CPU received
its initial boot SIPI but not the hotplug SIPI. Earlier stopped-register capture
also showed a carrier in WAIT with reset CS:RIP, not a root exception.
The inspected local KVM `vmx_check_nested_events` / `kvm_apic_accept_events`
paths explicitly discard SIPIs while the nested hypervisor is in VMX root.
This supports an INIT/SIPI delivery-ordering race; it is not yet a fixed
lifecycle. BitVisor's direct-WAIT path likewise minimizes root work, while its
software-WAIT alternative observes startup ICR writes. No APIC/device emulation
has been added in this increment; any further intervention needs measured
justification and must preserve ordinary device/interrupt ownership.

Evidence logs: `/tmp/x86-smp-current-host-and-one-cpu.log`,
`/tmp/x86-smp-two-cpus-serial-fix.log`, `/tmp/x86-smp-four-cpus-first.log`,
`/tmp/x86-smp-eight-cpus-first.log`, `/tmp/x86-smp-bsp-full-regression.log`,
`/tmp/x86-smp-eight-cpus-hotplug-first.log`,
`/tmp/x86-smp-eight-cpus-hotplug-wait-first.log`, and
`/tmp/x86-smp-eight-hotplug-pcpu-capture.log`. A manual diagnostic invocation in
`/tmp/x86-smp-eight-pcpu-hotplug-diag.log` accidentally reused the subsequent
1-CPU UKI; it was stopped and is **not** SMP/hotplug evidence. The automated
capture run rebuilt the correct 8-CPU/hotplug UKI through the normal runner.

No Direct Windows/Hyper-V/WSL2, S3, S4, reboot matrix, Unsafe mode or physical
hardware success is inferred from these Linux cold boots.

## Qualification still required

Profile boot-option return/error/reboot qualification and failure injection;
1/2/4/8 Windows L1 CPUs and robust Linux AP hotplug/reboot;
S3/cancellation/time/NMI; Direct Hyper-V/WSL2
and S4; short/extended daily suite; measured Current/Unsafe A/B and default decision.
Existing Linux test failures in `correctness-2026-09-09.md` are not waived.

Physical-only: firmware/SMM/EC/AML quirks, genuine S0ix residency, physical device
power loss, Wi-Fi/Thunderbolt/USB-C dock resume, AP firmware ownership and battery/AC
transitions. No physical laptop has been tested in this iteration.
