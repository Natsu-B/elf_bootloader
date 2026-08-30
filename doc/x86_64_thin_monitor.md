# x86_64 thin monitor: architecture and validation status

This document records only implementation and measurements that exist on
`feat/x86-thin-monitor` as of 2026-08-30. Linux KVM and Windows boot results below come from
actual runs. Hyper-V and WSL2 pass both as direct-OVMF controls and through the trusted outer-KVM
variable-overlay path. The one-vCPU direct-VMCS monitor reaches nested VMX, but has not emitted the
Hyper-V PASS marker and its longer run ended in a watchdog bugcheck.

## Status summary

| Area | Current evidence | Not yet demonstrated |
| --- | --- | --- |
| x86-64 UEFI entry | Builds as `x86_64-unknown-uefi`; boots under QEMU/KVM + OVMF | Physical-machine boot |
| Direct-VMCS launch | One vCPU reaches VMX non-root from a runtime EFI driver; Linux crosses `ExitBootServices` while L0 retains its code, data, stack, and `HOST_CR3` pages | Private L0 GDT/IDT/TSS, SMP, and bare-metal lifetime validation |
| Linux UKI/KVM | Linux 7.1.5 loads `kvm_intel nested=0`, creates `/dev/kvm`, and runs the deterministic real-mode L2 to `KVM_EXIT_IO` | SMP, a normal distribution userspace, and a faulting or long-mode L2 |
| Direct-VMCS nested VMX | The running monitor handles VMXON, VMCLEAR, VMPTRLD, VMREAD, VMWRITE, INVEPT, INVVPID, VMLAUNCH, and VMRESUME through a direct hardware VMCS; a sparse Windows Hyper-V diagnostic crossed 524,288 balanced direct entries/exits | Non-empty MSR lists, independent CR2/XSAVE state, optional VMX controls, and SMP |
| Trusted outer KVM | A separate runtime artifact installs profile-1 variable hooks and chainloads Windows without entering project VMX; two-vCPU Hyper-V and WSL2 both passed through host KVM | Physical-machine boot, Sandbox, and VBS/HVCI |
| Direct EPT | QEMU-only 8 GiB L0 identity EPT plus a measured L1-supplied EPTP used directly for L2 | Platform-derived RAM/MMIO memory typing and bare-metal use |
| UEFI variables | In-place Runtime Services overlay for profile-private boot variables; focused OVMF profile-2 round trip, table CRC, Linux virtual-address transition, and a profile-1 Windows desktop boot measured | Cross-reboot/profile-switch persistence, Linux `efibootmgr`, and Windows BCD mutation/isolation |
| Direct-VMCS Windows | Windows 11 Enterprise Evaluation 25H2 boots and reaches the desktop through the one-vCPU monitor, including after the variable hooks were installed | Direct-monitor SMP, Sandbox, and VBS/HVCI |
| Direct Hyper-V/WSL2 controls | Without this monitor, Hyper-V passed with two and one QEMU vCPUs; a matched two-vCPU A/B failed only when QEMU hid VPID/INVVPID; WSL 2.7.11 ran a two-vCPU WSL2 BusyBox guest to `uname` and `/proc/cpuinfo` | These controls do not exercise this L0 or prove its VPID implementation |
| Direct-VMCS Hyper-V/WSL2 | The EFER.SCE corruption that caused `smss.exe` to terminate was corrected, but a later long Hyper-V run hit `DPC_WATCHDOG_VIOLATION`; WSL2 has not run through this path | Hyper-V PASS, practical nested performance, a Hyper-V VM, and a direct-VMCS WSL2 run |

Relevant commits include `af35d3b` (x86 HAL/VMX foundation), `d02d384` (four-operation
variable adapter), `767e120` (UEFI payload in VMX non-root), `a0c0bc8` (Linux UKI builder),
`1f9b344` (CPUID dispatch and outer VMRESUME), `4549597` (Linux L1 shell), `ced873a`
(long-mode walk and VMX operand decode), `8d4cfc2` (conservative nested capability mask),
`c8bd855` (runtime-resident monitor, private `HOST_CR3`, and VMXON interception), `39019e8`
(deterministic KVM L2 probe), `6a1547a` through `6a40b2b` (nested VMCS instructions and
INVEPT), `8c83a42` (direct nested entry and exit reflection), `f16b53a` (chainload Windows from
its installed ESP), `c9c73f3` (reproducible Windows monitor test), `eff5ee9` (direct Hyper-V
control), `366e4cf` (direct WSL2 control), `879a2cd` (immutable Windows backing during monitor
tests), `90bd961` (firmware Runtime Services variable overlay), and `041a1aa` (reproducible
Windows CPU-feature A/B selection).

## Architecture and late launch

The AArch64 boot paths remain separate. The x86 work is split into target-gated or independent
crates:

* `arch_hal/x86_64_hal`: CPUID/MSR/control-register access, typed physical addresses, VMCS field
  encodings, VMX instruction wrappers, and the smoke EPT builder.
* `x86_uefi_loader`: UEFI entry, COM1 diagnostics, reserved monitor pages, VMX setup, and the
  current VM-exit target.
* `x86_guest_uefi_test`: the smallest firmware payload used to prove `StartImage` in VMX
  non-root.
* `nested_vmx`: safe, heap-free policy/state for the trusted direct-VMCS path.
* `uefi_variable_overlay`: safe, heap-free profile-selection policy independent of VMX and the
  firmware ABI.

The direct-VMCS smoke uses an application copy only as a bootstrap. `cargo xbuild x86` also copies
the same PE image and changes its subsystem to EFI runtime driver with
`objcopy --subsystem=efi-rtd`. OVMF loads that second image as `EfiRuntimeServicesCode`, so Linux
preserves its pages across `ExitBootServices`:

```text
OVMF / physical UEFI
  -> BOOTX64.EFI (ordinary EFI application)
       1. LoadImage(GUESTX64.EFI), or locate installed bootmgfw.efi on another filesystem
       2. LoadImage(MONITORX64.EFI), pass the guest handle in LoadOptions
       3. StartImage(runtime monitor)
  -> MONITORX64.EFI (EFI runtime driver, VMX root)
       4. allocate one 91-page EfiRuntimeServicesData block below 8 GiB
       5. build identity EPT and an L0-owned identity HOST_CR3
       6. VMXON, then install the selected-profile Runtime Services overlay and update its CRC
       7. VMCLEAR, VMPTRLD, VMLAUNCH
  -> guest_entry (VMX non-root L1)
       8. firmware StartImage(preloaded payload, Linux UKI, or Windows boot manager)
       9. small payload: VMCALL after StartImage returns
          Linux UKI: ExitBootServices, load KVM, and continue running
          Windows: boot from its installed ESP and continue to the desktop
      10. Linux only: kvm_intel programs its VMCS and enters the real-mode L2
  -> L2 (the KVM probe)
      11. direct L1 EPT and VMCS run in hardware
      12. L0 reflects requested exits into KVM; userspace observes KVM_EXIT_IO
```

