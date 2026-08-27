# x86_64 thin monitor: architecture and validation status

This document records only implementation and measurements that exist on
`feat/x86-thin-monitor` as of 2026-08-28. It deliberately does not infer Linux KVM,
Windows Hyper-V, or WSL2 success from the smaller UEFI VMX smoke test.

## Status summary

| Area | Current evidence | Not yet demonstrated |
| --- | --- | --- |
| x86-64 UEFI entry | Builds as `x86_64-unknown-uefi`; boots under QEMU/KVM + OVMF | Physical-machine boot |
| First VMX launch | One vCPU reaches VMX non-root; the small payload returns by VMCALL and the Linux path crosses `ExitBootServices` | L0 interrupt reflection, SMP, and reclaim-safe long-lived L0 ownership |
| Linux UKI | Reproducible UKI build; both direct OVMF and this L0 reach the initramfs serial shell with `vmx=1` | `kvm_intel`, QEMU/KVM L2, SMP, or a normal distribution userspace |
| Trusted nested VMX | Heap-free state/capability/direct-VMCS patch policy and tests | VMX-instruction interception, VM-exit reflection, Linux KVM or Hyper-V execution |
| Direct EPT | QEMU-only 4 GiB identity EPT; direct-L1-EPT policy is encoded | Platform-derived RAM/MMIO memory typing and a live L1 EPT |
| UEFI variables | ABI-independent profile overlay for the four variable operations, with tests | Runtime-services table integration and OVMF/OS isolation test |
| Windows | None | Windows boot, Hyper-V, WSL2, Sandbox, VBS/HVCI |

Relevant commits include `af35d3b` (x86 HAL/VMX foundation), `d02d384` (four-operation
variable adapter), `767e120` (UEFI payload in VMX non-root), `a0c0bc8` (Linux UKI builder),
`e38b217` (Nix UKI tools), `1f9b344` (CPUID dispatch and VMRESUME), `4549597` (Linux L1 shell),
and `28294ff` (parameterized smoke runner).

## Architecture and late launch

The AArch64 boot paths remain separate. The x86 work is split into target-gated or independent
crates:

* `arch_hal/x86_64_hal`: CPUID/MSR/control-register access, typed physical addresses, VMCS field
  encodings, VMX instruction wrappers, and the smoke EPT builder.
* `x86_uefi_loader`: UEFI entry, COM1 diagnostics, reserved monitor pages, VMX setup, and the
  current VM-exit target.
* `x86_guest_uefi_test`: the smallest firmware payload used to prove `StartImage` in VMX
  non-root.
* `nested_vmx`: safe, heap-free policy/state for the intended trusted direct-VMCS path.
* `uefi_variable_overlay`: safe, heap-free profile-selection policy independent of VMX and the
  firmware ABI.

The current smoke uses a late launch while UEFI Boot Services are still live:

```text
OVMF / physical UEFI
  -> x86_uefi_loader (VMX root)
       1. LoadImage(GUESTX64.EFI) while still in root mode
       2. reserve the VMX/EPT/MSR-bitmap/stack pages as EFI_RESERVED_MEMORY_TYPE
       3. VMXON, VMCLEAR, VMPTRLD, VMLAUNCH
  -> guest_entry (VMX non-root)
       4. firmware StartImage(preloaded image)
       5. VMCALL after StartImage returns
  -> L0 VM-exit target and serial result
```

The current QEMU smoke reserves 77 pages: one VMXON page, one VMCS page, six EPT paging pages, one
zeroed MSR-bitmap page, four host-stack pages, and 64 guest-stack pages (256 KiB). This proves the
basic transition and lets firmware load and relocate a variable-sized EFI image before launch,
subject to available firmware memory and the EPT ceiling below. It is not yet a persistent
Type-1 monitor: the loader's executable pages are still ordinary UEFI application memory. Linux
does call `ExitBootServices`, but the reserved lifetime of all live L0 executable/data pages has
not yet been established for bare metal.

The intended late-launch continuation is to leave firmware and the selected OS in L1 while L0
retains only VM-exit, VMX, variable-overlay, and reserved-memory state. Moving all live L0 code,
data, stacks, descriptors, and page tables to non-reclaimable memory is required before that
continuation is safe.

## Threat model and TCB boundary

