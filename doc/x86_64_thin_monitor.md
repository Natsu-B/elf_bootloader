# x86_64 thin monitor: architecture and validation status

This document records only implementation and measurements that exist on
`feat/x86-thin-monitor` as of 2026-08-28. Linux KVM results below come from an actual L2 run;
Windows Hyper-V and WSL2 remain untested.

## Status summary

| Area | Current evidence | Not yet demonstrated |
| --- | --- | --- |
| x86-64 UEFI entry | Builds as `x86_64-unknown-uefi`; boots under QEMU/KVM + OVMF | Physical-machine boot |
| First VMX launch | One vCPU reaches VMX non-root from a runtime EFI driver; Linux crosses `ExitBootServices` while L0 retains its code, data, stack, and `HOST_CR3` pages | Private L0 GDT/IDT/TSS, SMP, and bare-metal lifetime validation |
| Linux UKI/KVM | Linux 7.1.5 loads `kvm_intel nested=0`, creates `/dev/kvm`, and runs the deterministic real-mode L2 to `KVM_EXIT_IO` | SMP, a normal distribution userspace, and a faulting or long-mode L2 |
| Trusted nested VMX | The running monitor handles VMXON, VMCLEAR, VMPTRLD, VMREAD, VMWRITE, INVEPT, VMLAUNCH, and VMRESUME through a direct hardware VMCS; external-interrupt, EPT-violation, and I/O exits were reflected to KVM | Non-empty MSR lists, CR2/XSAVE switching, optional VMX controls, and SMP |
| Direct EPT | QEMU-only 4 GiB L0 identity EPT plus a measured L1-supplied EPTP used directly for L2 | Platform-derived RAM/MMIO memory typing and a non-test workload |
| UEFI variables | ABI-independent profile overlay for the four variable operations, with tests | Runtime-services table integration and OVMF/OS isolation test |
| Windows | None | Windows boot, Hyper-V, WSL2, Sandbox, VBS/HVCI |

Relevant commits include `af35d3b` (x86 HAL/VMX foundation), `d02d384` (four-operation
variable adapter), `767e120` (UEFI payload in VMX non-root), `a0c0bc8` (Linux UKI builder),
`1f9b344` (CPUID dispatch and outer VMRESUME), `4549597` (Linux L1 shell), `ced873a`
(long-mode walk and VMX operand decode), `8d4cfc2` (conservative nested capability mask),
`c8bd855` (runtime-resident monitor, private `HOST_CR3`, and VMXON interception), `39019e8`
(deterministic KVM L2 probe), `6a1547a` through `6a40b2b` (nested VMCS instructions and
INVEPT), and `8c83a42` (direct nested entry and exit reflection).

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

The current smoke uses an application copy only as a bootstrap. `cargo xbuild x86` also copies
the same PE image and changes its subsystem to EFI runtime driver with
`objcopy --subsystem=efi-rtd`. OVMF loads that second image as `EfiRuntimeServicesCode`, so Linux
preserves its pages across `ExitBootServices`:

```text
OVMF / physical UEFI
  -> BOOTX64.EFI (ordinary EFI application)
       1. LoadImage(GUESTX64.EFI)
       2. LoadImage(MONITORX64.EFI), pass the guest handle in LoadOptions
       3. StartImage(runtime monitor)
  -> MONITORX64.EFI (EFI runtime driver, VMX root)
       4. allocate one 83-page EfiRuntimeServicesData block below 4 GiB
       5. build identity EPT and an L0-owned identity HOST_CR3
       6. VMXON, VMCLEAR, VMPTRLD, VMLAUNCH
  -> guest_entry (VMX non-root L1)
       7. firmware StartImage(preloaded guest)
       8. small payload: VMCALL after StartImage returns
          Linux UKI: ExitBootServices, load KVM, and continue running
       9. kvm_intel programs its VMCS and enters the real-mode L2
  -> L2 (the KVM probe)
      10. direct L1 EPT and VMCS run in hardware
      11. L0 reflects requested exits into KVM; userspace observes KVM_EXIT_IO
```

The runtime data block is 83 pages: one VMXON page, one VMCS page, six EPT pages, one zeroed MSR
bitmap, four host-stack pages, 64 guest-stack pages (256 KiB), and six L0 page-table pages (PML4,
PDPT, and four page directories). Linux reports the runtime image and data as `device reserved`.
The VMCS uses the private identity table as `HOST_CR3`, rather than firmware's Boot Services page
tables. This is enough for the measured one-vCPU QEMU/Linux path, but the monitor still reuses
firmware GDT/IDT/TSS state and is not ready for a bare-metal fault or interrupt.

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
2. The trusted nested path puts L1's EPTP directly into the hardware VMCS for L2.
   Because L1 physical addresses are treated as machine physical addresses, there is no EPT12 x
   EPT01 composition, shadow EPT, or EPT02 cache.

The first map is only a bounded QEMU layout, not a general firmware/OS memory map. The earlier
1 GiB all-write-back map stopped at the local-APIC GPA `0xFEE00000`; expanding the map allowed the
UKI to enter the kernel and reach its initramfs shell. A real launch must cover the required
physical address width and derive suitable RAM/MMIO cache types from platform state. The current
code therefore cannot be used for arbitrary bare-metal MMIO or for firmware allocations above
the limit.

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