The runtime data block is 91 pages: one VMXON page, one VMCS page, ten EPT pages, one zeroed MSR
bitmap, four host-stack pages, 64 guest-stack pages (256 KiB), and ten L0 page-table pages (PML4,
PDPT, and eight page directories). Linux reports the runtime image and data as `device reserved`.
The VMCS uses the private identity table as `HOST_CR3`, rather than firmware's Boot Services page
tables. This is enough for the measured one-vCPU QEMU/Linux and QEMU/Windows paths, but the
monitor still reuses firmware GDT/IDT/TSS state and is not ready for a bare-metal fault or
interrupt.

## Threat model and TCB boundary

Linux, Linux KVM, Windows, Hyper-V, administrators, and root are trusted. The monitor is intended
to isolate accidental or policy-driven changes to boot state, not to defend L0 from a malicious
L1. In particular, a trusted L1 may construct an EPT that maps L0 memory; this is accepted to
avoid shadow EPT and nested-page-table validation.

The project-owned direct-VMCS TCB is intended to contain only:

* the x86 loader, VM-entry/exit assembly, VMCS policy, and architecture wrappers;
* the variable-overlay policy and its bounded firmware Runtime Services hooks;
* the physical UEFI firmware and the selected, explicitly trusted L1 OS/hypervisor.

There is no device model, scheduler, virtual block/network device, filesystem in L0, ACPI AML
interpreter, migration, or snapshot support. Physical devices and device firmware are expected
to remain shared. PK, KEK, db, and dbx are also shared by policy.

### Trusted outer-KVM boundary

`cargo xbuild x86` also emits `x86-uefi-kvm-loader.efi` and
`x86-uefi-kvm-monitor.efi`. This pair trusts CPU microcode, host Linux/KVM, QEMU, OVMF, and the
selected L1 OS/hypervisor. Its project-owned TCB is the UEFI bootstrap/runtime handoff and the
three profile-variable hooks; it does not mediate CPU execution or defend state from that trusted
outer stack.

The trusted runtime installs the same `GetVariable`, `SetVariable`, and `GetNextVariableName`
overlay and directly calls `StartImage`. It executes no project `VMXON` or `VMLAUNCH`: KVM remains
L0, Windows/Hyper-V is L1, and KVM handles Hyper-V's nested VMX. The default direct-VMCS artifacts
remain available for the bare-metal-oriented monitor work.

| Release runtime artifact | PE size | `.text` size | Decoded VMX instruction sites |
| --- | ---: | ---: | ---: |
| `x86-uefi-monitor.efi` | 81,408 bytes | 69,273 bytes | 303 |
| `x86-uefi-kvm-monitor.efi` | 22,016 bytes | 16,825 bytes | 0 |

These figures come from `cargo xbuild x86 --release`, `stat`, and GNU `objdump` 2.44; the VMX
count includes decoded `VMCALL`, `VMREAD`, and `VMWRITE` sites. The trusted PE is 73.0% smaller
overall and 75.7% smaller in `.text`. These measurements show the active implementation
reduction; they are not a formal TCB proof.

## Direct EPT and its current ceiling

There are two distinct EPT uses:

1. The current QEMU smoke builds ten EPT paging pages and maps physical `[0, 8 GiB)` identically
   with 2 MiB read/write/execute leaves. `[0, 2 GiB)` and `[4 GiB, 6 GiB)` are write-back buckets
   for the 4 GiB test RAM; `[2 GiB, 4 GiB)` and `[6 GiB, 8 GiB)` are uncacheable buckets for PCI
   MMIO and firmware windows. The loader rejects its reserved block, entry/exit code, or CR3 if
   an address is at or above 8 GiB.
2. The trusted nested path puts L1's EPTP directly into the hardware VMCS for L2.
   Because L1 physical addresses are treated as machine physical addresses, there is no EPT12 x
   EPT01 composition, shadow EPT, or EPT02 cache.

The first map is only a bounded QEMU layout, not a general firmware/OS memory map. The earlier
1 GiB all-write-back map stopped at the local-APIC GPA `0xFEE00000`; expanding the map allowed the
UKI to enter the kernel and reach its initramfs shell. A real launch must cover the required
physical address width and derive suitable RAM/MMIO cache types from platform state. The current
code therefore cannot be used for arbitrary bare-metal MMIO or for firmware allocations above
the limit. The Windows QEMU harness constrains q35's 64-bit PCI hole to 1 GiB and tells OVMF to
use the same aperture, keeping its high MMIO inside the existing uncacheable EPT buckets. Those
knobs describe only this QEMU layout; they do not make the fixed map suitable for bare metal.

The Linux 7.1.5 KVM probe supplied an EPTP that the monitor used directly. L1 issued global and
single-context INVEPT before the direct entry; L0 forwarded both to hardware. The conservative
capability mask retains only four-level EPT, write-back EPTP, and the supported invalidation
types. The accepted threat model permits L1's EPT to map L0 memory.

## Trusted direct-VMCS nesting model

The path follows BitVisor's unsafe/trusted nesting idea: L1's VMCS page is also the
hardware VMCS instead of copying a software VMCS12 into a separately synthesized VMCS02. The CPU
remains the authority for VMCS field and VM-entry validation. `nested_vmx` currently implements
the state and policy pieces:

* per-vCPU VMXON/current-VMCS/clear-or-launched state;
* exact VMsucceed, VMfailInvalid, and VMfailValid CF/ZF transformations;
* conservative KVM-required allowed-one controls and capability masking;
* provenance for `effective = l1_requested | l0_required`, so an L0-only exit is not reflected
  merely because L0 forced its control bit;
* a 30-field direct-VMCS patch manifest: 26 host-state fields plus both address/count pairs for
  the VM-exit MSR store and load lists.