Linux, Linux KVM, Windows, Hyper-V, administrators, and root are trusted. The monitor is intended
to isolate accidental or policy-driven changes to boot state, not to defend L0 from a malicious
L1. In particular, a trusted L1 may construct an EPT that maps L0 memory; this is accepted to
avoid shadow EPT and nested-page-table validation.

The added TCB is intended to contain only:

* the x86 loader, VM-entry/exit assembly, VMCS policy, and architecture wrappers;
* the variable-overlay adapter once it is connected to firmware Runtime Services;
* the physical UEFI firmware and the selected, explicitly trusted L1 OS/hypervisor.

There is no device model, scheduler, virtual block/network device, filesystem in L0, ACPI AML
interpreter, migration, or snapshot support. Physical devices and device firmware are expected
to remain shared. PK, KEK, db, and dbx are also shared by policy.

## Direct EPT and its current ceiling

There are two distinct EPT uses:

1. The current QEMU smoke builds six EPT paging pages and maps physical `[0, 4 GiB)` identically
   with 2 MiB read/write/execute leaves. `[0, 1 GiB)` is marked write-back for the test RAM;
   `[1 GiB, 4 GiB)` is uncacheable for the QEMU APIC, PCI MMIO, and firmware windows. The loader
   rejects its reserved block, entry/exit code, or CR3 if an address is at or above 4 GiB.
2. The trusted nested model intends to put L1's EPTP directly into the hardware VMCS for L2.
   Because L1 physical addresses are treated as machine physical addresses, there is no EPT12 x
   EPT01 composition, shadow EPT, or EPT02 cache.

The first map is only a bounded QEMU layout, not a general firmware/OS memory map. The earlier
1 GiB all-write-back map stopped at the local-APIC GPA `0xFEE00000`; expanding the map allowed the
UKI to enter the kernel and reach its initramfs shell. A real launch must cover the required
physical address width and derive suitable RAM/MMIO cache types from platform state. The current
code therefore cannot be used for arbitrary bare-metal MMIO or for firmware allocations above
the limit.

The direct-L1-EPT path is a tested policy, not an integrated execution path. `INVEPT` wrappers
exist, and the conservative capability mask retains four-level EPT, write-back EPTP, and
single/global invalidation when hardware provides the required subset. No L1 has yet supplied an
EPTP to the running monitor.

## Trusted direct-VMCS nesting model

The intended path follows BitVisor's unsafe/trusted nesting idea: L1's VMCS page is also the
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

Before an L2 entry, L0 must save L1's VMCS host state and replace it with L0's CRs, segment bases,
descriptor-table bases, SYSENTER state, PAT/EFER, RSP/RIP, and applicable CET state. Hardware then
lands at L0 on an L2 exit. For an exit requested by L1, L0 applies the saved L1 host state as the
new L1 guest state and reflects the nested exit; an exit forced only by L0 is handled locally and
L2 resumes.

### VM-exit MSR store/load mirror

Passing L1's two VM-exit MSR lists through unchanged would make L0 enter with L1-selected host
MSRs and would expose stores caused by L0-only exits. The encoded model therefore does this:

| Direct VMCS field | Entry-time replacement | Reflected L2 exit | L0-only exit |
| --- | --- | --- | --- |
| `VM_EXIT_MSR_STORE_ADDR/COUNT` | Save L1 metadata; point at an L0-owned mirror | Copy captured values to L1's original store list | Discard mirror values |
| `VM_EXIT_MSR_LOAD_ADDR/COUNT` | Save L1 metadata; point at an L0 list that restores L0-safe MSRs | Apply L1-requested host MSRs before returning to L1 | Keep/restore L0 state and resume L2 |

The policy caps each mirrored list at 512 entries. The mirror buffers, list copying, guest-memory
adapter, VMX-instruction decoder, hardware VMCS patch/restore loop, and nested-exit synthesizer are
not implemented in the running monitor yet. VMCS shadowing, VPID, APICv, posted interrupts,
VMFUNC, PML, TSC scaling, and eVMCS are deliberately not advertised by the policy.

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

The ABI-independent adapter implements the four UEFI variable operations from UEFI 2.11:

| Operation | Profile behavior |
| --- | --- |
| `GetVariable` | Map private names to the selected profile; otherwise pass the key through; preserve required-buffer-size behavior |
| `SetVariable` | Apply the same mapping; an empty data slice is deletion |
| `GetNextVariableName` | Rebuild a bounded snapshot, hide raw private/other-profile/internal keys, and expose selected keys under logical names with a terminating NUL |
| `QueryVariableInfo` | Pass through physical-store capacity for the requested attributes |

