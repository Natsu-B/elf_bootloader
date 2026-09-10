# Physical x86 L0: incremental implementation and validation

## Standalone persistent-profile gate

```sh
nix develop --accept-flake-config --command cargo xrun x86 --uefi-profiles --release
```

This finite test uses a fresh disposable OVMF variable store, default q35, 256 MiB,
one CPU and no installed OS or project VMX. It also runs in the standard release
suite. The production firmware-backed hooks must preserve Windows/Linux private
boot-variable views through cold/warm resets, full-store failures, full enumeration,
ExitBootServices and an identity SetVirtualAddressMap. A final cold reset checks
the runtime writes and the independent persistent profile selector: four boots,
three resets, 60 seconds total. Security variables and BootCurrent remain shared;
no security contents or Windows product keys are logged.

The transcript is `bin/x86_64/profile-contract.log`; generated firmware/EFI files
are not source artifacts. This is a profile/firmware gate, not Direct SMP, S3/S4,
nonidentity OS runtime mapping, Hyper-V or physical-laptop qualification. The
physical Direct backend still has no overlay by default; production profile/path
selection wiring remains work in progress. Current evidence and limitations are
in [the daily-candidate log](evidence/x86_64/daily-candidate-2026-09-11.md).

## Opt-in profile Direct selection

The additional opt-in profile Direct selection gate is:

```sh
nix develop --accept-flake-config --command cargo xrun x86 --profile-direct --release
```

It tests six native-UEFI Direct cases, not installed Windows/Linux: each profile
selected explicitly and from the persistent selector, plus Windows BootNext and
Linux BootOrder execution. The backend logs `profile-uefi`
and uses distinct `x86-uefi-profile-direct-{loader,monitor}.efi` artifacts. Exact
UTF-16 `windows` / `linux` LoadOptions make an explicit selection; empty options
require an existing valid selector. Linux's current-ESP path is supplied at build
time by `THIN_HV_LINUX_EFI_PATH`; this test child build sets it to the disposable
`\EFI\ubuntu\shimx64.efi` fixture. Missing configuration never selects another OS.
The original `physical-uefi` backend remains overlay-free.

On a persistent-profile boot, only that profile's BootNext/BootOrder/Boot#### are
consulted. BootNext is consumed before attempting its target; BootOrder considers
active boot-category entries in order. Missing/inactive entries may be skipped,
but malformed, unsupported, or other-ESP entries fail closed. A missing BootOrder
uses the configured same-profile manager; an exhausted recorded order does not.
Explicit profile selection boots the configured manager without consuming its
BootNext. Full device paths must name the current ESP; file-only paths are rooted
there, and short hard-drive paths must match its partition signature. No filesystem
enumeration, synthetic identity, or outer-KVM fallback is involved.

The selected Boot#### optional bytes are retained for the loaded image, and native
volatile BootCurrent is published for that option. A pre-entry failure restores
the previous BootCurrent and releases the option buffer after unloading the target.
Configured-path boots leave BootCurrent firmware-provided. This is not a complete
replacement for firmware BDS: additional BootOrder targets are not launched after
an OS that successfully loaded subsequently returns from StartImage. Driver options
are namespaced but not automatically launched. SMP, OS power/update cycles and
Hyper-V remain unfinished; these six cases are not a daily-use claim.

## Authoritative starting point

The normal `cargo xrun x86 --release` suite now includes read-only CPU inventory
under QEMU/KVM and QEMU/TCG with 1, 2, 4 and 8 CPUs. Logs are retained as
`bin/x86_64/preflight-{kvm,tcg}-cpus-{1,2,4,8}.log`. Each must enumerate exactly
the requested number of distinct APIC identities and a unique enabled BSP,
without starting an AP or entering project VMX. Disabled CPUs are retained in
the inventory. This is **firmware inventory coverage, not L1 SMP support**:
the physical/profile Direct gate still rejects more than one firmware CPU.

Work started on 2026-09-07 and continued on 2026-09-08 (JST), without switching branches.
The checked-out local implementation is authoritative; the public Branches index described in
the task did not expose this development branch. No code was reconstructed from older main/origin.

The requested initial Git inspection recorded:

```text
$ git branch --show-current
feat/x86-thin-monitor
$ git status --short
 M AGENTS.md
$ git log --oneline -20
646807e docs(x86): record bounded poweroff timeout verification
1945057 docs(x86): record final trusted-path review
fcb15fa fix(x86): close final trusted review findings
780cc10 docs(x86): record post-review validation
b59e2eb fix(x86): reject invalid soak media IDs
125e062 test(x86): harden Windows daily-use gates
335163a test(x86): reject crash-recovered Linux soaks
d2ec722 fix(x86): preserve trusted chainload status
505e900 fix(x86): close trusted review gaps
9afbdb8 docs(x86): record Claude quota stop
6a5ebbe docs(x86): record daily-use validation
e149051 fix(x86): shut down cleanly after S4 resume
b6b16ea fix(x86): validate procfs-backed S4 probe
e965f0f fix(x86): require clean Linux soak shutdown
f186a8a fix(x86): harden Windows S4 validation
2d3cc3a test(x86): add repeatable Windows daily soak
e40eb29 test(x86): add repeatable Linux daily soak
d2ee9c3 test(x86): cover trusted guest locations
3fbaa9f perf(x86): remove HAL from trusted loader
976b2e9 refactor(x86): isolate trusted chainloader
```

The pre-existing AGENTS.md change is retained and excluded from implementation commits.
Claude is not used. QEMU-testable work proceeds before physical-machine validation.
The branch remains `feat/x86-thin-monitor`. The HAL platform-map work is recorded in
`7a48ad7`; the integrated physical loader/preflight, diagnostics and runner changes are in
`dd9299c`. Commit `0ec4438` also rejects late Direct-VMX failures after QEMU exit. These are
incremental foundations, not completion of the physical L0 architecture.

The changed implementation surfaces are:

* `x86_uefi_loader/src/{main,chainload,physical_chainload,physical_preflight,trusted_outer_kvm}.rs`:
  feature-separated backend dispatch, shared firmware image helpers, current-ESP selection and
  bounded read-only inventory.
* `arch_hal/x86_64_hal/src/platform_memory.rs`: `PhysicalWidth`/`PhysicalRange`, validated UEFI
  map decoding, architectural memory-type decisions and reserved-region-aware EPT planning.
* `x86_uefi_loader/src/vmx_smoke.rs`: `ExitDiagnostics`, `record_diagnostic` and
  `dispatch_l1_exit`; no new nested capability exposure or SMP state implementation.