The running one-vCPU monitor applies the conservative `IA32_VMX_*` masks, keeps hardware
CR4.VMXE set while exposing L1's requested value through the VMCS read shadow, and handles VMXON,
VMCLEAR, VMPTRLD, register-form VMREAD/VMWRITE, INVEPT, VMLAUNCH, and VMRESUME. The VMXON path
checks virtual CR4.VMXE, CPL, VMX fixed bits, operand encoding, L1 long-mode page translation,
physical width/alignment, and the hardware revision ID. Nested instructions return architectural
VMsucceed/VMfail flags, and hardware validates direct VMCS operations.

Before an L2 entry, L0 saves the direct VMCS host fields and replaces them with VMCS01's L0 CRs,
segment bases, descriptor-table bases, SYSENTER state, PAT/EFER, RSP/RIP, and supported optional
state. Hardware then lands at L0 on an L2 exit. L0 restores the direct fields, reloads VMCS01,
applies the saved L1 host state as its guest state, and resumes Linux KVM at L1's host RIP/RSP.
The measured run reflected external-interrupt exits, an EPT violation, and the final I/O exit;
KVM resolved the EPT violation and userspace received `KVM_EXIT_IO`.

### VM-exit MSR list ceiling

Passing L1's VM-exit MSR lists through unchanged could load L1 host MSRs before L0 runs. The
current measured KVM VMCS uses zero entry-load, exit-store, and exit-load counts. L0 verifies that
condition and patches the two exit address/count pairs to zero while L2 runs. It stops instead of
entering an L2 with non-empty lists or unsupported optional host-state controls. Bounded L0 MSR
mirrors are deferred until a measured workload requires them. VMCS shadowing, VPID, APICv,
posted interrupts, VMFUNC, PML, TSC scaling, and eVMCS remain unadvertised.

## UEFI variable profile model

`uefi_variable_overlay` treats the following exact names in the EFI global-variable namespace as
profile-private:

```text
BootOrder  BootNext  BootCurrent  Boot####
DriverOrder  Driver####
```

`####` is exactly four uppercase hexadecimal digits. A private logical key is stored under the
monitor vendor GUID with the UTF-16 backend name `P<8-HEX-DIGIT-PROFILE>:<logical-name>`. All
other namespaces and names, including PK/KEK/db/dbx, stay shared.

The ABI-independent adapter defines the four-operation policy from UEFI 2.11:

| Operation | Profile behavior |
| --- | --- |
| `GetVariable` | Map private names to the selected profile; otherwise pass the key through; preserve required-buffer-size behavior |
| `SetVariable` | Apply the same mapping; an empty data slice is deletion |
| `GetNextVariableName` | Rebuild a bounded snapshot, hide raw private/other-profile/internal keys, and expose selected keys under logical names with a terminating NUL |
| `QueryVariableInfo` | Pass through physical-store capacity for the requested attributes |

The runtime monitor installs bounded wrappers for `GetVariable`, `SetVariable`, and
`GetNextVariableName` directly into OVMF's live `EFI_RUNTIME_SERVICES` table. It recomputes the
table CRC with firmware `CalculateCrc32`; `QueryVariableInfo`, `UpdateCapsule`,
`QueryCapsuleCapabilities`, Secure Boot variables, and every unrelated service remain the original
firmware entry points. A virtual-address-change event applies the original firmware
`ConvertPointer` to the three saved functions that are no longer present in the public table. If
VM launch returns or fails, installation is rolled back, including the table entries, CRC, event,
and any Memory Attributes Table edits.

The bootstrap selects a stable profile from the chainload source and passes it to the runtime
driver in a small `repr(C)` handoff: staged `GUESTX64.EFI` (the test payload or Linux UKI) selects
profile 2, while an installed Windows `bootmgfw.efi` selects profile 1. This is source-based
selection, not yet a partition-GUID policy or an interactive boot-profile selector.

`GetNextVariableName` streams the firmware enumeration through a fixed 2,048-`CHAR16` scratch
buffer. It hides physical profile keys, converts only the selected profile back to logical names,
and returns `EFI_DEVICE_ERROR` if the firmware reports a longer name. UEFI normally forbids
reentering `GetNextVariableName` or its variable-services reentry group while a call is busy, so
the scratch buffer uses one blocking spin lock. The UEFI MCE/INIT/NMI reentry exception is not
supported by this single-vCPU implementation; such reentry would deadlock and requires per-CPU or
otherwise reentrant scratch before bare-metal use.

The focused OVMF payload deletes both test backend keys, seeds profile 1's raw `BootOrder`, then
uses the installed profile-2 hook to verify logical `SetVariable`/`GetVariable`, direct profile-2
backend storage, profile-1 noninterference, filtered enumeration, pass-through
`QueryVariableInfo`, and the live Runtime Services CRC. It prints:

```text
thin-hv: uefi variable overlay PASS
```

This is a real firmware-backend test but deliberately one boot with profile 2; deleting the two
keys makes it deterministic, so it does not prove cross-reboot persistence or a profile switch.
The backend uses nonvolatile OVMF variables and therefore has persistent wiring, but a two-boot
Windows/Linux isolation test remains required. Linux has crossed its runtime virtual-address
transition and reached the KVM L2 marker with the hooks installed. A later monitor run selected
profile 1, patched one MAT descriptor, and reached the Windows desktop with the hooks installed.
That boot proves hook lifetime and chainload compatibility, not Windows boot-variable mutation or
cross-profile isolation.

`BootNext` is currently only mapped and stored: the physical firmware has already selected and
started the monitor, so the monitor does not consume a profile's `BootNext` or resolve its
`Boot####` target on the next reset. `BootCurrent` is mapped like the other names but is not
synthesized from the selected chainload target, so it is normally absent unless a backend value is
seeded. Authenticated variable payloads bind the original variable name and GUID; changing both for
backend storage means name-bound authenticated writes are unsupported. The boot variables under
this policy are normally unauthenticated, but this remains an explicit ABI ceiling.

## Nix build and test commands

Enter the pinned development environment for every command:

```sh
nix develop --accept-flake-config
```

The shell supplies nightly Rust with the AArch64 and x86 UEFI targets, QEMU, OVMF, swtpm,
binutils, `cpio`, `file`, `gzip`, a static BusyBox, and the systemd x86 EFI stub. It exports
`OVMF_CODE`, `OVMF_VARS`, `BUSYBOX_STATIC`, and `LINUX_EFI_STUB`.

Build both direct-VMCS and trusted outer-KVM bootstrap/runtime pairs, plus the test payload, under
ignored `bin/x86_64/`:

```sh
cargo xbuild x86
```

Run the small UEFI VMX smoke:

```sh
cargo xrun x86
```

Run the architecture-independent checks directly:

```sh
cargo test -p x86_64_hal
cargo test -p nested_vmx
cargo test -p uefi_variable_overlay
cargo fmt --all -- --check
```

On the current branch, AArch64 `cargo xbuild`, all 15 `cargo xtest -t std` entries, the unit-test
plan, the UEFI/QEMU `virtio_blk_modern` test, and the U-Boot/QEMU `stage1_translation` test pass.
A full `cargo xtest` stopped before executing tests because its `sudo -v` preflight could not
authenticate in the non-interactive session.

## QEMU/KVM + OVMF UEFI smoke

`scripts/x86_64/run-uefi-smoke.sh` creates a fresh copy of the OVMF variable template, stages the
loader as `EFI/BOOT/BOOTX64.EFI`, the runtime copy as `EFI/BOOT/MONITORX64.EFI`, and the payload
as `EFI/BOOT/GUESTX64.EFI`, then runs one vCPU:

```text
-machine q35,accel=kvm
-cpu host,+vmx,-hypervisor
-smp 1
```

The baseline small payload reached non-root firmware `StartImage` and VMCALL. The CPUID handler
committed in `1f9b344` was also measured with the small payload, which intentionally issues CPUID
twice:

```text
thin-hv: uefi entry
thin-hv: CPUID VMX=1
thin-hv: loading runtime monitor
thin-hv: uefi entry
thin-hv: runtime monitor active
thin-hv: variable overlay profile=2 mat_patches=1
thin-hv: guest uefi payload
thin-hv: VMEXIT cpu=0 level=1 reason=0xa qualification=0x0 ... instruction_len=2
thin-hv: guest cpuid vmx=1 hypervisor=0
thin-hv: uefi variable overlay PASS
thin-hv: VMEXIT cpu=0 level=1 reason=0xa qualification=0x0 ... instruction_len=2
thin-hv: VMEXIT cpu=0 level=1 reason=0x12 qualification=0x0 ... instruction_len=3
thin-hv: vmx guest PASS start_image_status=0x0
```

This validates the runtime-driver handoff, real OVMF variable wrappers, Runtime Services CRC, and
CPUID filtering for the small payload: leaf 1 exposes VMX and clears the hypervisor-present bit,
while the Hyper-V-reserved CPUID range is zeroed. Linux KVM initialization and the runtime virtual
address transition are measured separately below. The trusted artifact delegates VMX exposure to
the outer KVM/QEMU stack and passes this payload with nested VMX exposed:

```sh
env X86_MONITOR_IMAGE="$PWD/bin/x86_64/x86-uefi-kvm-monitor.efi" \
  X86_RETURN_MARKER='thin-hv: trusted outer KVM guest PASS' \
  X86_UEFI_CPU='host,+vmx,-hypervisor,kvm=off' \
  scripts/x86_64/run-uefi-smoke.sh \
  bin/x86_64/x86-uefi-kvm-loader.efi bin/x86_64/x86_guest_uefi_test.efi
```

Run through `nix develop`: its pinned OVMF 202505 exercised one Memory Attributes Table edit in
the direct trace, while QEMU's separately bundled OVMF happened to publish the measured image
allocation without RO/XP and therefore logged `mat_patches=0`.

## Linux UKI and direct OVMF control test

Build the test UKI from the running NixOS kernel, static BusyBox initramfs, systemd EFI stub, and
`scripts/x86_64/linux-l1-init`:

```sh
scripts/x86_64/build-linux-uki.sh
```

The builder writes `bin/x86_64/linux-l1.efi`, verifies that it is an x86-64 EFI application, and
prints its exact size for the current kernel and module closure.

The direct-OVMF control can be reproduced without the monitor as follows. Exit status 124 is
expected when `timeout` stops the interactive shell.

```sh
nix develop --accept-flake-config --command bash -c '
set -eu
stage=bin/x86_64/linux-direct
mkdir -p "$stage/esp/EFI/BOOT"
install -m 0644 bin/x86_64/linux-l1.efi "$stage/esp/EFI/BOOT/BOOTX64.EFI"
install -m 0600 "$OVMF_VARS" "$stage/OVMF_VARS.fd"
timeout --foreground --kill-after=2s 12s qemu-system-x86_64 \
  -machine q35,accel=kvm -cpu host,+vmx,-hypervisor -smp 1 -m 768M \
  -nodefaults -display none -monitor none -serial stdio -no-reboot -no-shutdown \
  -drive "if=pflash,format=raw,readonly=on,file=$OVMF_CODE" \
  -drive "if=pflash,format=raw,file=$stage/OVMF_VARS.fd" \
  -drive "if=none,id=esp,format=raw,file=fat:rw:$stage/esp" \
  -device virtio-blk-pci,drive=esp
'
```

The control reached the initramfs shell and showed that the outer KVM supplied VMX:

```text
thin-hv: linux L1 /proc/cpuinfo vmx=1
thin-hv: linux L1 shell
/ #
```

This separates UKI construction and the Linux image itself from failures in this monitor.

## Monitor persistence, Linux KVM, and L2 execution

Loading `kvm_intel` forced the first late VM exit after Linux had reclaimed Boot Services memory
and exposed two L0 lifetime bugs. First, an ordinary EFI application's code pages were reclaimed,
so its saved `HOST_RIP` no longer contained monitor code. Loading the monitor a second time as an
EFI runtime driver keeps its PE sections reserved. Second, the VMCS still used firmware's
`HOST_CR3`; Linux reclaimed the page-table pages behind it. The ten L0 page-table pages in the
91-page runtime allocation provide an L0-owned four-level identity map and are installed as
`HOST_CR3` before launch.

Linux also performs the EFI runtime virtual-address transition. Post-transition VM-exit logging
through `core::fmt` followed relocated formatting metadata while L0 intentionally continued under
its physical identity map. The VM-exit path now writes fixed byte strings and hexadecimal fields
directly to COM1. Pre-launch firmware diagnostics may still use `core::fmt`; the persistent
VM-exit path does not.

The first variable-hook run reached Linux's virtual-address transition but faulted when Linux
called the relocated `SetVariable` wrapper: the monitor instruction page was mapped NX. OVMF's
`InsertImageRecord` stops adding the image-properties records used to split the MAT after
`EndOfDxe`, while this monitor is loaded later by BDS. Runtime relocation registration still
succeeds, but the Memory Attributes Table fallback labels the otherwise valid
`EfiRuntimeServicesCode` allocation RO+XP. Immediately before VM launch, the monitor now validates
MAT version, descriptor size/count and checked ranges, requires the complete monitor image to be
covered by runtime-code descriptors, and clears only RO/XP on descriptors intersecting that late
image. It leaves all other MAT entries untouched. The trusted-L1 threat model accepts that the
measured containing allocation is RWX. Rollback restores those fields only if the configuration
table still points to the same MAT buffer.