Enumeration is bounded by compile-time entry/name capacities and returns out-of-resources when the
snapshot cannot represent the store. This is the deliberate current ceiling.

No adapter is installed into an actual `EFI_RUNTIME_SERVICES` table yet. Consequently this branch
does not yet prove runtime table CRC handling, virtual-address transition, authenticated-variable
interaction, OVMF persistence, Linux `efibootmgr`, Windows BCD behavior, or Windows/Linux profile
isolation. `UpdateCapsule` and `QueryCapsuleCapabilities` are also not intercepted by this module;
the intended policy is pass-through, but that integration does not exist yet.

## Nix build and test commands

Enter the pinned development environment for every command:

```sh
nix develop --accept-flake-config
```

The shell supplies nightly Rust with the AArch64 and x86 UEFI targets, QEMU, OVMF, binutils,
`cpio`, `file`, `gzip`, a static BusyBox, and the systemd x86 EFI stub. It exports `OVMF_CODE`,
`OVMF_VARS`, `BUSYBOX_STATIC`, and `LINUX_EFI_STUB`.

Build the two x86 EFI applications and stage them under ignored `bin/x86_64/`:

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

After the Linux L1 changes, `nix develop --accept-flake-config --command cargo xtest -t std`
passed all 15 selected host-test packages. A fresh full AArch64 firmware/QEMU regression is still
required.

## QEMU/KVM + OVMF UEFI smoke

`scripts/x86_64/run-uefi-smoke.sh` creates a fresh copy of the OVMF variable template, stages the
loader as `EFI/BOOT/BOOTX64.EFI`, stages the payload as `EFI/BOOT/GUESTX64.EFI`, and runs one vCPU:

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
thin-hv: guest uefi payload
thin-hv: VMEXIT cpu=0 level=1 reason=0xa qualification=0x0 ... instruction_len=2
thin-hv: guest cpuid vmx=1 hypervisor=0
thin-hv: VMEXIT cpu=0 level=1 reason=0xa qualification=0x0 ... instruction_len=2
thin-hv: VMEXIT cpu=0 level=1 reason=0x12 qualification=0x0 ... instruction_len=3
thin-hv: vmx guest PASS start_image_status=0x0
```

This validates CPUID filtering only for the small payload: leaf 1 exposes VMX and clears the
hypervisor-present bit, while the Hyper-V-reserved CPUID range is zeroed. It is not evidence that
KVM or Hyper-V can initialize.

## Linux UKI and direct OVMF control test

Build the test UKI from the running NixOS kernel, static BusyBox initramfs, systemd EFI stub, and
`scripts/x86_64/linux-l1-init`:

```sh
scripts/x86_64/build-linux-uki.sh
```

The artifact regenerated inside the Nix development shell was:

```text
linux L1 UKI: artifact=.../bin/x86_64/linux-l1.efi
linux L1 UKI: file=PE32+ executable (EFI application) x86-64 (stripped to external PDB), for MS Windows, 10 sections
linux L1 UKI: size=14982656 bytes
```

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

## Monitor-to-Linux debug progression and result

The Linux UKI was then used as `GUESTX64.EFI` with 768 MiB and a 12-second timeout. The captured
pre-handler failure was:

```text
thin-hv: guest cr0=0x80010033 cr3=0x2fc01000 cr4=0x2668 efer=0xd00 rip=0x2e118450 rsp=0x2fb38ff8
thin-hv: VMEXIT cpu=0 level=1 reason=0xa qualification=0x0 guest_rip=0x2ee3ee95 instruction_len=2
thin-hv: vmx guest FAIL marker=0x7468696e68764d58 start_image_status=0xffffffffffffffff vmxoff=Success
```

Intel defines basic exit reason `10` (`0xA`) as CPUID. The all-ones `start_image_status` shows that
the exit occurred inside firmware/UKI execution before `StartImage` returned. This is not an EPT
violation and not a UKI-build failure.

A CPUID save/dispatch/resume path is implemented. The historical failure was captured with the UKI
as the second argument; marker checks are disabled because the Linux image never reached them:

```sh
X86_UEFI_MEMORY=768M \
X86_UEFI_TIMEOUT_SECONDS=12 \
X86_RETURN_MARKER= \
X86_GUEST_MARKER= \
scripts/x86_64/run-uefi-smoke.sh \
  bin/x86_64/x86-uefi-loader.efi \
  bin/x86_64/linux-l1.efi