* `x86_guest_uefi_test/src/physical_policy.rs`, the x86 loader/guest manifests, `xtask` and
  `xtest.txt`: non-VMX firmware fixtures, distinct backend artifacts and integrated host/QEMU gates.
* `scripts/x86_64/`: existing UEFI/Windows/Linux runners, finite KVM lifecycle checks, bounded
  diagnostic decoder and read-only Windows status/SelfTest scripts. The Windows serial matcher
  is `serial_has_exact_marker`; the daily-soak change is test-only deterministic shutdown.

## Deployment and backend boundary

The production goal is physical UEFI -> project thin L0 -> the machine's original Windows or
Linux installation, selected mutually exclusively. Hyper-V/WSL2 or KVM then runs an L2 through
trusted Direct-VMCS. There is no simultaneous Windows/Linux execution milestone yet.

| Build feature | Artifact in bin/x86_64 | Role |
| --- | --- | --- |
| physical-chainload | x86-uefi-physical-loader.efi | No project VMX, no resident runtime; original ESP baseline |
| physical-preflight | x86-uefi-preflight.efi | Read-only diagnostic; returns to firmware, no guest launch |
| direct-vmx | x86-uefi-loader.efi and x86-uefi-monitor.efi | Existing one-vCPU QEMU project-L0 prototype, not physical-ready |
| trusted-outer-kvm | x86-uefi-kvm-loader.efi | Outer-KVM reference, A/B control and CI backend |

Exactly one backend feature may be selected in a loader build. `cargo xbuild x86` builds separate
artifacts and checks that each non-VMX artifact contains no decoded VMX instructions. The default
`direct-vmx` feature still denotes the existing prototype; do not deploy that artifact on a real
Windows machine merely because the physical baseline/preflight succeeds.

## Stage 1: physical chainload baseline