With the Nix-shell OVMF 202505, one descriptor was changed (`mat_patches=1`); Linux then completed
`SetVirtualAddressMap`, loaded `kvm_intel`, and reached `thin-hv: linux L1 L2 KVM PASS` in the same
run. This workaround is deliberately OVMF-specific: a firmware that splits the loaded image into
`EfiRuntimeServicesData`, uses a protected or differently structured MAT, or needs more than 16
intersecting descriptors is rejected. A later runtime-code/data allocation could make OVMF
regenerate the MAT and lose the edit. No such allocation was observed after installation in the
measured boot; no extra ExitBootServices hook is added until a real workload demonstrates that
need.

After `cargo xbuild x86` and `scripts/x86_64/build-linux-uki.sh`, the measured L2 run is reproduced
with this command from the Nix development shell:

```sh
env X86_RETURN_MARKER= \
  X86_GUEST_MARKER='thin-hv: linux L1 L2 KVM PASS' \
  X86_UEFI_TIMEOUT_SECONDS=25 \
  X86_UEFI_MEMORY=768M \
  scripts/x86_64/run-uefi-smoke.sh \
  bin/x86_64/x86-uefi-loader.efi \
  bin/x86_64/linux-l1.efi
```

The initramfs loads the generic KVM module, runs `modprobe kvm_intel nested=0`, and executes the
freestanding `/bin/kvm-probe`. That probe creates a VM and one vCPU through `/dev/kvm`, maps one
page, enters a real-mode L2 that loads DX with `0xe9`, performs `outl %eax, %dx`, and requires
userspace to receive the matching `KVM_EXIT_IO`. The ignored capture
`bin/x86_64/l2-kvm-pass.log` records Linux 7.1.5 and the following direct-entry sequence:

```text
thin-hv: linux L1 /proc/cpuinfo vmx=1
thin-hv: linux L1 kvm_intel=1
thin-hv: linux L1 /dev/kvm=1
thin-hv: L1 VMCLEAR region=0x000000002e4dc000
thin-hv: L1 VMPTRLD region=0x000000002e4dc000
thin-hv: L1 INVEPT kind=0x0000000000000002
thin-hv: L1 INVEPT kind=0x0000000000000001
thin-hv: L1 VMLAUNCH direct=0x000000002e4dc000
thin-hv: L1 L2 EXIT reason=0x0000000000000001
thin-hv: L1 VMRESUME direct=0x000000002e4dc000
thin-hv: L1 L2 EXIT reason=0x0000000000000030
thin-hv: L1 L2 EXIT qualification=0x0000000000000184
thin-hv: L1 VMRESUME direct=0x000000002e4dc000
thin-hv: L1 L2 EXIT reason=0x000000000000001e
thin-hv: L1 L2 EXIT qualification=0x0000000000e90003
thin-hv: linux L1 L2 KVM PASS
thin-hv: linux L1 shell
```

Exit reason `0x1` is an external interrupt, `0x30` is an EPT violation resolved by KVM, and
`0x1e` is the expected I/O exit. This proves the measured chain `host KVM -> this L0 -> Linux
7.1.5/kvm_intel -> real-mode L2`, including direct L1 EPT, nested VMRESUME, and delivery of
`KVM_EXIT_IO` to L1 userspace. `nested=0` only prevents L1 KVM from advertising VMX to L2.

## Windows, Hyper-V, and WSL2 status

The Microsoft-hosted Windows 11 Enterprise Evaluation 25H2 English ISO was downloaded as ignored
`bin/x86_64/windows/win11-enterprise-eval-25h2-en-us.iso`, verified as SHA-256
`a61adeab895ef5a4db436e0a7011c92a2ff17bb0357f58b13bbc4062e535e7b9`, and installed to an
ignored 80 GiB sparse raw `bin/x86_64/windows/windows.raw`. Both the unattended install and a
subsequent direct OVMF boot reached the Windows desktop. The measured monitor boot is reproduced
after `cargo xbuild x86` with:

```sh
scripts/x86_64/windows/windows-test.sh monitor
```

Monitor mode stages only `BOOTX64.EFI` and `MONITORX64.EFI` on its loader ESP. If staged
`GUESTX64.EFI` is absent, the loader enumerates non-parent `SimpleFileSystem` handles and uses a
complete device path to load `\EFI\Microsoft\Boot\bootmgfw.efi` from the first matching installed
ESP. This preserves the Windows boot manager's actual device handle and file path. The current
first-match rule is sufficient for one Windows installation; profile partition-GUID selection is
needed if multiple Windows ESPs matter.

The monitor test creates fresh `monitor-vars.fd` from the OVMF template so firmware boot entries
cannot bypass the loader, then runs q35 with `pci-hole64-size=1G` and OVMF
`X-PciMmio64Mb=1024`. This fresh variable store is a test-harness boot control, not the profile
variable overlay described above. QEMU's temporary disk snapshot keeps `windows.raw` immutable
because the persistent Hyper-V qcow2 uses it as a backing file. The loader selected profile 1,
installed the Runtime Services hooks, logged `mat_patches=1`, and reached the desktop marker in
the same measured run.

The ignored evidence captures are:

* `bin/x86_64/windows/direct-install-pass.log` and
  `bin/x86_64/windows/direct-boot-pass.log`: `thin-hv: windows desktop`;
* `bin/x86_64/windows/monitor-variable-overlay-pass.log`, SHA-256
  `667a88f1a6272b166b4316b7c30e3f6c0ee2ff0789cb20c2fbdb1c103b54a690`: monitor startup,
  `variable overlay profile=1 mat_patches=1`, and Windows VMX capability reads;
* `bin/x86_64/windows/monitor-variable-overlay-desktop-pass.log`, SHA-256
  `4709211153cfb7d89cd549a41d0299e02b6ebcbec5fd232c32ef607e7cc69d3b`: the
  `thinhvwindowsdesktop` COM2 marker from that post-hook run;
* `bin/x86_64/linux-l2-regression-pass.log`: `thin-hv: linux L1 L2 KVM PASS` after the Windows
  loader changes.