```

Linux was then rerun with CPUID dispatch and a zeroed MSR bitmap. Six CPUID exits resumed before
the first address-space blocker:

```text
thin-hv: guest cr0=0x80010033 cr3=0x2fc01000 cr4=0x2668 efer=0xd00 rip=0x2e113520 rsp=0x2fb38ff8
thin-hv: vmx guest FAIL: unhandled VM exit reason=0x30 qualification=0x182 guest_rip=0x2ee3e624 instruction_len=3 vm_instruction_error=0x0 cpuid_exits=6 rax=0xfee00000 rbx=0xfee000b0 rcx=0x1b rdx=0xfee00900
thin-hv: VMXOFF status=Success
```

Basic exit reason `0x30` is an EPT violation. The access reached the local-APIC physical range at
`0xFEE00000`, which was outside the smoke EPT's `[0, 1 GiB)` map. This separated the address-space
ceiling from the CPUID handler.

The next run used the QEMU-specific 4 GiB identity map, with low test RAM as write-back and the
upper 3 GiB as uncacheable. It passed 188 CPUID exits and reached a high Linux kernel RIP before
stopping:

```text
thin-hv: guest cr0=0x80010033 cr3=0x2fc01000 cr4=0x2668 efer=0xd00 rip=0x2e111830 rsp=0x2fb38ff8
thin-hv: vmx guest FAIL: unhandled VM exit reason=0x2 qualification=0x0 guest_rip=0xffffffff820b1a60 instruction_len=3 vm_instruction_error=0x0 cpuid_exits=188 rax=0x21690000 rbx=0x800 rcx=0x70 rdx=0x1060
thin-hv: VMXOFF status=Success
```

Intel defines basic exit reason `2` as triple fault. That run emitted no Linux initramfs serial
marker, but proved that CPUID handling and the APIC address-space extension moved execution into
the kernel.

The remaining QEMU/Linux bring-up used a 256 KiB guest stack and made the directly executed
controls agree with the passthrough CPUID view: RDTSCP, INVPCID, XSAVES/XRSTORS, and
UMWAIT/TPAUSE are enabled when required. CR4.VMXE is present in the hardware guest state but
hidden through the VMCS mask/read shadow so Linux initially sees its pre-launch CR4; actual nested
VMX transitions are not implemented. Unconditional XSETBV exits are applied directly for this
trusted L1 and the RIP is advanced. The zero MSR bitmap lets covered MSRs execute directly; a
bitmap-outside RDMSR used by Linux's feature probing receives an injected `#GP(0)`.

With those bounded handlers, the same monitor command reached the shell:

```sh
X86_UEFI_MEMORY=768M \
X86_UEFI_TIMEOUT_SECONDS=45 \
X86_RETURN_MARKER= \
X86_GUEST_MARKER='thin-hv: linux L1 shell' \
scripts/x86_64/run-uefi-smoke.sh \
  bin/x86_64/x86-uefi-loader.efi \
  bin/x86_64/linux-l1.efi
```

```text
[    0.477802] Run /init as init process
thin-hv: linux L1 /proc/cpuinfo vmx=1
thin-hv: linux L1 shell
~ #
```

This is an actual `OVMF -> L0 -> Linux L1` serial-shell result. It proves neither that
`kvm_intel` initializes nor that an L2 guest runs; those are separate pending tests.

## Windows, Hyper-V, and WSL2 status

No Windows ISO or disk image has been downloaded or installed for this branch. Windows has not
booted directly or through the monitor. Hyper-V features have not been enabled, no Hyper-V VM has
been started, and WSL2 has not run. Windows Sandbox and VBS/HVCI are also untested.

The repository contains only the conservative standard-VMX policy needed to begin those tests.
It does not implement or advertise Hyper-V CPUID leaves, SynIC, VP Assist Page, enlightened VMCS,
or enlightened VM-entry. The design goal remains to expose bare-metal-style VMX (`VMX=1`,
`hypervisor-present=0`) to trusted Windows, but the small CPUID payload is the only measurement of
that view so far.