`x86_uefi_loader/src/chainload.rs` owns shared, checked firmware protocol/image operations.
`physical_chainload::run` chooses only `LoadedImage.DeviceHandle`, proving SimpleFileSystem on that
handle and constructing a complete DevicePath before normal firmware LoadImage/StartImage.
The [UEFI Loaded Image protocol](https://uefi.org/specs/UEFI/2.11/09_Protocols_EFI_Loaded_Image.html)
defines that source handle, its file-path portion, and binary LoadOptions.

Empty LoadOptions select `\EFI\Microsoft\Boot\bootmgfw.efi` on the current ESP. An explicit
selection is one NUL-terminated UTF-16LE absolute ASCII path, such as
`\EFI\ubuntu\shimx64.efi`, on that same ESP. The bounded format is at most 260 UTF-16 units including
NUL; malformed termination, odd byte length, non-ASCII paths, relative/dot/empty components,
ambiguous trailing dots/spaces, and an explicit self-chainload are rejected. This is a binary
LoadOptions contract, not a general UEFI Shell command-line parser.

The loader must itself be started from the intended existing OS ESP. Starting it from an unrelated
USB ESP does not authorize searching other filesystems: a missing target fails, even if another
disk has a Windows loader. Duplicate Windows paths on other ESPs cannot change the selected
device. There is no staged GUESTX64.EFI preference, firmware-enumeration-first rule, or outer-KVM
fallback in this backend. A future explicit partition/Boot#### selector must remain deterministic.

This baseline does not allocate project RuntimeServices pages, install variable hooks/overlays,
rewrite system tables, execute project VMX instructions, or create an ESP. Child warning/error
statuses are preserved, temporary path/ExitData pools are released, and returned applications are
not redundantly unloaded. COM1 polling is bounded; failed diagnostics cannot indefinitely block
normal firmware boot.

The QEMU-only `x86_guest_uefi_test` policy driver/payload builds exercise this exact loader rather
than a replacement selector. Two disposable, read-only ESPs include duplicate Windows/Linux
paths; the secondary ESP is enumerated first while the driver ESP has explicit boot priority.
Before running cases, the driver must prove that firmware can load the three secondary-ESP
targets without starting them. Its five cases require default Windows and explicit Linux on
the current ESP to succeed, an other-ESP-only target to return NOT_FOUND, malformed LoadOptions
to return INVALID_PARAMETER, and self-chainload to return ACCESS_DENIED. Child images verify
their actual LoadedImage device handle. Ordered markers, exact statuses, and scoped expected
failures are checked by [run-uefi-smoke.sh](../scripts/x86_64/run-uefi-smoke.sh); an attached but
firmware-invisible secondary disk is not accepted as duplicate-ESP coverage.

The fixture artifacts `x86-uefi-physical-policy-driver.efi` and
`x86-uefi-physical-policy-payload.efi` are test programs, not deployment backends. Their presence
or successful compilation alone does not establish a passed policy boot test.

## Stage 2: physical preflight

`physical_preflight::run` records CPUID vendor/family/model, VMX and physical width, gated VMX
capability MSRs, FEATURE_CONTROL, PAT/MTRR state, and a validated UEFI memory-map snapshot.
It records ACPI/SMBIOS table presence/addresses and MSDM header presence only. It never reads the
MSDM key payload, dumps SMBIOS identifiers, or changes firmware identity. Temporary Boot Services
pool and EPT-audit page allocations are released; there is no VMXON, WRMSR, ExitBootServices, child launch, or persistent
firmware-variable mutation.

Map sizes, descriptor version/stride/counts, physical-width limits, overlaps, arithmetic and
pointer spans are checked. ACPI reads are bounded by readable RAM descriptors. An MSDM child is
examined only through its 36-byte header; its product-key body is neither read nor checksummed.
Runtime-service `entry_present` is a pre-ExitBootServices pointer inventory, not proof that the
service remains usable after OS transition or is advertised by EFI_RT_PROPERTIES_TABLE.

Stable provenance/terminal markers distinguish diagnostic completion from readiness:

```text
thin-hv: backend=physical-chainload project_vmx=0 resident_runtime=0
thin-hv: physical chainload PASS
thin-hv: backend=physical-preflight project_vmx=0
thin-hv: preflight VMX=0 direct_vmx_ready=0
thin-hv: preflight EPT audit SKIP reason=no-ept-capability direct_vmx_ready=0
thin-hv: physical preflight PASS
```

`direct_vmx_ready=0` also remains zero when VMX is present: inventory alone does not fix the
unfinished physical monitor. Missing VMX is tested under TCG without probing unsupported VMX MSRs.

## Remaining Direct-VMCS work, identified in current source

| Stage | Current implementation boundary / remaining work |
| --- | --- |
| 3: platform EPT | The heap-free HAL planner now feeds a checked 4-level EPT materializer, including 4 KiB/2 MiB/1 GiB leaves and explicit arena ownership. Physical preflight builds and discards tables from the actual UEFI/MTRR snapshot. This is map-only coverage, not complete APIC/PCI MMIO discovery. The active vmx_smoke carrier still uses ept::build_identity_8g and fixed WB/UC QEMU buckets. No physical EPT is installed. |
| 4: private L0 state | Carrier and direct-L2 VM-exit host fields now use private GDT/IDT/TSS, selectors, FS/GS/SYSENTER targets, an ordinary host stack and four separate IST stacks. Every root exception stops visibly; NMI forwarding is not implemented. HOST_CR3 is still a fixed 8 GiB identity map. A separate L1 bootstrap and platform host map remain required before physical deployment. |
| 5: pCPU ownership | VMXON/carrier and L1_VCPU_STATE, CARRIER_PATCH_VALUES, DIRECT_PATCH_VALUES, DIRECT_ENTRY_POLICY, NESTED_RUN remain single-CPU. No AP bringup, VMCS migration or normal Windows SMP claim. |
| 6: identity | New baseline/preflight leave system tables, real TPM, Secure Boot data and storage untouched. Physical Direct-VMX reservations/identity preservation have not been established. |
| 7: Linux nested KVM | Direct-VMX Linux passes repeated two-VM context switching, IO completion, register/CR2/XMM0–7/MXCSR checks, and same-GPA memslot replacement in single-vCPU QEMU/KVM. VMfailValid error exposure is corrected and checked by a native VMX fixture; operand exception delivery remains incomplete. This is not physical Linux/SMP or full XSAVE coverage, and reference soaks are not Direct-VMX evidence. |
| 8: Hyper-V state | CR2/XSAVE remain shared; XSETBV inputs are not independently validated; nonempty MSR lists fail-stop. ControlProvenance/MSR mirrors are not runtime-integrated; all direct L2 exits currently reflect. VPID ownership/reuse is not implemented. |
| 9: watchdog | Bounded BSP-only exit counters and fixed-size timeout capture are implemented; repeated nonfatal VMX logging is removed. No validated watchdog correction or full state-management solution is established. The latest Windows timeout is a separate observation, not proof of the historical bugcheck. |
| 10: original Windows | Read-only status collection/documentation can be exercised under QEMU, but the actual original motherboard/firmware installation is untested. |

Do not substitute a conventional VMCS12/VMCS02 layer merely to avoid these state/lifetime issues.
Do not expose additional Hyper-V capabilities until their semantics and all visible CPUs are
correctly virtualized. Excluding the runtime PE from L1 also requires relocating the current
non-root guest_entry trampoline; blindly unmapping it breaks the present handoff.

## Validation procedure and evidence classes

### QEMU-first nested gate (2026-09-08)

Latest expanded coverage, reproduced failures and exact commands are recorded in
[the QEMU nested validation report](evidence/x86_64/validation-2026-09-08-nested.md)
and the subsequent [upstream/real-L2 matrix](evidence/x86_64/validation-2026-09-08-upstream.md).
The older result tables below describe their own increments, not the latest matrix.

The target is to finish architectural development and reproducible nested regressions in QEMU
before exposing an original physical Windows installation to L0. Real-machine testing must still
validate firmware, actual MMIO/DMA/device behavior, microcode/CPU differences, Secure Boot, TPM,
activation **status only**, and platform power transitions. QEMU cannot certify those properties.
The present Direct-VMX prototype is not ready for that production qualification: the active
physical EPT/host map, all-CPU ownership, operand faults and Hyper-V state gaps above remain.

Run the existing x86 test framework's bounded nested suite:

```sh
nix develop --accept-flake-config --command cargo xrun x86 --nested --release
nix develop --accept-flake-config --command env LINUX_KVM_CYCLES=256 cargo xrun x86 --nested --release
```

It runs six QEMU/KVM cases serially, reports every case failure, and exits nonzero if any case
fails. Do not run another generic UEFI/Linux runner concurrently: those existing runners share
the serial log, ESP and OVMF variable-store paths. Windows tests with separate private work
directories can run alongside them, subject to the three-QEMU limit. There is no backend
fallback. Four native-UEFI instruction-contract cases compare
`outer-kvm / reference` against `direct-vmx / project L0` under the native CPU capabilities and
under `vmx-vmwrite-vmexit-fields=off`. This second profile tests error publication without
assuming physical `IA32_VMX_MISC[29]` makes the VMCS error field writable; the matching reference
fixture must report its read-only assertion as executed. The same binary runs under both
backends. Two Linux cases run the real KVM probe with the requested cycle count (default 64,
range 1..4096). Linux UKIs and monitors are built in release by its existing runner; use the
explicit `--release` command above for a uniform release suite.

The instruction fixture allocates four disposable BootServices pages, validates their
layout/ownership, and writes no firmware variable or identity data. VMLAUNCH/VMRESUME cases
are guaranteed entry failures, not successful L2 execution.
It checks VMXON/OFF, VMPTRST, VMCLEAR/VMPTRLD, VMREAD/VMWRITE, two-VMCS state/error persistence
over eight switches, exact CF/ZF and errors 2/3/5/7/9/10/11/12/13/15/28, including wide fields,
misaligned operands, invalid revisions, shadow-header capability masking and all advertised
INVEPT/INVVPID types plus malformed descriptors. All VMCS pages are
cleared and VMXOFF, exact control restoration and FreePages must succeed before its PASS.
The existing Linux probe separately provides actual L2 VMLAUNCH/VMRESUME/exit evidence.

Each Linux probe process owns two independent single-vCPU VMs, interleaving every phase of eight
OUT→IN→OUT→HLT rounds. It verifies all GPRs, selected flags/segments and CR2, completes userspace
IO before inspecting state, deletes/recreates the data slot at the same GPA with alternating
backing pages, and checks both active and retired backing data. PASS requires successful explicit
`munmap`/`close` of both VM/vCPU contexts and `/dev/kvm`; process exit remains a cleanup backstop,
not a substitute for the gate. The original real-mode phase uses 64 `KVM_RUN` calls,
16 completed HLT/state checks and 14 remaps. Each VM initializes XMM0–7/MXCSR sentinels using guest
`MOVDQU`/`LDMXCSR`, then executes SSE2 `PXOR` and `MOVDQU` stores each round. Every completed
HLT checks `KVM_GET_FPU` against all eight expected XMM values and independently checks the
vector actually stored by L2; guest `STMXCSR` stores verify MXCSR each round. Initializing in
the guest avoids assumptions about the legacy FPU ioctl: the
[Linux v7.1.5 implementation](https://raw.githubusercontent.com/gregkh/linux/v7.1.5/arch/x86/kvm/x86.c)
does not transfer its structure's MXCSR member or mark the XSAVE SSE component active when
copying XMM bytes. The new long-mode phase initializes and checks all XMM0–15 separately.
FPU state is not reset between rounds; this is not a cached SET/GET-only test. The cycle
transcript requires `sse_checks=16`. The same two VMs then execute 16 long-mode checkpoints
with two four-level roots, 4 KiB leaves, PTE replacement/INVLPG, CR3 reload/root switching,
XMM0–15/MXCSR, EFER/PAT, disabled-debug DR0 sentinels and nondecreasing guest TSC.
Total per process: 144 KVM_RUN, 32 HLT, 64 paging checks, 16 INVLPG, 48 CR3 writes and
14 memslot remaps. The strict transcript requires each coverage counter and successful cleanup.
Two VMs on one L1 CPU are not L2 SMP. These checks do not establish concurrent invalidation,
VPID generations, full XSAVE/AVX, live-L2 suspend, or TSC accuracy/performance.

The native fixture first reproduced an actual Direct-VMX defect on the prior `7373d97` EFI:
`VMCLEAR(VMXON pointer)` returned ZF=1 but VMREAD(error) still returned 12 rather than 3. The
reference passed. The monitor now executes native failures with the L1-selected VMCS current,
captures their error before restoring the carrier, and records synthetic errors in that same
hardware VMCS. On CPUs without writable exit fields, a closed set of guaranteed-failing VMX
operations records the architectural error, followed by readback verification. No opaque VMCS
bytes, software VMCS12/02 layer, error registry or new crate was added. Repeated VMXON, read-only
VMWRITE under the advertised capability mask, and wide invalid field operands are also corrected.
The successful hot path gains no formatted logging. This is a verified instruction-contract fix,
**not evidence that the Hyper-V "Please wait" cause is fixed**.

The runner preserves generated transcripts under `bin/x86_64/nested-contract-<backend>-<cpu-profile>.log`
and `bin/x86_64/nested-linux-<backend>.log`. Old serial evidence is removed before each case;
failure to clear it prevents that case from running. Gates reject mixed backends, incomplete or
out-of-order coverage, missing cleanup/return evidence, late failures, NULs and oversized logs.
No generated EFI, disk, variable-store or diagnostic artifact belongs in a commit.

Before physical production qualification, require independently passing QEMU gates for the
following unfinished items (do not reclassify a reference pass as Direct success):

| QEMU gate still required | What must be established before relying on physical testing |
| --- | --- |
| Physical-backend execution | Install the platform-derived EPT and private host map, discover MMIO, and separate the shared L1 bootstrap from monitor-private storage in the actual backend. |
| Architectural exception/entry matrix | Precise CPL/operand faults and VM-entry failures; nonempty MSR lists; CR2/XCR0/XSAVE/debug/PAT/EFER state; event/NMI injection and L0-only versus reflected exits. |
| SMP and lifetime | Virtualize every visible L1 CPU, AP startup and reset, per-CPU VMCS ownership/migration, VPID reuse and invalidation, then nested L2 SMP. |
| Real OS nested workloads | Linux L2 OS workloads and repeated suspend/reboot; Direct Windows Hyper-V boot and WSL2 workloads, reboot and sustained device/interrupt load. |
| Power-state regressions | Direct backend S3/S4/reset recovery with active nested guests and persistent-state checks; reference-only power tests are not sufficient. |

Only after these are passed should physical tests become chiefly original-installation,
firmware/device, activation-status, reboot and real power-management qualification. A direct
firmware boot path bypassing L0 must remain available throughout.

This increment started on branch `feat/x86-thin-monitor` at `7373d97` with only the user's
`AGENTS.md` change dirty. The requested branch/status/log-20 inspection was repeated before
edits. VMX corrections are committed in `5f574a6`, the native contract/extended Linux
suite in `4f6f1d8`, and actual SSE lifecycle checks in `3bd5918`. Changed production symbols
are HAL `RecordedFailure`/`record_failure`,
monitor `with_l1_current_vmcs`/`execute_l1_vmx_instruction`/`publish_l1_instruction_error`,
and the VMX instruction handlers. No AArch64 production paths or dependencies were changed.

Available validation for that increment:

| Evidence class | Check | Result |
| --- | --- | --- |
| Host | All affected package entries: HAL 40, nested 7, loader 43, guest 7, xtask 26 | PASS: 123 test executions |
| Build | `cargo xbuild x86`, release builds, non-VMX artifact disassembly, `cargo fmt --check` | PASS |
| QEMU/KVM | Native VMX contracts: two backends × native/read-only VMCS CPU profiles | PASS: 4 cases; invalid INVEPT/INVVPID checked in all four |
| QEMU/KVM | Linux two-VM IO/state/memslot/SSE lifecycle, 256 cycles each backend | PASS: 2 cases, 4,096 SSE checks per backend, clean guest poweroff |
| QEMU TCG/KVM | Unchanged standard xrun matrix in debug and release, including expected root exceptions | PASS: 18 cases |
| QEMU/KVM | Fresh reference Windows Hyper-V overlay from the existing evaluation seed | PASS, runner exit 0 |
| QEMU/KVM | Fresh Direct Windows Hyper-V overlay, 1200-second deadline | FAIL, runner exit 1; final screen remained "Please wait", no BSOD observed |
| Physical hardware | Original installed Windows/Linux, activation status, devices, firmware, all CPUs, power | UNVERIFIED; no physical-machine state was changed |

The final Direct Windows snapshot recorded 9,789,052 observed L2 entries, 9,789,051 reflected
exits, 1,068,048 external-interrupt exits and zero nested-entry failures; phase 4/nested-exit
and reason 31/RDMSR are consistent with one reflection still in progress. These are bounded diagnostics,
not proof of Windows initialization progress or correct interrupt/MSR/XSAVE semantics, and
not a performance benchmark. The instruction-error fix did not resolve the observed boot stall.

Evidence logs are local generated artifacts, excluded from Git:

* `/tmp/x86-nested-contract-baseline-20260908.log`: initial reference PASS / old Direct error 3 failure.
* `/tmp/x86-nested-full-matrix-20260908.log`: six-case suite plus debug/release standard matrix PASS.
* `/tmp/x86-nested-sse-final-suite-20260908.log`: final six-case suite PASS including SSE at 256 cycles per backend.
* `/tmp/x86-nested-sse-final-host-20260908.log`: final 123 host executions and format check PASS.
* `/tmp/x86-nested-sse-final-build-20260908.log`: final debug x86 build PASS.
* `/tmp/x86-nested-windows-reference-20260908.log`: reference Hyper-V PASS.
* `/tmp/x86-nested-windows-hyperv-20260908.log`: Direct Hyper-V timeout; private work directory
  `/tmp/thin-hv-nested-windows-hyperv.vctJUP`, final screen/counters under
  `monitor-hyperv-failure-diagnostics.pnlb3P/`.

Intermediate test-development failures were not hidden: a missing `std::path::Path` qualifier
stopped the first xtask host build; a too-short expected terminal marker rejected a successful
Direct contract until the gate required its exact zero StartImage status; the initial SSE
probe incorrectly expected the KVM FPU ioctl's MXCSR member to be transferred. A subsequent
reference diagnostic verified that SET/GET copied XMM seed bytes before entry, but the first
actual L2 execution received zeros (`/tmp/x86-nested-sse-state-diagnostic-20260908.log`),
consistent with the unmarked XSAVE component described above. Both were fixture-initialization
failures before Direct was attempted, not accepted backend passes. Actual one-time guest
initialization passed one-cycle A/B checks and then the final 256-cycle suite on both backends.

Use the existing Nix development environment and repository test framework:

```sh
nix develop --accept-flake-config --command cargo fmt --check
nix develop --accept-flake-config --command cargo xtest -p x86_uefi_loader
nix develop --accept-flake-config --command cargo xtest -p xtask
nix develop --accept-flake-config --command cargo xtest -p x86_64_hal
nix develop --accept-flake-config --command cargo xtest -p nested_vmx
nix develop --accept-flake-config --command cargo xtest -p x86_guest_uefi_test
nix develop --accept-flake-config --command cargo xbuild x86
nix develop --accept-flake-config --command cargo xrun x86
nix develop --accept-flake-config --command cargo xrun x86 --release
```

The nine-case xrun matrix distinguishes Direct-VMX, its separate expected-root-exception fixture,
outer-KVM/reference, physical-chainload under KVM,
preflight under both KVM and TCG, and the new physical policy fixture under TCG.
`X86_UEFI_BACKEND` specifies the expected backend before launch;
a mismatching/missing backend marker or forbidden runtime/overlay marker fails the run. It is not
inferred from guest output. QEMU staging uses disposable filesystems, not a physical Windows ESP.
Keep at most three QEMU processes active. UEFI/Linux runners share staging, serial logs and a
variable store, so run them serially. Concurrent Windows jobs require separate writable disk
overlays, matching variable stores/TPM state, work-directory locks and VNC endpoints. Their
shared backing images must remain immutable.

### Platform EPT / private host-state increment, 2026-09-08

This increment started on `feat/x86-thin-monitor` at `2ba44e4`, with only the user's existing
AGENTS.md edit. Branch/status and the latest twenty commits were inspected before editing;
the branch was not switched. Implementation commits are `298f096` (HAL materializer) and
`193e6e0` (private VM-exit host state, connected preflight audit and test gates).

The concrete architectural changes are:

* `ept::{required_platform_pages,build_platform_identity,PlatformEpt}` exhaust the validated
  platform plan before publishing a usable result. Sizing counts table intervals rather than
  walking every 4 KiB leaf, so enormous unsupported layouts fail capacity checks promptly.
  The contiguous arena must be explicitly private, including its unused tail. Errors clear
  its root. All leaf types retain guest PAT participation. GPA walk width and paging-structure
  physical width are checked separately. There is no heap or QEMU fallback in this builder.
* `platform_snapshot::with_snapshot` captures only the CPU/MTRR state currently consumed by
  inventory and construction, without repeated MSR reads. `platform_ept_audit::with_storage`
  reserves 256 table pages plus a 32-page descriptor scratch area before GetMemoryMap, checks
  captured physical/canonical bounds before accessing those pages, builds the actual map with
  all 288 pages excluded, and releases all temporary storage. EPTP is never installed.
  This is UEFI-map coverage only: PCI root windows, BAR relocation space and all APIC/device
  apertures still need platform discovery. `mmio_complete=0 direct_vmx_ready=0` is mandatory.
* `host_state::{HostStack,HostEnvironment}` initializes one private GDT/TSS page, one IDT page
  and four independent 8 KiB IST stacks. The existing 16 KiB host stack is retained. All 256
  IDT gates terminate through an assembly-only bounded COM1/CLI/HLT handler; root NMI handling
  deliberately stops rather than silently claiming forwarding. The new storage adds 40 KiB
  to the single-CPU monitor block (101 pages total). It is runtime-reserved but is **not yet
  excluded from the active QEMU carrier EPT**. No guard-page or physical SMP claim is made.
* `run_direct_monitor` validates allocations and conditional VMX-MSR presence before access,
  rejects unsupported CET/LA57 host state, and prepares private host fields before VMXON.
  `write_host_state` no longer copies firmware descriptor tables, selectors, FS/GS or SYSENTER
  targets. The cross-crate manifest test requires all sixteen initialized host fields to have
  exactly one Direct-VMCS `HostState` patch. Immediate VMfail does not install these descriptors;
  post-entry terminal paths retain storage and never return to firmware. Pre-entry failures
  free the block only after successful VMXOFF if VMX was enabled; failed VMXOFF stops with storage retained.
* `host-exception-test` builds separately named loader/runtime images for QEMU only. After a
  successful native guest return it raises #UD in VMX root. Its strict gate requires the ordered
  private-host/guest/armed/exception records and timeout status 124; ordinary Direct-VMX rejects
  those markers. Do not deploy these test artifacts. Preflight gates likewise distinguish an
  actual KVM EPT-build PASS from TCG's explicit no-VMX SKIP. Expected build/run errors now return
  xtask status 1 instead of aborting with a core dump.

| Evidence class | Check | Result for this increment |
| --- | --- | --- |
| Host unit tests | All five affected packages / feature rows | PASS: 116 Rust test executions: HAL 39, loader 39 (direct 8, fault fixture 8, reference 5, chainload 7, preflight 11), nested 7, xtask 25, physical-policy driver/payload 4+2. |
| Build/format | `cargo fmt --check`, x86 debug/release builds | PASS; no new external crate, root dependency change or AArch64 production-source change. The shared xtask change is error reporting only; AArch64 hardware was not tested. |
| QEMU/KVM + TCG | Debug and release nine-case xrun matrices | PASS, 18 cases total: seven KVM and two TCG cases per profile. The root #UD fixture is an expected-stop test, not a successful OS boot. |
| QEMU/KVM, preflight | Actual UEFI/MTRR EPT construction | PASS in both profiles: 17 table pages, 6,385 leaves, all 288 temporary pages excluded. No VMX enabled and no physical EPT installed. |
| QEMU/KVM, Direct-VMX | Real Linux KVM L2 lifecycle | PASS: 64 ordered create/run/destroy cycles and clean poweroff with private carrier/direct-L2 host fields. One vCPU only. |
| QEMU/KVM, reference | Same Linux lifecycle control | PASS: 64 cycles and clean poweroff; not project-L0 evidence. |
| QEMU/KVM, Direct-VMX | Ordinary Windows desktop | PASS: fresh release run, exact desktop response, validated pre-success counters and runner exit zero. Screen shows the desktop and PowerShell. No L2 entry was recorded; this is not Hyper-V, SMP, S5 or daily-use proof. |
| QEMU/KVM, reference | Windows Hyper-V control | PASS: fresh overlay, expected Hyper-V marker and runner exit zero. |
| QEMU/KVM, Direct-VMX | Windows Hyper-V | FAIL: 1200-second marker deadline and runner exit 1. Both mid-run and final screens show “Please wait”, not a BSOD. The private host-state change does not establish Hyper-V usability. |
| Host negative integration | Unavailable QEMU executable | PASS: expected status 1, no VM started and no xtask panic/core dump. |
| Physical hardware | Original Windows/Linux and firmware identity | UNVERIFIED. No physical PC was booted or modified in this increment. |

Logs are `/tmp/x86-platform-{complete-validation,matrix-retest,final-host,committed-validation}-20260908.log`,
`/tmp/x86-private-host-linux-lifecycles-20260908.log`,
`/tmp/x86-private-host-windows-{normal,reference,hyperv-final}-20260908.log`, and
`/tmp/x86-xtask-run-error-check-20260908.log`. The first full matrix failed because the new
fault-arming records contained CRCRLF (SerialPort already expands LF); the output was corrected
and both complete matrices passed without weakening the gate. Two Hyper-V fixture setup attempts
failed before QEMU launch because copied base vars/TPM prerequisites were missing; neither is an
OS boot result. The successful desktop and Hyper-V/reference jobs use separate writable QEMU
state, immutable backing images and independent VNC endpoints.

The failed Direct Hyper-V run has two validated 144-byte diagnostic snapshots. The mid-run
capture briefly stopped and explicitly resumed the owned QEMU, using the existing runner's
ownership/status helpers; this is diagnostic evidence, not an uninstrumented performance run.
Only bounded decoded JSON was retained, not the raw record. The snapshots are in
`/tmp/thin-hv-hyperv-midrun-counter.EDS27j/counters.json` and
`/tmp/thin-hv-windows-private-host-hyperv.Un5FVd/monitor-hyperv-failure-diagnostics.8AUOPs/counters.json`.

| Counter | Mid-run | Timeout | Change |
| --- | ---: | ---: | ---: |
| L1 exits | 57,024,008 | 84,983,611 | 27,959,603 |
| Observed L2 entries | 6,570,335 | 9,838,744 | 3,268,409 |
| Reflected L2 exits | 6,570,334 | 9,838,744 | 3,268,410 |
| External-interrupt exits | 746,945 | 1,067,637 | 320,692 |
| Interrupt-window exits | 499,215 | 713,554 | 214,339 |
| INVEPT / INVVPID | 6,773 / 6,154 | 6,773 / 6,154 | 0 / 0 |
| Recorded nested entry failures | 0 | 0 | 0 |

Mid-run capture occurred during an L2 RDMSR exit before its reflection completed; the one-count
entry/reflection difference is consistent with that phase boundary. Timeout capture occurred at
an L1 VMREAD exit. These changing counters rule out the entire monitor having stopped between
the snapshots, but do not establish forward progress in Windows initialization, correct interrupt
delivery, preservation of MSR/XSAVE state, or the root cause of the failure. There is no new
DPC_WATCHDOG_VIOLATION observation and no justified physical-daily-use claim.

### Earlier foundation results, 2026-09-08 (before the increment above)

PASS means the named runner's observed gate, not completion of an architectural stage. Logs
listed here are local, untracked `/tmp` evidence; no disk images, variable stores, TPM state,
screenshots or dumps are committed with this document.

| Evidence class | Check | Recorded result and limit |
| --- | --- | --- |
| Host unit tests | `x86_64_hal`, `nested_vmx`, loader feature rows | PASS: HAL 23/23, nested 7/7, loader 26 executions (direct 7, reference 5, physical-chainload 7, preflight 7). Includes three new diagnostic state/ABI tests. |
| Host unit tests | `xtask` | PASS: 23/23 including backend/physical-policy/lifecycle gates, exact LF/CRLF regression and the decoder's eight synthetic checks. |
| Host unit tests | New physical-policy driver/payload rows | PASS: driver 4/4, payload 2/2. Includes storage-controller/status policy, not proof of physical firmware behavior. |
| Build | Backend and physical-policy artifacts | PASS: debug/release builds and non-VMX artifact checks. The connected-fixture release run includes the new exit counters. |
| QEMU/KVM and QEMU TCG | Eight-case debug/release xrun matrices | PASS: six KVM backend cases and two TCG cases (preflight and physical policy), in each profile. Both include the connected fixture and new counters. |
| QEMU TCG | Five-case physical policy fixture and secondary-ESP visibility | PASS after fixture-only generic mass-storage controller connection: both ESPs proved visible, same-ESP Windows/Linux loading succeeds, other-ESP-only target returns NOT_FOUND, malformed options return INVALID_PARAMETER, and self-path returns ACCESS_DENIED. The initial missing-controller run failed before case 1 and is not counted as coverage. |
| QEMU/KVM, Direct-VMX | Linux real KVM single probe | PASS: observable deterministic KVM_EXIT_IO result. No physical hardware result. |
| QEMU/KVM, Direct-VMX | Linux create/run/destroy lifecycle | PASS: 64 ordered successful KVM_RUN I/O probes, process exit zero, final poweroff. One L1 vCPU; no L2 SMP claim. |
| QEMU/KVM, outer-KVM/reference | Linux lifecycle control | PASS: the same 64-cycle gate and poweroff, independently labelled reference. |
| QEMU/KVM, outer-KVM/reference | Linux soak and reboot | PASS: phase 2 reports 80 hashes, 1000 L2 probes, verified scratch-disk data after reboot and clean poweroff. A clocksource watchdog read-timeout warning is present; this is not a warning-free or latency-bound claim. |
| QEMU/KVM, outer-KVM/reference | Linux S3/deep suspend | PASS: three resume cycles, CPU re-online, nested KVM, scratch EFI-runtime variable checks and poweroff. Those scratch-variable tests are QEMU-only. |
| QEMU/KVM, outer-KVM/reference | Windows Hyper-V | PASS: expected Hyper-V marker through the reference loader. This does not validate the project Direct-VMX L0. |
| QEMU/KVM, Direct-VMX | Windows Hyper-V | FAIL: repeated 1200-second marker deadline. Both inspected screens showed “Please wait”, not a BSOD. The instrumented run records 9,648,662 observed L2 entries and reflections, with zero recorded nested entry failures. Repeated L2 execution is not Hyper-V usability or a diagnosed watchdog cause. |
| QEMU/KVM, Direct-VMX | Ordinary Windows desktop probe | PASS after correcting the LF/CRLF matcher, on a fresh disposable run with a 600-second deadline and final backend/capture/exit-status gates. Previous 900-second runs were false-negative test results: both COM1 desktop and 25 valid COM2 replies were present. This is a desktop probe, not a clean Windows S5, reboot, SMP or daily-use result. |
| QEMU/KVM, outer-KVM/reference | Windows WSL daily soak | Original run FAIL overall: the guest completed its 60-minute/two-round target (421 actual rounds), but did not shut down within the following 120 seconds. After using explicit guest S5 instead of the host's ACPI-button request, a fresh two-round/zero-minute regression PASSes including reboot, workload verification and clean poweroff. The repaired 60-minute run has not been repeated. |
| QEMU/KVM, outer-KVM/reference | Windows WSL2 S4 | PASS: WSL2 verifier, S4 request, clean poweroff, cold restart, exact `state=S4 guest_resume=1` result and final clean shutdown. |
| QEMU/KVM, disposable evaluation Windows | Physical-status synthetic SelfTest | PASS on the third run and again with the final LF/CRLF matcher: exact zero-query JSON, child exit zero, COM2 PASS and clean poweroff. The first two attempts failed without a result; desktop screenshots did not establish a parser error. The successful revision waits for desktop readiness and executes the same encoded command in a console. |
| Physical hardware | Original Windows/Linux, activation status, devices, reboot, Hyper-V/WSL2, S3/S4/deep suspend | UNVERIFIED. No QEMU result changes this classification. |

Host evidence is in `x86-host-final-20260908.log`, `x86-hal-final-20260908.log` and
`x86-new-fixture-host-build-20260908.log`; the final affected-package run
`x86-all-affected-final-20260908.log` records 84 passing Rust test executions. The final
`x86-final-source-validation-20260908.log` records 85 passing executions, format check and x86 build.
Seven-case matrix evidence is in
`x86-qemu-matrix-20260908.log` and `x86-qemu-release-20260908.log`. Linux evidence is in
`x86-linux-direct-single-20260908.log`, `x86-linux-{direct,reference}-cycles-20260908.log`,
`x86-linux-reference-soak-20260908.log` and `x86-linux-reference-s3-20260908.log`.
Windows evidence is in `x86-windows-{reference,direct}-hyperv-20260908.log`,
`x86-windows-direct-normal-20260908.log`, and
`x86-windows-physical-selftest{,-screen}-20260908.log`. Finalization is tracked separately in
`x86-policy-final-validation-20260908.log`, `x86-windows-reference-wsl-soak-20260908.log`, and
`x86-windows-physical-selftest-console-20260908.log`. Pending runs must be checked through their
final process status as well as their exact guest markers before updating this table.
The completed reference S4 run is recorded in `x86-windows-reference-s4-20260908.log`.
The short deterministic-shutdown regression completed with exit zero in
`x86-windows-soak-shutdown-retest-20260908.log`; this is not a second 60-minute soak.
`x86-final-build-qemu-20260908.log` records successful `cargo fmt --check`, `cargo xbuild x86`,
the final debug eight-case matrix, and another 64-cycle Linux run for each backend after the
counter changes. AArch64 source paths and root dependency manifests remain unchanged.

`x86-policy-final-validation-20260908.log` records completed host counts of xtask 21, fixture
driver 3, fixture payload 2, and loader feature rows 4/5/7/7, all with zero failures. Its debug builds
and first seven QEMU checks completed before the secondary-ESP precondition failure. This
supersedes the earlier batch that stopped because the fixture's xtest rows were not yet
published; that earlier batch never reached its build step. The subsequent connected-controller
release matrix completed successfully in `x86-policy-connected-validation-20260908.log`.

The Direct-VMX diagnostics publish one monitor-owned 144-byte, versioned BSP-only record before
VMX entry. Saturating counters separate L1 exits, direct entry attempts, observed L2 entries,
reflections, L0-only handling, interrupt exits, invalidations and entry failures. Odd sequence
values reject interrupted updates; these are diagnostic counters, not a per-CPU state model.
The actual `x86_64-unknown-uefi` release assembly contains no SIMD instructions in the new hot
counter updater or its scalar call arguments. This does not fix existing CR2/XSAVE state gaps.
Direct Windows timeout capture reads only that exact record after validating its published
address against the resident image, and saves decoded counters plus a screen; it does not dump
guest memory, MSRs, firmware tables or product keys. Counter availability is independent of
mandatory backend and functional success checks.

The fresh instrumented runs are in `x86-windows-instrumented-{normal,hyperv}-20260908.log`.
Their timeout captures both decoded successfully. Ordinary Windows records 2,632,579 L1 exits,
including 2,474,707 external-interrupt, 52,689 interrupt-window and 105,176 CPUID exits, with no
nested attempts. Hyper-V records 83,440,827 L1 exits and 9,648,662 observed L2 exits/reflections;
its last reason 24 denotes an L1 VMRESUME exit. The one-count difference between L1 exits and
completed L0 handling matches a snapshot taken before the current handler completed. These
cumulative single snapshots do not identify a late stall, delivered interrupts, per-exit latency,
or state corruption. `l0_only_handled_exits` includes completed L1 handlers selecting L2 entry;
it is not a count of L2 exits swallowed by L0. INVEPT/INVVPID counters count intercepted
instructions, not proof of successful invalidations. The active Windows reproducers predate
the final success-path gate hardening; both nevertheless failed at their original deadlines,
and diagnostic acquisition did not change either result.

Byte-level review of the ordinary repro subsequently found 25 exact desktop replies ending
in CRLF on COM2, as well as the COM1 desktop marker. The old exact matcher accepted only LF,
so it rejected valid replies and continued opening the Run dialog until the deadline. The
last screenshot was one such retry, not evidence of lost keyboard input. The correction must
accept only LF or one terminal CR before LF, without accepting prefixes, suffixes or embedded
carriage returns; successful boot/probe revalidation is recorded separately from this original
false-negative runner result.
The corrected matcher is covered by exact LF/CRLF, malformed-record and large-input/pipe-drain
host tests. Fresh QEMU/KVM validation completed with exit zero in
`x86-windows-normal-crlf-retest-20260908.log` and
`x86-windows-physical-selftest-final-20260908.log`. The ordinary desktop runner may terminate
QEMU with HMP `quit` after its powerdown wait; unlike SelfTest/soak/S4, its PASS does not establish
clean guest poweroff.
The final Direct runner also replays the shared backend/fatal gate after QEMU has exited, so
a terminal monitor error appended after pre-success capture cannot be hidden by HMP `quit`.
A further fresh ordinary run with that final check passed in
`x86-windows-normal-final-20260908.log`; the existing late-failure transcript tests remain the
host regression coverage. No keyboard timing or Hyper-V capability was changed to obtain PASS.

The first physical-chainload attempt correctly propagated
the test payload's DEVICE_ERROR: that fixture hid the CPUID hypervisor bit but not the KVM
signature leaf. Adding the same QEMU `kvm=off` setting used by the reference fixture fixed the
test environment, without changing production CPUID or relaxing guest/status checks. An earlier
format check and the initial fixture/test failures are not counted as passes.

### Repeating the Linux and Windows checks

[run-linux-kvm-test.sh](../scripts/x86_64/run-linux-kvm-test.sh) builds the existing Linux UKI and
uses the existing UEFI runner. The guest executes a fresh real KVM probe process per cycle; each
must report KVM_RUN I/O at port `0xe9` with `L2OK`, exit zero and appear in exact sequence. Missing,
duplicate, out-of-order, foreign-backend and late failure evidence is rejected.

```sh
nix develop --accept-flake-config --command env LINUX_KVM_BACKEND=direct-vmx LINUX_KVM_CYCLES=64 scripts/x86_64/run-linux-kvm-test.sh
nix develop --accept-flake-config --command env LINUX_KVM_BACKEND=outer-kvm LINUX_KVM_CYCLES=64 scripts/x86_64/run-linux-kvm-test.sh
nix develop --accept-flake-config --command scripts/x86_64/run-linux-soak-test.sh
nix develop --accept-flake-config --command scripts/x86_64/run-linux-suspend-test.sh
```

The lifecycle default deadline is 300 seconds, with a bounded configurable cycle count. The
[soak runner](../scripts/x86_64/run-linux-soak-test.sh) and
[suspend runner](../scripts/x86_64/run-linux-suspend-test.sh) remain outer-KVM/reference only.
These bounded runs are not day-long daily-use or controlled performance comparisons; boot
uptimes and log wall times do not establish physical virtualization overhead.

[windows-test.sh](../scripts/x86_64/windows/windows-test.sh) distinguishes `monitor` and
`monitor-hyperv` (Direct-VMX) from `trusted-kvm-hyperv`, `trusted-kvm-wsl-soak` and related
reference modes. Use only disposable evaluation work directories with independent qcow2
overlays and matching cloned vars/TPM for mutable Windows regression modes. Never run their
installation, disk preparation, Hyper-V/WSL enablement or variable-store helpers on the original
physical installation.

The new `check-physical-status` mode internally snapshots the evaluation disk and clones its
vars/TPM, attaches read-only test media and restricts networking. It runs only
[physical-status-test.ps1](../scripts/x86_64/windows/physical-status-test.ps1), which calls the
collector's `-SelfTest`, checks child exit zero and exact zero-query JSON, and requests clean
shutdown. The mounted test volume must be unique; there is no assumed drive letter. The latest
diagnostic path waits for the established desktop probe, opens a persistent console, sends the
unchanged encoded command, and records screenshots so launch/parser/media failures can be
distinguished. A screenshot alone never passes the test or proves a particular failure cause.

```sh
nix develop --accept-flake-config --command scripts/x86_64/windows/windows-test.sh check-physical-status
```

This command is an evaluation-QEMU test, not the physical status-collection procedure. Its gate
requires `hardware_queries=0`, the exact SelfTest PASS marker, no late failure, and clean QEMU
exit; a desktop marker alone is insufficient. The default overall deadline is 600 seconds,
with bounded probe and final shutdown waits. Finish source validation with `cargo fmt`.

## Safe original-Windows validation sequence

Keep the original Windows Boot Manager and a firmware-direct bypass boot selection intact.
Never replace bootmgfw.efi, repartition disks, alter BCD, enroll/remove Secure Boot keys, change TPM
ownership, or change activation data as part of these tests. No key extraction, synthetic MSDM,
SMBIOS forgery, or activation workaround is permitted. An unsigned EFI image rejected by the
existing Secure Boot policy is a failed compatibility gate, not a reason to weaken that policy.

Before any physical boot experiment, confirm the firmware-direct bypass actually boots the
original installation and that the existing authorized disk-encryption recovery procedure is
available through its normal secure channel. Do not extract, print, copy into this repository,
or include recovery secrets in diagnostic logs. Do not disable/suspend disk-encryption
protection, clear the TPM, or modify protectors to make a test pass. A recovery prompt is a stop
condition: return through the known-good firmware path and follow the existing recovery process,
not an automated hypervisor repair step. Keep an operator-controlled way to select that bypass
even when the project image cannot start.

Secure Boot must accept the project EFI image through an already authorized signing/trust path.
The build does not enroll keys or bypass firmware verification. If that trust path is unavailable,
stop and resolve it with the machine's operator; do not disable Secure Boot or change its databases
as an implicit test step. Never replace the original Windows Boot Manager with a project artifact.

1. Boot the original installation directly through firmware. Record activation status only and
   verify normal physical devices and reboot.
2. Start the non-VMX physical-chainload artifact from the intended existing ESP through a
   reversible, operator-controlled firmware selection. Confirm the COM1 backend marker, normal
   Windows boot, unchanged activation status, devices and reboot.
3. Do not perform the physical Direct-VMX step until stages 3–8 and all Windows-visible pCPUs are
   ready. Then repeat the same baseline/status/device/reboot checks with project-L0 provenance.
4. Only after ordinary Windows is stable, validate already-configured Hyper-V startup and WSL2
   L2 work. Any feature enablement or installation is a separate, explicit operator action.
5. Test hibernation/deep suspend separately; neither chainloading nor pointer inventory proves
   firmware resume correctness. Keep the firmware bypass available after every failed run.

The read-only collector is [physical-status.ps1](../scripts/x86_64/windows/physical-status.ps1).
Its `-SelfTest` parameter set exercises synthetic status policy without hardware queries. Normal
collection requires an explicit `-BootLabel firmware`, `physical-chainload`, or `direct-vmx`,
and emits status-only JSON to stdout; it does not retrieve product-key fields. A single licensed
base Windows product is required to confirm activation; missing, ambiguous or failed queries
remain unconfirmed. Do not treat an unconfirmed result as authorization to modify licensing.

The status collector's boot-path label is operator-supplied and must be corroborated by serial
provenance; it cannot attest that a project L0 ran. Do not reuse the QEMU UEFI scratch-variable
payload or automated QEMU disk/firmware preparation on the original Windows installation.