These results prove `host KVM -> this L0 -> Windows desktop` with one vCPU after hook
installation. They do not prove that Windows changed an overlaid boot variable or that a second
profile remained isolated.

### Direct Hyper-V and WSL2 controls

The Hyper-V and WSL modes boot the persistent `windows-hyperv.qcow2`, its own OVMF variables, and
its own TPM state directly under QEMU/KVM; `BOOTX64.EFI` and this monitor are absent. They are
control experiments for the Windows image and outer KVM, not evidence that nested VMX works
through this L0:

```sh
scripts/x86_64/windows/windows-test.sh hyperv
scripts/x86_64/windows/windows-test.sh download-wsl
scripts/x86_64/windows/windows-test.sh wsl
```

The default two-vCPU Hyper-V run enabled `Microsoft-Hyper-V`, set
`hypervisorlaunchtype Auto`, rebooted, and reported:

```text
thin-hv: windows hyperv feature=Enabled present=1 vmms=Running event2=1
thin-hv: windows hyperv PASS
```

A separate one-vCPU direct-OVMF A/B run produced the same result. The captures
`hyperv-direct-pass.log` and `hyperv-direct-1vcpu-pass.log` both have SHA-256
`cb9000c74c33d930ef842631e6c5a84d2a3d2e51043c86cc788f0654c1400fb6`. This rules out vCPU
count alone as the explanation for the monitor-mediated failure below.

Commit `041a1aa` exposes the QEMU CPU string as `WINDOWS_CPU`. A matched direct-OVMF test reused
the same two-vCPU Hyper-V qcow2, OVMF variable store, and TPM state. The default
`host,+vmx,-hypervisor` baseline emitted the Hyper-V PASS marker in under 60 seconds. Changing
only the CPU string to `host,+vmx,-hypervisor,-vmx-vpid` booted Windows, emitted
`thin-hv: windows hyperv enable begin`, rebooted into Windows again, and then timed out at 360
seconds without the PASS marker. The ignored A/B evidence is:

| Capture | SHA-256 |
| --- | --- |
| `bin/x86_64/windows/hyperv-vpid-baseline-desktop.log` | `cb9000c74c33d930ef842631e6c5a84d2a3d2e51043c86cc788f0654c1400fb6` |
| `bin/x86_64/windows/hyperv-vpid-baseline-serial.log` | `54478a9dc16aa0c77bf969710e367e6f16800063764edba61fd013b344075c29` |
| `bin/x86_64/windows/hyperv-vpid-baseline-qemu.log` | `f0d0033868787d250289d73ae3e7702354f0e3fbd2f982a164e5505827480a9b` |
| `bin/x86_64/windows/hyperv-no-vpid-desktop.log` | `cd6e58617791cbc3bb6f4cb2ae6e56c0e8fc889bd80885c1541c4b9bda22d2e6` |
| `bin/x86_64/windows/hyperv-no-vpid-serial.log` | `e46fe4794e55f443bf3ce1bb12f5b42fc3ea7a4c480cda2cf55e659d423efc6a` |
| `bin/x86_64/windows/hyperv-no-vpid-qemu.log` | `2f291ce4de39860931f52d05106ca98ede199cec326773f1f4dec32a08b1db53` |

QEMU's `-vmx-vpid` dependency group also removes the associated INVVPID exposure. The measured
inference is therefore that Hyper-V requires the VPID/INVVPID capability group in this setup; it
does not yet prove that advertising and forwarding those facilities in this L0 is sufficient or
correct.

The WSL control verified Microsoft's WSL 2.7.11.0 MSI (SHA-256
`a611ddacee689d2fb1fb5319e58af7f3998864d86cdce632eadd8e61614a0f9d`), enabled
`VirtualMachinePlatform`, and imported the deterministic BusyBox rootfs as `ThinHvTest` with WSL
version 2. The final clean run reported Linux `6.18.33.2-microsoft-standard-WSL2`, processors 0
and 1, `thin-hv-wsl2-guest-ok`, and `thin-hv: windows wsl2 PASS`. Its ignored artifacts are:

* `bin/x86_64/windows/wsl-desktop-serial.log`, SHA-256
  `111401172fdaf9f233348a7d220d59396241a900faa41ea9f72c23ef16e3d343`;
* `bin/x86_64/windows/hyperv-media/thin-hv-wsl-rootfs.tar`, SHA-256
  `ea99179cda2aed59ae7d5018005a95490f627cd7ade94f9d4c5c0f1abd8cd230`;
* media stamp `3d0136133c2beda2c71788709efa297c6025aab0b89b1cb319ae10dcc3f0fa52`.

### Trusted outer-KVM Hyper-V and WSL2

The trusted modes reuse the Hyper-V-enabled image and TPM state, but boot through the small
profile-overlay artifacts with two vCPUs:

```sh
scripts/x86_64/windows/windows-test.sh trusted-kvm-hyperv
scripts/x86_64/windows/windows-test.sh trusted-kvm-wsl
```

The Hyper-V run emitted `thin-hv: windows hyperv PASS`, selected overlay profile 1, and contained
no `thin-hv: L1 VMLAUNCH direct=` marker. Six counterbalanced same-baseline pairs with fresh qcow2,
UEFI-variable, and TPM children averaged 23.601 seconds direct and 22.933 seconds trusted before
the latest startup trimming. Six post-change pairs averaged 23.430 and 23.370 seconds. The paired
trusted-minus-direct 95% intervals were [-1.726, +0.391] and [-0.803, +0.681] seconds respectively,
so neither trusted overhead nor an end-to-end speedup was resolved.

The trusted WSL2 run passed with WSL 2.7.11.0, kernel `6.18.33.2-2`, and both guest processors.
Including its setup reboot, it reached `thin-hv: windows wsl2 PASS` in 216.732 seconds. COM1 showed
two boot epochs, each with `thin-hv: trusted outer KVM runtime active profile=1`, and neither
contained a direct `VMLAUNCH` marker. `qemu-img check` found no errors afterward.
These results validate the UEFI overlay in front of KVM's ordinary nested virtualization; they do
not exercise the direct-VMCS monitor below.

The exact commands, timestamps, environment, artifact hashes, and coherent Windows log hashes are
recorded in the [2026-08-30 validation manifest](evidence/x86_64/validation-2026-08-30.md).

### Direct-VMCS Hyper-V boundary