Build the bootstrap application, its runtime-driver copy, and the test payload under ignored
`bin/x86_64/`:

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
thin-hv: guest uefi payload
thin-hv: VMEXIT cpu=0 level=1 reason=0xa qualification=0x0 ... instruction_len=2
thin-hv: guest cpuid vmx=1 hypervisor=0
thin-hv: VMEXIT cpu=0 level=1 reason=0xa qualification=0x0 ... instruction_len=2
thin-hv: VMEXIT cpu=0 level=1 reason=0x12 qualification=0x0 ... instruction_len=3
thin-hv: vmx guest PASS start_image_status=0x0
```

This validates the runtime-driver handoff and CPUID filtering for the small payload: leaf 1
exposes VMX and clears the hypervisor-present bit, while the Hyper-V-reserved CPUID range is
zeroed. Linux KVM initialization is measured separately below; Hyper-V remains untested.

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
`HOST_CR3`; Linux reclaimed the page-table pages behind it. The six additional pages in the
83-page runtime allocation now provide an L0-owned four-level identity map and are installed as
`HOST_CR3` before launch.

Linux also performs the EFI runtime virtual-address transition. Post-transition VM-exit logging
through `core::fmt` followed relocated formatting metadata while L0 intentionally continued under
its physical identity map. The VM-exit path now writes fixed byte strings and hexadecimal fields
directly to COM1. Pre-launch firmware diagnostics may still use `core::fmt`; the persistent
VM-exit path does not.

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

No Windows ISO or disk image has been downloaded or installed for this branch. Windows has not
booted directly or through the monitor. Hyper-V features have not been enabled, no Hyper-V VM has
been started, and WSL2 has not run. Windows Sandbox and VBS/HVCI are also untested.

The repository contains only the conservative standard-VMX policy needed to begin those tests.
It does not implement or advertise Hyper-V CPUID leaves, SynIC, VP Assist Page, enlightened VMCS,
or enlightened VM-entry. The design goal remains to expose bare-metal-style VMX (`VMX=1`,
`hypervisor-present=0`) to trusted Windows. The small payload and Linux KVM probe measure that
view, but Windows and Hyper-V do not yet.

## Known limitations and bare-metal boundary

* Intel VMX only; AMD SVM is out of scope.
* One vCPU only. Nested state and the active direct run use global storage; there is no AP startup,
  x2APIC policy, APICv, or posted-interrupt support. Move both state objects to per-pCPU storage
  before SMP.
* The smoke EPT covers only the first 4 GiB and uses a QEMU-specific low-WB/upper-UC split. It is
  not safe for a general bare-metal RAM/MMIO layout.
* The runtime PE sections and 83-page data block survive Linux `ExitBootServices`, and the VMCS
  uses an L0-owned `HOST_CR3`. L0 still reuses firmware GDT/IDT/TSS state; private descriptor
  tables and fault handlers are required before bare-metal use.
* `MONITORX64.EFI` is currently the application PE copied with its subsystem changed to EFI
  runtime driver. It has no runtime virtual-address-change handler or self-relocated resident
  core. Raw VM-exit logging works across the measured Linux relocation, but this is not a general
  Windows or firmware runtime-PE solution.
* The bootstrap finds `\EFI\BOOT\MONITORX64.EFI` on its own firmware device handle. A production
  Windows chain needs an explicit monitor/guest device-path handoff instead of assuming the test
  ESP and fallback boot path.
* The measured nested path handles VMXON, VMCLEAR, VMPTRLD, register-form VMREAD/VMWRITE, INVEPT,
  VMLAUNCH, and VMRESUME. Memory-form VMREAD/VMWRITE, VMXOFF, INVVPID, optional VMX controls, and
  VMX in L2 are not supported.
* Direct entry currently requires zero VM-entry MSR-load, VM-exit MSR-store, and VM-exit MSR-load
  counts. Add bounded L0-owned mirrors before accepting non-empty lists.
* The real-mode L2 probe is deliberately fault-free and does not exercise extended register
  state. L0 does not save/switch CR2 or XSAVE state around a direct run; add both before a faulting,
  SIMD-using, SMP, or Windows/Hyper-V workload.
* Linux KVM has run one VM/vCPU to `KVM_EXIT_IO`; an L2 Linux kernel, KVM SMP, and sustained or
  device-heavy workloads have not run.
* The profile variable adapter has no firmware ABI hook, persistent backend wiring, or real OVMF
  profile test. Each QEMU smoke starts from a copied OVMF variable template.
* No physical PCI/NVMe/GPU/USB/NIC handoff has been tested. There is no IOMMU setup.
* Serial diagnostics have no compile-time release trace switch and are not yet removed from the
  VM-exit hot path in release builds.
* Development under host KVM creates another nesting level. The tiny
  `host KVM -> this L0 -> Linux KVM -> L2` chain passed, but failures from features beyond this
  probe must still be separated between the monitor and outer KVM's nested-nested support.
* The bootstrap, runtime monitor, and guests are unsigned. Secure Boot was disabled for the QEMU
  measurements; signing and verification policy must be added before a Secure Boot test.
* `cargo xbuild x86` produces bare-metal-stagable bootstrap and runtime EFI images under ignored
  `bin/x86_64/`, but neither has booted on physical hardware. The QEMU-specific map, descriptor
  tables, runtime relocation, and device-path assumptions must be resolved first.

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