## Known limitations and bare-metal boundary

* Intel VMX only; AMD SVM is out of scope.
* One vCPU only. There is no AP startup, physical interrupt routing, x2APIC policy, APICv, or posted
  interrupt handling.
* The smoke EPT covers only the first 4 GiB and uses a QEMU-specific low-WB/upper-UC split. It is
  not safe for a general bare-metal RAM/MMIO layout.
* The 77 reserved pages survive firmware allocation, but the UEFI loader's own executable/data
  lifetime after `ExitBootServices` has not been made safe.
* The VM-exit path currently handles only CPUID, trusted XSETBV, one unsupported-RDMSR `#GP`
  path, and the VMCALL test. Invalid XSETBV operands are not converted to guest `#GP`; general
  exception/interrupt, I/O, MSR, EPT, and nested-VMX exit routing is absent.
* `nested_vmx` is a policy crate, not a live instruction emulator. Linux has not loaded
  `kvm_intel` under L0, and no L2 Linux has booted.
* The profile variable adapter has no firmware ABI hook, persistent backend wiring, or real OVMF
  profile test. Each QEMU smoke starts from a copied OVMF variable template.
* No physical PCI/NVMe/GPU/USB/NIC handoff has been tested. There is no IOMMU setup.
* Serial diagnostics are not yet compiled out in a release configuration.
* Development under host KVM creates another nesting level. A future
  `host KVM -> this L0 -> Linux KVM/Hyper-V -> L2` failure must be separated from this monitor's
  behavior; no such three-level execution has been attempted yet.
* `cargo xbuild x86` produces a bare-metal-stagable EFI application under ignored `bin/x86_64/`,
  but it is unsigned and has not been booted on physical hardware. The QEMU-specific 4 GiB map and
  UEFI-memory-lifetime issue must be replaced before that test can establish OS readiness.

All generated EFI files, ESP directories, OVMF variable stores, serial logs, UKIs, future ISO or
qcow2 files, and other large artifacts belong under `bin/` (or another ignored build directory).
The repository's `.gitignore` excludes `/bin`; none of these generated artifacts should be
committed.

## Primary references

* Intel, [Intel 64 and IA-32 Architectures Software Developer's Manual](https://www.intel.com/content/www/us/en/developer/articles/technical/intel-sdm.html), especially Volume 3C/3D VMX operation, VMCS fields, VM-entry checks, exit reasons, EPT, and VMX instruction status.
* UEFI Forum, [UEFI Specification 2.11, Runtime Services / Variable Services](https://uefi.org/specs/UEFI/2.11/08_Services_Runtime_Services.html#variable-services).
* BitVisor, [Nested Virtualization, including Unsafe Nested Virtualization](https://github.com/matsu/bitvisor/blob/66559d62e2932a9c416e541a43bf0ccc0557cd06/docs/nested_virtualization.md) and [`vt_shadow_vt.c`](https://github.com/matsu/bitvisor/blob/66559d62e2932a9c416e541a43bf0ccc0557cd06/core/x86/vt_shadow_vt.c).
* Linux KVM, [`arch/x86/kvm/vmx/nested.c`](https://github.com/torvalds/linux/blob/73e3f0710014fe6d4ed98cfc02292f6121db7558/arch/x86/kvm/vmx/nested.c), [`vmx.c`](https://github.com/torvalds/linux/blob/73e3f0710014fe6d4ed98cfc02292f6121db7558/arch/x86/kvm/vmx/vmx.c), [`vmx.h`](https://github.com/torvalds/linux/blob/73e3f0710014fe6d4ed98cfc02292f6121db7558/arch/x86/kvm/vmx/vmx.h), and [KVM selftests](https://github.com/torvalds/linux/tree/73e3f0710014fe6d4ed98cfc02292f6121db7558/tools/testing/selftests/kvm).
* Microsoft, [Hyper-V TLFS: Nested virtualization](https://learn.microsoft.com/en-us/virtualization/hyper-v-on-windows/tlfs/nested-virtualization), [Hyper-V feature discovery](https://learn.microsoft.com/en-us/virtualization/hyper-v-on-windows/tlfs/feature-discovery), and [Hyper-V hardware requirements](https://learn.microsoft.com/en-us/windows-server/virtualization/hyper-v/host-hardware-requirements).