`scripts/x86_64/windows/windows-test.sh monitor-hyperv` boots the Hyper-V-enabled qcow2 through
the one-vCPU monitor. A fresh direct control reached `thin-hv: windows hyperv PASS` in about 210
seconds; a separate `-smp 1` direct control reached the same marker in 60 seconds, excluding the
monitor's one-vCPU limit as the cause of the nested timeout.

The pre-fix monitor run crashed with bugcheck `0xEF`. Its active dump has SHA-256
`b02e06df72ce806c7e1657052fb0bb2e8c4b43176d691179e8af48ffeb8c8066`. The terminating process
was `smss.exe` with exit status `0xC000001D` (`STATUS_ILLEGAL_INSTRUCTION`); its preserved user RIP
was `ntdll!NtQueryVirtualMemory+0x12`, on intact `0f 05` (`SYSCALL`) bytes. Sparse VMCS tracing
then showed direct exit and entry EFER controls all clear, active EFER `0xd00` after the carrier
exit, and more than 524,288 balanced direct entries/exits. The carrier had loaded its L0 host EFER
on the intercepted L1 `VMLAUNCH`, replacing the live L1 value `0xd01` with `0xd00`. The all-clear
direct entry correctly inherited that bad value, so `SYSCALL` raised #UD because SCE was clear.

The fix leaves carrier `VM_EXIT_SAVE_IA32_EFER` and `VM_ENTRY_LOAD_IA32_EFER` enabled but removes
`VM_EXIT_LOAD_IA32_EFER`. The carrier exit therefore saves L1's `0xd01` into `GUEST_IA32_EFER`
without replacing the live value, and carrier resume reloads that saved value. This tuple was also
checked against the exact Linux 7.1.5 KVM nested-VMX implementation used by the host. A fresh
post-fix retest crossed the old crash boundary and ran for the full 1200-second harness limit with
one `Boot0002`, one `thin-hv: runtime monitor active`, no BSOD, no monitor fault, and an animating
Windows `Please wait` screen. It did not emit `thin-hv: windows hyperv PASS`, so this is evidence
that the specific EFER.SCE #UD path was corrected rather than a nested Hyper-V pass. A remaining
measured cost is nested-under-KVM throughput; the reflected exit path still materializes the L1
host state with 54 carrier VMWRITEs per direct exit.

A longer post-fix run made that cost concrete: 19 of 20 sampled register snapshots were in this
L0, while every sampled EFER remained `0xd01`. The reset at 12:42 was not the planned feature-enable
reboot; the direct control had already reported Hyper-V enabled and running at 09:55, and
`monitor-hyperv` does not invoke `hyperv-enable.ps1`.

The reset produced a new kernel dump with SHA-256
`4be11f89920c0b7dcdfd698a56e115279268725a66aee8dad9ba32f87b8aaa6f`. It records bugcheck
`0x133` (`DPC_WATCHDOG_VIOLATION`), parameters
`(1, 0x1e00, 0xfffff806899c43b0, 0)`, system time `2026-08-30 12:41:53.719`, and uptime
`1:06:43.938`; the next UEFI epoch began at 12:42:31-32. Parameter 1 equal to 1 denotes cumulative
excessive time at `DISPATCH_LEVEL` or above. That is consistent with the extreme reflected-exit
slowdown, but does not identify a responsible driver. Post-crash WER data also records
`LogonUI.exe` / `Windows.UI.Logon.dll` failing with `0xc0000005`, so the later `Please wait` screen
was a logon failure rather than evidence of normal forward progress. Its temporary dump has
SHA-256 `5a66631299619ddd5dfd0557278dd993567c425f3c342c0b9a1eab52ce750fd5`.

A mountless independent recheck is tracked in the [forensic transcript](evidence/x86_64/direct-vmx-0x133-forensic.txt).
Its exact image-specific tool is the [read-only verifier](../scripts/x86_64/windows/direct-vmx-0x133-readonly.py).

Reproduce from a direct-PASS work directory without mutating that control:

```sh
direct=/path/to/direct-pass-workdir
WINDOWS_TEST_DIR="$direct" WINDOWS_HYPERV_TIMEOUT_SECONDS=1200 \
  scripts/x86_64/windows/windows-test.sh hyperv

monitor=$(mktemp -d /tmp/thin-hv-monitor.XXXXXX)
cp -a --reflink=auto --sparse=always "$direct/." "$monitor/"

WINDOWS_TEST_DIR="$monitor" WINDOWS_HYPERV_TIMEOUT_SECONDS=1200 \
  scripts/x86_64/windows/windows-test.sh monitor-hyperv
```

Ignored evidence copies are under `bin/x86_64/windows/evidence/`. The fresh direct desktop log has
SHA-256 `9e598901c04db859c7cf03824092c84213da20167768293dda2c4c338b420e4c`; the separate one-vCPU
control log has SHA-256 `cb9000c74c33d930ef842631e6c5a84d2a3d2e51043c86cc788f0654c1400fb6`.
The fixed monitor serial and QEMU logs have SHA-256
`2cc19bcb039519e90840456ebb6ca55ad91def75ff28a7d144c09182beaaaec1` and
`be134a8e4e6446f5db663641d898e53d1688d5258431935ea9e62718033faaf1`; the 1080-second screen
capture has SHA-256 `d0152df71706b68d0735a715ee31097443c50725427c013eab7e0ad847d12d2a`,
and `smss_context_probe.py` has SHA-256
`37208d56f70ad6942528ee645caa603b9e752b974f856aeed275157223b55284`.
No Hyper-V VM or WSL2 guest has run through this direct-VMCS L0; the trusted results above
bypass it.

The direct-VMCS path contains only the conservative standard-VMX policy needed to begin those tests.
It does not implement or advertise Hyper-V CPUID leaves, SynIC, VP Assist Page, enlightened VMCS,
or enlightened VM-entry. The design goal remains to expose bare-metal-style VMX (`VMX=1`,
`hypervisor-present=0`) to trusted Windows. Windows Sandbox and VBS/HVCI remain untested in both
configurations.

## Known limitations and bare-metal boundary

* The direct-VMCS backend is Intel VMX only; AMD SVM is out of scope.
* The direct-VMCS backend supports one vCPU only. Nested state and the active direct run use global
  storage; there is no AP startup, x2APIC policy, APICv, or posted-interrupt support. Move both
  state objects to per-pCPU storage before direct-monitor SMP. The trusted outer-KVM path instead
  delegates SMP to KVM and passed with two vCPUs.
* The direct smoke EPT covers only the first 8 GiB and uses QEMU-specific fixed WB/UC buckets. It
  is not safe for a general bare-metal RAM/MMIO layout. The q35/OVMF 1 GiB PCI-hole settings are
  also QEMU-only.
* The direct runtime PE sections and 91-page data block survive Linux `ExitBootServices`, and the
  VMCS uses an L0-owned `HOST_CR3`. L0 still reuses firmware GDT/IDT/TSS state; private descriptor
  tables and fault handlers are required before bare-metal use.
* Direct `MONITORX64.EFI` is currently the application PE copied with its subsystem changed to EFI
  runtime driver. Its virtual-address-change handler converts the three saved firmware variable
  entry points, but the monitor has no self-relocated resident core. The OVMF-specific MAT
  workaround requires complete `EfiRuntimeServicesCode` coverage and may be lost if a later
  runtime allocation regenerates the table. Raw VM-exit logging works across the measured Linux
  relocation and the post-hook Windows boot, but this is not a general bare-metal firmware
  runtime-PE solution.
* The bootstrap finds `\EFI\BOOT\MONITORX64.EFI` on its own firmware device handle and chainloads
  Windows from the first other filesystem containing `bootmgfw.efi`. Multiple Windows installs
  need profile-owned ESP selection instead of firmware enumeration order.
* The measured direct nested path handles VMXON, VMCLEAR, VMPTRLD, register-form VMREAD/VMWRITE,
  INVEPT, INVVPID, VMLAUNCH, and VMRESUME. Memory-form VMREAD/VMWRITE, VMXOFF, optional VMX
  controls, and VMX in L2 are not supported.
* Direct entry currently requires zero VM-entry MSR-load, VM-exit MSR-store, and VM-exit MSR-load
  counts. Add bounded L0-owned mirrors before accepting non-empty lists.
* L0 shares CR2 and extended register state with its trusted one-vCPU L1 around a direct run.
  Add independent CR2/XSAVE switching before accepting untrusted, SMP, or workloads that require
  those states to remain private; the measured Hyper-V path has already entered L2 under this
  trusted-state ceiling.
* Through the direct-VMCS L0, Linux KVM has run one VM/vCPU to `KVM_EXIT_IO`; an L2 Linux kernel,
  KVM SMP, and sustained or device-heavy workloads have not run.
* Hyper-V through the direct-VMCS L0 has not reached PASS. The EFER.SCE fix removed the earlier
  `0xEF`, but the long run ended in `0x133` after extreme reflection overhead; WSL2 has not run
  through that L0. Hyper-V and WSL2 both pass through the trusted outer-KVM overlay, which provides
  no CPU, memory, or device isolation from Linux/KVM, QEMU, or OVMF.
* The profile variable hooks have one real OVMF profile-2 round-trip/enumeration/CRC test and use
  the firmware's nonvolatile backend. That focused payload deletes both test keys, so it does not
  prove cross-reboot persistence or switching between profiles. A profile-1 Windows desktop boot
  proves hook lifetime but not variable isolation; Linux `efibootmgr` and Windows BCD mutation
  remain untested. `BootNext` is not consumed on reset, `BootCurrent` is not synthesized, and
  name-bound authenticated writes are unsupported.
* No physical PCI/NVMe/GPU/USB/NIC handoff has been tested. There is no IOMMU setup.
* Direct-monitor serial diagnostics have no compile-time release trace switch and are not yet
  removed from the VM-exit hot path in release builds.
* Development of the direct backend under host KVM adds the measured
  `host KVM -> this L0 -> L1 hypervisor -> L2` nesting and reflection cost. The trusted path
  deliberately removes this project's L0 layer and accepts the outer KVM/QEMU/OVMF stack as TCB.
* The bootstrap, runtime monitor, and guests are unsigned. Secure Boot was disabled for the QEMU
  measurements; signing and verification policy must be added before a Secure Boot test.
* `cargo xbuild x86` produces both artifact pairs under ignored `bin/x86_64/`, but neither has
  booted on physical hardware. The direct path's QEMU-specific map and descriptor-table lifetime,
  plus both paths' firmware runtime and device-path assumptions, remain unverified there.

All generated EFI files, ESP directories, OVMF variable stores, serial logs, UKIs, ISO or qcow2
files, and other large artifacts belong under `bin/` (or another ignored build directory).
The repository's `.gitignore` excludes `/bin`; none of these generated artifacts should be
committed.

## Primary references

* Intel, [Intel 64 and IA-32 Architectures Software Developer's Manual](https://www.intel.com/content/www/us/en/developer/articles/technical/intel-sdm.html), especially Volume 3C/3D VMX operation, VMCS fields, VM-entry checks, exit reasons, EPT, and VMX instruction status.
* UEFI Forum, [UEFI Specification 2.11, Runtime Services / Variable Services](https://uefi.org/specs/UEFI/2.11/08_Services_Runtime_Services.html#variable-services).
* BitVisor, [Nested Virtualization, including Unsafe Nested Virtualization](https://github.com/matsu/bitvisor/blob/66559d62e2932a9c416e541a43bf0ccc0557cd06/docs/nested_virtualization.md) and [`vt_shadow_vt.c`](https://github.com/matsu/bitvisor/blob/66559d62e2932a9c416e541a43bf0ccc0557cd06/core/x86/vt_shadow_vt.c).
* Linux KVM, [`arch/x86/kvm/vmx/nested.c`](https://github.com/torvalds/linux/blob/73e3f0710014fe6d4ed98cfc02292f6121db7558/arch/x86/kvm/vmx/nested.c), [`vmx.c`](https://github.com/torvalds/linux/blob/73e3f0710014fe6d4ed98cfc02292f6121db7558/arch/x86/kvm/vmx/vmx.c), [`vmx.h`](https://github.com/torvalds/linux/blob/73e3f0710014fe6d4ed98cfc02292f6121db7558/arch/x86/kvm/vmx/vmx.h), and [KVM selftests](https://github.com/torvalds/linux/tree/73e3f0710014fe6d4ed98cfc02292f6121db7558/tools/testing/selftests/kvm).
* Microsoft, [Hyper-V TLFS: Nested virtualization](https://learn.microsoft.com/en-us/virtualization/hyper-v-on-windows/tlfs/nested-virtualization), [Hyper-V feature discovery](https://learn.microsoft.com/en-us/virtualization/hyper-v-on-windows/tlfs/feature-discovery), and [Hyper-V hardware requirements](https://learn.microsoft.com/en-us/windows-server/virtualization/hyper-v/host-hardware-requirements).
