# Direct-VMCS correctness work, started 2026-09-09

Local authoritative branch: `feat/x86-thin-monitor`. Starting HEAD: `3623b62`.
Before edits, `git branch --show-current`, `git status --short` and
`git log --oneline -30` were recorded. The only initial worktree modification
was the user's `AGENTS.md`; it is retained and excluded from these commits.
No branch switch, Claude/subagent use, new dependency or physical-machine test.

This is an incremental implementation record, **not completion of the physical
L0 architecture**. Outer-KVM results are reference evidence only. Original OEM
Windows, activation data, firmware identities and physical storage are untouched.

## Findings checked against the starting source

| Requested finding | Source evidence and initial status |
| --- | --- |
| 1. CPUID.OSXSAVE / leaf 0D | Confirmed: `vmx_smoke::dispatch_l1_exit` runs host CPUID and changes only VMX/hypervisor bits. L0 enables OSXSAVE. XCR0/XSS are still shared, so leaf 0D currently uses live guest state. |
| 2. Invalid XSETBV | Confirmed: dispatcher passes guest operands directly to hardware, with a comment explicitly deferring #GP validation. |
| 3. L0 extended-state preservation | Confirmed: `vmexit_entry` saves GPRs only; no monitor XSAVE/CR2 scratch frame surrounds Rust calls or failed nested entry. |
| 4. Original nested host validation | Confirmed: `handle_l1_vmentry` reads then patches original host fields without complete pre-patch host-state validation. |
| 5. VMX memory faults | Confirmed: `read_l1_linear_u64`, `write_l1_linear_u64` and `paging::translate_long_mode` collapse failures to `Option`; callers stop. The old walk does not check write permissions. |
| 6. MSR lists | Confirmed: nonzero entry/load/store counts trigger `unsupported nested VM-entry state`. `MsrMirrorMetadata` and the patch manifest already exist; do not duplicate them. |
| 7. Control provenance | Confirmed: policy type exists, but `vmexit_dispatch` unconditionally reflects an active `NESTED_RUN`. |
| 8. VPID lifetime | Confirmed: INVVPID forwards L1 descriptors to hardware; VPID is advertised without an L0-owned allocation/lifetime policy. |
| 9. Platform map wiring | Confirmed: active carrier calls `ept::build_identity_8g` and `build_host_identity_8g`. Planner/materializer exist but are not the active carrier map. |
| 10. Bootstrap ownership | Confirmed: initial guest RIP is the runtime monitor's `guest_entry`; excluding that image would remove executing L1 code. |
| 11. Variable overlay | Confirmed: `run_direct_monitor` calls `runtime_variables::install` unconditionally after VMXON. Physical non-VMX chainload is already separate and deterministic. |
| 12. pCPU state | Confirmed: `L1_VCPU_STATE`, carrier/direct patch caches, entry policy, `NESTED_RUN` and diagnostics remain BSP globals; allocation also assumes one VMXON/carrier/host environment. |
| 13. Root NMI | Confirmed: `host_state` explicitly stops on every root IDT vector. Carrier external-interrupt/window handling already exists but is not root-NMI forwarding. |
| 14. EPT large pages | Confirmed: the trusted EPT capability mask omits large-page bits; the platform materializer's support is not evidence for nested capability correctness. |
| 15. Reflection overhead | Confirmed: coarse bounded counters exist; VMREAD/VMWRITE/VMPTRLD counts and complete validated reflection dirtiness tracking do not. |
| 16. S3 lifecycle | No resume/re-entry lifecycle exists in the active BSP monitor; the previous Direct S3 capability-loss failure remains a regression, not fixed by this first step. |

Existing foundations are retained rather than reimplemented: private host
GDT/IDT/TSS/stacks, the checked platform memory planner and EPT materializer,
deterministic physical chainloading, header-only MSDM presence detection,
Direct PAT/EFER handling, architectural VM-instruction-error recording (including
read-only VMCS hardware), and the existing backend/transcript gates.

## Step 1: L1 CPUID and XSETBV

Changed files and major symbols:

* `arch_hal/x86_64_hal/src/xstate.rs`: pure `visible_cr`, `leaf1_for_cr4`,
  `validate_xsetbv` and `XsetbvFault`, plus host policy tests.
* `arch_hal/x86_64_hal/src/lib.rs`: exports the policy module.
* `x86_uefi_loader/src/vmx_smoke.rs`: `l1_visible_cr4`, CPUID/XSETBV branches of
  `dispatch_l1_exit`, existing #GP/#UD injection helpers' safety documentation.
* `x86_guest_uefi_test/src/l1_xstate.rs`: scoped L1 exception fixture, exact-RIP
  fault recovery, OSXSAVE toggles, XCR0 writes and negative exception probes.
* `x86_guest_uefi_test/src/nested_contract.rs`: executes the L1 fixture before
  VMXON and includes completed coverage in its strict final marker.
* `scripts/x86_64/run-uefi-smoke.sh`: `check_nested_contract_log` requires all new
  XSTATE coverage fields. Missing/zero coverage cannot pass.
* `xtask/src/main.rs`: extends
  `nested_contract_gate_requires_complete_architectural_and_cleanup_evidence`.

L1's CR4 is reconstructed as `(GUEST_CR4 & !MASK) | (SHADOW & MASK)`. Only the
dynamic CPUID.1 OSXSAVE bit changes; static XSAVE availability is preserved.
Leaf 0D remains hardware-derived while L0 does not change XCR0/XSS during CPUID.
Any later private L0 XCR0/XSS switch must also synthesize its dynamic sizes from
saved guest state; this first step does not claim extended-register preservation.

XSETBV checks #UD priority, CPL, XCR index, the advertised XCR0 bitmap, mandatory
x87 and SSE/AVX, MPX, AVX-512 and AMX dependencies before hardware execution.
Faults inject #GP(0) or #UD without advancing RIP or changing XCR0. Only ECX and
EDX:EAX low halves are significant. No additional capability is advertised.
Rules were checked against the [Intel SDM, volume 1 section 13.3 and XSETBV](https://cdrdv2-public.intel.com/868137/325462-089-sdm-vol-1-2abcd-3abcd-4.pdf)
and the locally pinned Linux 7.1.5 `__kvm_set_xcr` implementation.

The fixture runs as L1, not through Linux's L2 CPUID implementation. Its temporary
IDT preserves firmware gates except #GP/#UD; synchronous recovery accepts only
the exact probe instruction RIP. Every ordinary success/failure path restores
IDTR, CR4, XCR0 and IF before returning. Invalid index, absent x87, AVX without
SSE, unsupported bits and OSXSAVE-clear #UD all require exception delivery and
continued execution. No Runtime Services or firmware identity writes are used.

### Validation

Before-fix monitor SHA256:
`afc952fde2c9d5aac58b2cea2e1cd2206226152468c2ceb5934fe431a58d6c04`.
The new fixture against that unchanged monitor **FAILs as expected**:
`l1-cpuid-osxsave actual=0x1 expected=0x0`. Evidence:
`/tmp/x86-correctness-step1-before-direct.log`.

Executed host package commands (inside `nix develop --accept-flake-config --command`):
`cargo xtest -p x86_64_hal`, `-p nested_vmx`, `-p x86_uefi_loader`,
`-p x86_guest_uefi_test`, `-p xtask`: **136 PASS, zero failures**
(43 + 8 + 45 + 9 + 31). Existing `xtest.txt` package entries need no change.
Evidence: `/tmp/x86-correctness-step1-{hal,nested_vmx,x86_uefi_loader,x86_guest_uefi_test,xtask}.log`.

`cargo xbuild x86`: **PASS**, `/tmp/x86-correctness-step1-xbuild.log`.
Release QEMU command:

```sh
nix develop --accept-flake-config --command env \
  LINUX_KVM_CYCLES=4096 LINUX_KVM_TIMEOUT_SECONDS=600 \
  cargo xrun x86 --nested --release
```

Evidence: `/tmp/x86-correctness-step1-nested-release.log`: **all six suite cases
PASS**, runner exit 0. The four native nested-contract backend/profile
combinations pass, including the Direct L1 exception probes. Both backends
complete all 4096 lifecycle cycles, including long-mode, CR2, XMM0-15/MXCSR,
debug/MSR, paging/remap and TSC checks, then clean S5. Reference success is not
attributed to Direct; its own run passes independently. No Windows or physical
machine was retested in this step. Other candidate issues remain open.

A final privilege audit also handles VM86 as CPL3 independently of CS selector
bits. The loader's 45 host tests and the entire release nested suite, including
both 4096-cycle runs, pass again after that adjustment:
`/tmp/x86-correctness-step1-loader-final.log`,
`/tmp/x86-correctness-step1-nested-final.log`. Final release monitor SHA256:
`35cecfea722d2678bf1208cc2f95a2b6ddccf581286c1deff7eb27126f05627d`.
`cargo fmt`, `cargo fmt --check` and `git diff --check` pass.

Further audit identified another dynamic CR4-dependent bit, CPUID.7.0 OSPKE,
which needs the same L1-state treatment; it is not silently claimed fixed by the
OSXSAVE-specific change. Existing INVVPID already rejects types above 3 before
reading its descriptor, whereas INVEPT still lacks the corresponding ordering
check. These distinctions will guide the next increments.

## Step 1 follow-up: CPUID.OSPKE

The same private HOST_CR4 issue also affects CPUID.7.0:ECX.OSPKE. The new
`xstate::leaf7_for_cr4` changes only that dynamic bit using L1-visible CR4.PKE;
it does not advertise PKU when hardware lacks it. Other leaf 7 subleaves remain
unchanged. This follows the Intel SDM CPUID/WRPKRU definition, not an assumption
that all host CPUID values should be virtualized.

`l1_xstate::protection_keys` tests four CR4.PKE transitions when PKU is present.
It saves PKRU and temporarily permits all keys using a register-only sequence
before accessing data with PKE enabled; it restores PKRU and original CR4
without intervening data access. The absent-PKU branch verifies OSPKE is clear
without executing unsupported control-register changes. The contract marker
reports both `pku` and `ospke_toggles`; the existing shell/xtask gate checks their
relationship and rejects omitted or contradictory coverage.

Changes: `xstate.rs`, `vmx_smoke::dispatch_l1_exit`, `l1_xstate.rs`,
`nested_contract::Prerequisites`/final marker, `check_nested_contract_log`, and
its existing xtask fixture test. No new framework or package entry.

Before-fix Direct **FAIL, as expected**: `l1-cpuid-ospke actual=0x0 expected=0x1`,
using the step-1 monitor SHA256 recorded above. Evidence:
`/tmp/x86-correctness-ospke-before-direct.log`.
The four affected host package runs all pass: **129 tests, zero failures**
(HAL 44, loader 45, guest fixture 9, xtask 31). Evidence:
`/tmp/x86-correctness-ospke-{x86_64_hal,x86_uefi_loader,x86_guest_uefi_test,xtask}.log`.
The unchanged `nested_vmx` package's eight tests passed in step 1.

The same 4096-cycle release nested command **passes all six cases**, with both
backends' full lifecycle coverage, clean S5 and host exit 0:
`/tmp/x86-correctness-ospke-nested-release.log`. All four instruction-contract
combinations pass with `pku=1 ospke_toggles=4`.
Two additional isolated runs with `X86_UEFI_CPU=host,+vmx,-hypervisor,kvm=off,-pku`
also pass the existing runner and transcript gate, reporting
`pku=0 ospke_toggles=0`, not pretending to exercise unavailable hardware:
`/tmp/x86-correctness-ospke-no-pku-{outer-kvm,direct-vmx}.log`.
Release monitor SHA256:
`6a1071c374bac144ce1a506facce744bb31ab78f59230139e584d764ae81130e`.

### Unmodified upstream tests after `d0149e2`

The existing `scripts/x86_64/run-linux-selftest.sh` ran the locally pinned,
unmodified Linux 7.1.5 selftest executables in both backends. Each invocation
used `LINUX_SELFTEST_BACKEND`, `LINUX_SELFTEST_NAME` and `LINUX_SELFTEST_ELF`;
no timeout or upstream source was changed.

| Test | outer-kvm/reference | direct-vmx/project L0 |
| --- | --- | --- |
| `xcr0_cpuid_test` | PASS | PASS |
| `vmx_exception_with_invalid_guest_state` | PASS | FAIL: killed at the existing 600-second guest bound, exit 137 |

Total: **3 PASS, 1 FAIL**. Logs:
`/tmp/x86-correctness-d0149e2-<test>-<backend>.log`.
The original periodic-signal invalid-state Direct failure remains open; a
reference PASS is not a workaround or Direct evidence.

## Step 2: precise VMX memory-operand faults

Changed files and major symbols:

* `arch_hal/x86_64_hal/src/paging.rs`: `DataAccess`, `DataFault`,
  `operand_range`, `untag`, `translate_data`, canonicality and permission checks.
  Removed the now-unused `Option`-only `translate_long_mode`, retaining its
  Linux-stack/five-level regression through the new walker.
* `x86_uefi_loader/src/vmx_smoke.rs`: `l1_memory_operand`, `l1_data_access`,
  `read_l1_vmx_pointer`, m64/m128 readers, m64 writer,
  `access_l1_paging_word`, `inject_l1_operand_fault`; wired VMXON, VMCLEAR,
  VMPTRLD, VMPTRST, memory VMREAD/VMWRITE, INVEPT and INVVPID callers.
* `x86_guest_uefi_test/src/l1_fault.rs`: shared exact-RIP #UD/#SS/#GP/#PF
  fixture, including CR2 capture and scoped IDTR/CR2/IF restoration.
* `x86_guest_uefi_test/src/l1_xstate.rs`: uses that shared fixture; unchanged
  OSXSAVE/OSPKE/XSETBV assertions and CR4/XCR0 cleanup.
* `x86_guest_uefi_test/src/l1_memory.rs`: private 4-/5-level L1 paging fixture,
  bad-pointer, permissions, exception-priority and discontiguous crossing tests.
* `x86_guest_uefi_test/src/nested_contract.rs`: allocates/checks the additional
  seven WB fixture pages, calls the probes before VMXON, without a current VMCS,
  and with a current VMCS, and requires completed coverage in its final marker.
* `scripts/x86_64/run-uefi-smoke.sh`, `xtask/src/main.rs`: strict coverage gate
  and missing/contradictory evidence tests. Global INVEPT/INVVPID are explicit
  prerequisites for this expanded contract, not silently skipped instructions.

The walker preserves not-present versus protection/reserved-bit #PF and
P/W/U/RSVD/PK error bits. It accumulates RW/US across levels, honors CR0.WP,
SMAP/AC and live PKRU/PKRS, validates NX/reserved address bits and large-leaf
alignment/support, and sets architectural A/D bits. Data LAM untagging precedes
whole-operand canonicality/overflow checks; invalidation descriptor *contents*
are not LAM-untagged. SS noncanonical accesses produce #SS(0), others #GP(0).
The carrier's virtual CR0/CR4, not private host values, drive permissions.

Fault injection preserves RIP/VMX flags and publishes #PF's linear address in
CR2. VMREAD field validity and invalid INVEPT/INVVPID types precede operand
access; VMWRITE reads its source before field-encoding failure. Missing-current
VMCS results retain VMfailInvalid. Every destination byte is checked before the
first payload store; discontiguous pages are not treated as a contiguous host
pointer. No guest bad linear pointer becomes a generic VMfail or monitor panic.

This remains the **single-BSP fixed-map backend**. Paging-word A/D updates are
serialized by the stopped sole L1 CPU, not a claim of SMP. An inaccessible L0
physical backing range or invalid monitor context stays a distinct fail-closed
diagnostic: it must not be fabricated as an architectural nonpresent L1 PTE.
Platform-map integration and pCPU ownership remain required. CR2 is still live
shared state; the later L0 scratch-frame work must preserve injected CR2 rather
than restore an obsolete L1 snapshot.

Native contract coverage now requires **16 #PF, 8 #GP, 1 #SS, 6 successful
page crossings and 8 exception-priority cases**, plus all prior VMX/XSTATE
checks. Each fault is accepted only at the exact test instruction RIP, returns
to its assembly continuation, and checks its error code and (for #PF) CR2.
The physical order of the two payload pages is reversed. All original firmware
mappings remain in a copied root; CR3/CR0 and exception state are restored before
VMCS cleanup and FreePages.

Architecture references: [Intel VMX instruction reference](https://cdrdv2-public.intel.com/825750/326019-sdm-vol-3c.pdf),
[Intel architectural PKRS MSR definition](https://www.intel.com/content/dam/develop/external/us/en/documents-tps/335592-sdm-vol-4-testsize.pdf),
and the pinned Linux 7.1.5 `vmx_get_untagged_addr`, `get_vmx_mem_address`,
INVEPT/INVVPID handlers and MMU protection-key rules.

### Validation and reference discrepancy

The first complete stage-2 release run used the same command as step 1, with
4096 cycles and the unchanged 600-second lifecycle bound. Direct native and
read-only-VMCS contracts **PASS**; both Linux lifecycle backends **PASS** all
4096 cycles and clean S5. The two outer-KVM contract profiles **FAIL** the newly
added no-partial-store assertion. Suite total: **4 PASS, 2 FAIL**, exit 1;
`/tmp/x86-correctness-step2-nested-release.log`.

The reference failure is preserved, not accepted by the PASS gate. The fixture
was then extended to record partial-store counts without aborting the remaining
memory cases or the old VMX contract. It still fails at the end if any partial
store occurred. Both reference profiles report `partial_stores=1`; both Direct
profiles report zero. All expanded exception/crossing/priority probes complete.
The host kernel is 7.1.5. Its corresponding source's
`kvm_write_guest_virt_helper` writes a page chunk before translating the next
chunk, consistent with the observed VMPTRST first-word modification followed by
#PF on the second page. This is a failure of this suite's strict no-partial-store
safety requirement, **not proof of physical Intel behavior or an independently
established SDM violation**. No host KVM code was modified to conceal the result.

Final stage-2 verification, after the complete/no-current fixture expansion:

* `cargo xtest -p x86_64_hal`, `-p nested_vmx`, `-p x86_uefi_loader`,
  `-p x86_guest_uefi_test`, `-p xtask` through `nix develop`: **144 PASS,
  0 FAIL** (51 + 8 + 45 + 9 + 31).
  `/tmp/x86-correctness-step2-final-<package>.log`.
* `nix develop --accept-flake-config --command env LINUX_KVM_CYCLES=4096
  LINUX_KVM_TIMEOUT_SECONDS=600 cargo xrun x86 --nested --release`:
  **4 PASS, 2 FAIL**, exit 1, same reference-only partial-store failures.
  `/tmp/x86-correctness-step2-nested-final.log`.
  Direct and reference each complete all 4096 lifecycle cycles with the existing
  KVM_RUN I/O, XMM0–15/MXCSR, CR2, paging, MSR/debug/TSC and teardown checks.
  Guest-observed lifecycle completion: reference 38.960 s, Direct 299.527 s;
  these are instrumented probe timings, not desktop-performance claims.
* `nix develop --accept-flake-config --command cargo xbuild x86`: **PASS**;
  `/tmp/x86-correctness-step2-xbuild.log`.
* `cargo fmt`, `cargo fmt --check`, `git diff --check`: **PASS**.
  Diff review found no AArch64 production changes. `AGENTS.md` remains the
  pre-existing user change and is not staged. Generated artifacts are not staged.

These are host unit and **QEMU/KVM** results only. No new QEMU TCG, Direct Windows
normal-boot/Hyper-V/WSL2, S3 or physical-machine result is claimed in steps 1–2.
The known Direct periodic-signal invalid-state, Hyper-V and S3 failures are not
resolved by the memory-fault work. Outer-KVM remains reference evidence only.

## Step 3: original L1 host-state validation before patching

Changed files and major symbols:

* `nested_vmx/src/host_validation.rs`, `nested_vmx/src/lib.rs`: exported pure
  `Limits`, `Error` and `validate`, with eight new policy tests.
* `arch_hal/x86_64_hal/src/vmx.rs`: `reject_host_entry`, a guaranteed-early
  hardware failure that preserves the original selector and opaque error field.
* `x86_uefi_loader/src/vmx_smoke.rs`: `l1_host_validation_limits` and the
  pre-patch validation/rejection branch in `handle_l1_vmentry`.
* `x86_guest_uefi_test/src/nested_contract.rs`: `host_field_boundaries`,
  `reject_host_field`, shared `read_field`, complete original-field restoration
  checks, and explicit cold/final coverage markers.
* `scripts/x86_64/run-uefi-smoke.sh`, `xtask/src/main.rs`: require all three
  host-validation coverage fields and reject missing/incomplete evidence.

The validator checks original CR0/CR4 fixed bits (without guest-mode
relaxations), CET/WP dependence, CR3 physical width and LAM exceptions, all seven
selector RPL/TI constraints and applicable null restrictions, canonical FS/GS,
TR/GDTR/IDTR and SYSENTER addresses, host mode and RIP, and conditional PAT/EFER
and CET fields already in the patch manifest. No VMX capability is added.
Unpatched PERF_GLOBAL_CTRL remains subject to hardware validation.

Architectural distinctions matter: HOST_CR0 CD/NW are not checked/switched by
VM entry/exit; descriptor/MSR canonicality uses maximum CPU width, while RIP
uses the CR4.LA57 value loaded on exit. HOST_RSP itself has no VM-entry
canonicality check, and SYSENTER_CS is an MSR field, not a host segment selector
subject to RPL/TI checks. They retain their original VMCS-width values instead
of acquiring invented VMfail conditions. These rules were checked against the
[Intel host-state checks, sections 27.2.2–27.2.4](https://cdrdv2-public.intel.com/825750/326019-sdm-vol-3c.pdf),
[Intel LAM VM-entry rules, section 6.6.2](https://cdrdv2-public.intel.com/782879/architecture-instruction-set-extensions-programming-reference.pdf),
and Linux 7.1.5 `nested_vmx_check_host_state` and canonicality helpers.

For an invalid original host value, the monitor first materializes **all**
original patch fields, including MSR-list counts/addresses. It temporarily
sets HOST_CS to zero and executes the requested VMLAUNCH/VMRESUME. This cannot
reach guest state or MSR-list loading, but preserves higher-priority launch and
control checks. The hardware-maintained error is captured, the original CS and
carrier selection are restored, and normal VMX completion publishes that same
error. This works with read-only VMCS exit fields: no VMWRITE-to-error assumption,
opaque VMCS access or VMCS12/VMCS02 translation is introduced. Materialized
fields invalidate the existing patch/policy caches before returning to L1.

Native probes cover **34 invalid host values**, plus VMRESUME-on-clear error 5
and invalid-control error 7 priority over an invalid host selector. Each invalid
host case requires VMfailValid/error 8 and VMREAD of the original bad value,
then restoration. All twenty baseline host fields and modified control/MSR
fields are checked after restoration. No valid nested entry is attempted by
this instruction-contract fixture; real L2 entry remains the Linux KVM probe.

### Validation

* Initial pure-policy run: `cargo xtest -p nested_vmx`, **16 PASS, 0 FAIL**;
  `/tmp/x86-correctness-step3-policy.log`.
* All five package runs through `nix develop`: **152 PASS, 0 FAIL**
  (nested_vmx 16, HAL 51, loader 45, guest fixture 9, xtask 31);
  `/tmp/x86-correctness-step3-<package>.log`.
* Intermediate release suite with `LINUX_KVM_CYCLES=1`: Direct native and
  read-only-VMCS contracts and both Linux backends PASS; the same two reference
  no-partial-store assertions FAIL. `/tmp/x86-correctness-step3-native-first.log`.
* Final release command uses `LINUX_KVM_CYCLES=4096
  LINUX_KVM_TIMEOUT_SECONDS=600 cargo xrun x86 --nested --release` through
  `nix develop --accept-flake-config --command`: **4 PASS, 2 FAIL**, exit 1.
  `/tmp/x86-correctness-step3-nested-release.log`. Both Direct contract profiles
  require `host_invalid=34 host_priority=2 host_restore=1`; both reference
  profiles also emit the completed host-check marker before failing the separate
  partial-store requirement. Both backends complete 4096 Linux L2 cycles.
* `cargo xbuild x86` through `nix develop`: **PASS**;
  `/tmp/x86-correctness-step3-xbuild.log`.
* `cargo fmt`, `cargo fmt --check`, `git diff --check`: **PASS**. Complete diff
  review found no AArch64 implementation changes; the user's `AGENTS.md` and
  all generated EFI/guest/firmware/log artifacts remain excluded from staging.

These remain QEMU/KVM and host results only. CET VM-exit capability remains
hidden; conditional policy tests are not a claim of a running CET nested guest.
Windows/Hyper-V, S3, per-pCPU ownership, platform-map wiring and physical hardware
are not newly qualified by this step.

## Step 4: protect live guest extended state from L0

Confirmed against `b6001f7`: the exit stub saved GPRs but called compiled Rust
without saving its x87/SSE register footprint. It also treated future CR2
switching as desirable, although VMX does not switch CR2 between L1 and L2.

Changed files and major symbols:

* `arch_hal/x86_64_hal/src/host_state.rs`: `HostXstate`,
  `HOST_XSTATE_MXCSR_OFFSET`, `populate`, `HostEnvironment::vmcs_fields` and
  the per-instance ownership/layout test.
* `x86_uefi_loader/src/vmx_smoke.rs`: private CR4 preparation,
  `vmexit_entry`, `vmexit_dispatch`, `nested_vmentry_failed`,
  `clobber_host_xmm`, `halt_with_guest_xstate`, `leave_vmx`,
  `leave_failed_launch`, terminal handlers and CR2/reflection documentation.
* `x86_uefi_loader/Cargo.toml`: explicit QEMU-only `host-xstate-test` feature.
* `x86_guest_uefi_test/src/l1_extended.rs`: aligned register images, a naked
  seed/execute/capture/restore probe, defined-byte comparison and layout test.
* `x86_guest_uefi_test/src/l1_xstate.rs`: legacy/AVX/interrupt state checks and
  FP preservation around all valid and faulting XSETBV probes.
* `x86_guest_uefi_test/src/nested_contract.rs`: state-checked rejected entries,
  complete coverage marker and unambiguous native-guest diagnostic prefix.
* `x86_guest_uefi_test/src/l1_memory.rs`: the same native-guest prefix change.
* `scripts/x86_64/run-uefi-smoke.sh`: strict extended-state coverage and
  separately identified `host-xstate` contract profile.
* `scripts/x86_64/run-linux-kvm-test.sh`: explicit Direct-only
  `LINUX_KVM_HOST_XSTATE_TEST=1`, requiring the armed marker exactly once.
* `xtask/src/main.rs`: reuse the host-fixture builder for the new image,
  linked-image ISA audit and its unit test, native/Linux clobber profiles,
  separate evidence filenames and transcript regression checks.

### Architectural contract

The existing per-CPU host allocation has a disjoint unused region in its GDT/TSS
page. At offset 256 it now owns a 64-byte-aligned 512-byte FXSAVE64 image and
private MXCSR value. No allocation or global XSTATE lock was added. VMCS
HOST_GS_BASE selects this instance; the existing patch-manifest completeness
test also covers this nonzero base on direct L2 exits. This is a per-instance
scratch foundation, **not implementation of physical SMP/AP ownership**.

Before each Rust entry, including an immediate nested-entry failure and the
terminal carrier-resume failure, the assembly saves live x87/MMX, XMM0–15 and
MXCSR. FNINIT and private MXCSR establish masked FP state for L0. The stub
restores the saved live state before entering L1 or L2. On an L2 exit it restores
the **L2 exit's state**, not an older L1 snapshot. Post-exit terminal paths
restore after diagnostics and retain private maps/tables/GS. Only initial
launch failure restores firmware controls before freeing inactive allocations.
Genuine root faults still fail closed with the private snapshot retained.

L0 does not change XCR0/XSS for its own work: the only running XCR0 write is the
validated emulation of L1 XSETBV. Consequently CPUID leaf 0D still sees the live
guest's enabled-state configuration. A legacy save is necessary even with
XCR0.SSE clear: this does not disable SSE register use. L0 deliberately uses no
AVX/AVX-512/AMX/opmask or XSAVE/XRSTOR instructions. A compilation guard and
decoded linked-image audit reject those paths, including dependency code.
The guest fixture independently checks all sixteen YMM registers where AVX is
advertised. ZMM/AMX/CET execution is not newly qualified by this test.

CR2 is never restored from a stale L1 snapshot. L0's ordinary path cannot change
it; the intentional L1 #PF injector does, and genuine root #PF never resumes.
The native probe checks CR2 through CPUID, XSETBV, interrupt and failed-entry
paths; the existing real/long-mode KVM probe continues testing L2 CR2.
L0 does not write PKRU, PKRS, XSS, DR0–3, DR6 or PMU state. Its private CR4 clears
PKE/PKS, preventing guest PKRS from denying private supervisor-page accesses;
the temporary PKRU read in the operand walker restores private CR4. VMX-managed
DR7/DEBUGCTL, PAT/EFER and PERF_GLOBAL_CTRL behavior is not replaced by an
arbitrary software context switch. No new capability bit is advertised.

Intel's [system-programming description of OSFXSR](https://cdrdv2-public.intel.com/835754/253668-sdm-vol-3a.pdf)
defines its role in saving/restoring XMM/MXCSR. Intel also explicitly permits
conservative XSAVE init tracking: XINUSE may remain set for an init-valued
component; it is not a promise to detect every init value.
See the [XSAVE tracking description](https://cdrdv2-public.intel.com/812380/252046-sdm-change-document.pdf).

### Regression coverage and current results

The native fixture now requires `fx_cpuid=6 fx_xsetbv=12 fx_entry=41 fx_irq=3`
and `ymm_rounds=4` when AVX is available (`0` otherwise). This includes the five
architectural XSETBV exceptions and 41 failed VMX entries, with live non-default
x87/MXCSR and distinct values in every XMM register. STI;HLT windows use the
unmodified live firmware timer/IDT and clear IF before capture; all three Direct
profiles observed ten external-interrupt exits in the 4096-cycle suite's native
part. The QEMU-only monitor deliberately clears every XMM register on every
dispatch and failed-entry Rust path. Production images contain no such clobber.

An additional transcript bug was found during review: the old native guest
diagnostics used the same `thin-hv: L1` prefix rejected as resident-monitor
evidence on reference backends. Rename those two guest diagnostics to
`thin-hv: native L1`, and include them in transcript unit fixtures. Backend
guards and the strict no-partial-store assertion are **not weakened**.

* Five package-specific host runs: **154 PASS, 0 FAIL** (HAL 51, guest 10,
  loader 45, nested_vmx 16, xtask 32), through `nix develop` and `cargo xtest -p`.
  Logs: `/tmp/x86-correctness-step4-<package>.log`.
* Release nested suite with `LINUX_KVM_CYCLES=4096
  LINUX_KVM_TIMEOUT_SECONDS=600 cargo xrun x86 --nested --release` through
  `nix develop --accept-flake-config --command`: **6 PASS, 2 FAIL**, exit 1.
  `/tmp/x86-correctness-step4-nested-final.log`. Direct native, read-only VMCS
  and clobber contracts PASS; reference/Direct/Direct-clobber Linux each finish
  all 4096 cycles and S5 poweroff. Only the two known reference partial-store
  assertions FAIL. No timeout was increased and no selftest source was changed.
* After the native diagnostic-prefix/precondition checks, the same release
  suite with `LINUX_KVM_CYCLES=1`: **6 PASS, 2 FAIL**, the same reference-only
  failures; `/tmp/x86-correctness-step4-nested-recheck.log`.
* Initial explicit Direct clobber/native run: **PASS**;
  `/tmp/x86-correctness-step4-native-first.log`. The first one-cycle full suite
  was **4 PASS, 3 FAIL**: the two known reference failures plus a duplicate CR
  in the test-only armed marker rejected by the strict profile gate. Correct
  the marker, not the gate. `/tmp/x86-correctness-step4-suite-first.log`.
  The subsequent explicit interrupt/clobber suite was **6 PASS, 2 FAIL**;
  `/tmp/x86-correctness-step4-suite-irq.log`.
* `cargo xrun x86 --release` through `nix develop`: **9 QEMU runs PASS, 0 FAIL**.
  `/tmp/x86-correctness-step4-smoke.log`. Seven are QEMU/KVM (Direct smoke,
  expected root-exception stop, three reference source/target selections,
  physical-chainload fixture, physical-preflight); two are QEMU/TCG (VMX-absent
  preflight and the five-case physical-chainload policy harness). The expected
  root #UD diagnostic is a fixture PASS, not a production exception recovery
  claim. None of these runs boots physical Windows.
* Original pinned `xcr0_cpuid_test` via `run-linux-selftest.sh` with
  `LINUX_SELFTEST_BACKEND=direct-vmx`: **PASS**, process exit 0 / assertions 1.
  `/tmp/x86-correctness-step4-xcr0_cpuid_test-direct-vmx.log`.
* Original pinned `vmx_exception_with_invalid_guest_state`, same runner and
  backend with the unchanged comparison bound
  `LINUX_SELFTEST_TIMEOUT_SECONDS=600`: **FAIL**, QEMU status 124, PASS marker
  missing after the periodic-signal phase began. Source/timing are unchanged;
  this is not fixed by the FP bracket.
  `/tmp/x86-correctness-step4-vmx_exception_with_invalid_guest_state-direct-vmx.log`.
  Both upstream ELFs are the existing
  `/tmp/thin-hv-kvm-selftests-7.1.5.MUloxc/out/x86/<test>` artifacts; their runner
  variables are `LINUX_SELFTEST_NAME` and `LINUX_SELFTEST_ELF`.
* Direct native contract with
  `X86_UEFI_CPU=host,+vmx,-hypervisor,kvm=off,avx=off,avx2=off`: **PASS**,
  including the separate `--check-nested-contract-log direct-vmx native` gate
  and `ymm_rounds=0`; `/tmp/x86-correctness-step4-no-avx.log`.
* `cargo xbuild x86`: **PASS**, including the baseline linked-ISA audit for
  normal and both host fixtures; `/tmp/x86-correctness-step4-xbuild.log`.
* `cargo fmt`, `cargo fmt --check`, `git diff --check`: **PASS**;
  `/tmp/x86-correctness-step4-fmt-check.log`. No AArch64 production path changed.

All results above are host or QEMU/KVM results, not physical hardware. Windows
Hyper-V has not been retested in this step. The earlier Direct periodic-signal
invalid-state timeout, platform-map/AP/NMI/S3 work and nonempty MSR lists remain
open until their own validation, regardless of the state-preservation PASS.

## Step 5 prerequisite: guarded root RDMSR

The capability-read fallback still executed raw RDMSR. An absent model-specific
MSR could therefore fault in L0 while preparing L1's capability response. MSR-list
handling also needs a way to distinguish an unreadable MSR from a monitor fault;
ordinary RDMSR success is **not** proof that hardware MSR-list use is legal.

* `arch_hal/x86_64_hal/src/host_state.rs`: `HostXstate` now owns three initially
  zero recovery words per CPU. `try_rdmsr` arms exactly one instruction and always
  disarms before returning. Its private #GP gate accepts only #GP(0), the private
  ring-zero CS and the armed RIP, preserving GPRs/CR2/FP state and using IST4.
  All other #GPs, root #UDs and NMIs remain fail-closed. The gate is included in
  retained-image checks; descriptor storage size does not increase.
* `x86_uefi_loader/src/vmx_smoke.rs`: `l1_vmx_capability` uses the guarded access.
  Absent MSRs follow the existing L1 #GP injection path without RIP advancement.
  The explicit host-XSTATE fixture probes reserved MSR `0xffffffff` before each
  VMX_BASIC read, requiring recovery followed by a successful valid read. This
  runs inside the existing FP-preservation bracket, with no hot serial logging.

Validation, all through `nix develop --accept-flake-config --command`:

* `cargo xtest -p x86_64_hal`: **51 PASS, 0 FAIL**;
  `/tmp/x86-correctness-step5-hal-guard.log`.
* `cargo xtest -p x86_uefi_loader`: **45 PASS, 0 FAIL**;
  `/tmp/x86-correctness-step5-guard-loader.log`.
* `cargo xtest -p x86_guest_uefi_test`: **10 PASS, 0 FAIL**;
  `/tmp/x86-correctness-step5-guard-guest.log`.
* `cargo xtest -p xtask`: **32 PASS, 0 FAIL**;
  `/tmp/x86-correctness-step5-guard-xtask.log`.
* `env LINUX_KVM_CYCLES=1 LINUX_KVM_TIMEOUT_SECONDS=600 cargo xrun x86 --nested
  --release`: **6 PASS, 2 FAIL**, process exit 1;
  `/tmp/x86-correctness-step5-guard-nested.log`. All three Direct native cases
  and all three Linux cases pass. Both reference native cases retain the known
  crossing-page partial-store failure. No assertion was weakened.
* `cargo xrun x86 --release`: **9 QEMU runs PASS, 0 FAIL**, process exit 0;
  `/tmp/x86-correctness-step5-guard-smoke.log`. Seven KVM and two TCG cases,
  including the expected fatal root #UD fixture, as in step 4.
* `cargo xbuild x86`, `cargo fmt`, `cargo fmt --check`, `git diff --check`:
  **PASS**; build/format logs are `/tmp/x86-correctness-step5-build-guard.log`
  and `/tmp/x86-correctness-step5-guard-fmt.log`.

This is a tested prerequisite, not completed nonempty MSR-list support. The
list runtime and its VM-entry-failure/VMX-abort semantics are still under work.
Windows/Hyper-V and physical hardware were not tested in this prerequisite;
outer-KVM evidence remains reference-only.

## Step 5: original MSR-list control validation

Confirmed: zeroing Direct exit-list fields before entry could hide L1's invalid
list controls; nonzero counts previously reached a monitor stop. Invalid list
metadata now takes the architectural early-failure path, before any list memory
access. Valid nonempty list execution is still a separate pending part of step 5.

* `nested_vmx/src/msr_list.rs`: bounded `List`, 16-byte `Entry` and operation-
  specific format checks. Full-range validation distinguishes count, alignment,
  overflow and physical-width errors; count zero ignores the address, and
  physical zero is not categorically rejected. Entry contents are not confused
  with early control failures. Three new host tests cover the boundaries.
* `nested_vmx/src/lib.rs`: `MsrMirrorMetadata::checked_lists` reuses that checker
  for both original exit ranges, with existing metadata-test coverage extended.
* `arch_hal/x86_64_hal/src/vmcs.rs`: named encodings for all three list addresses.
* `arch_hal/x86_64_hal/src/vmx.rs`: `reject_control_entry` and shared
  `reject_entry_field` generate hardware VM_INSTRUCTION_ERROR, without attempting
  to write the read-only error field. A verified reserved primary control bit
  guarantees failure before guest/MSR loading; the original field is restored.
  VMRESUME launch-state failure still has priority over error 7.
* `x86_uefi_loader/src/vmx_smoke.rs`: `checked_direct_msr_lists` reads original
  metadata, retaining the distinction between L0 read/invariant failure and
  L1-invalid controls. `handle_l1_vmentry` restores original patch fields and
  selects the appropriate early control/host rejection without stopping for
  malformed list controls. No capability bits changed.
* `x86_guest_uefi_test/src/nested_contract.rs`: `msr_list_boundaries` checks
  alignment, full-range physical overflow and excessive counts for each list,
  plus launch-state priority and ignored empty-list addresses. HOST_CS=0 is an
  independent safety guard, so a failed assertion cannot launch arbitrary L2
  state or dereference test addresses. All fields are restored on failure.
* `scripts/x86_64/run-uefi-smoke.sh`, `xtask/src/main.rs`: strict coverage gates
  now require `msr_invalid=12 msr_priority=12 msr_ignored=3` and `fx_entry=68`.
  Missing/decreased counts are negative runner-test cases.

Validation through the existing Nix/cargo framework:

* Five package tests: **157 PASS, 0 FAIL** (nested 19, HAL 51, loader 45,
  guest 10, xtask 32). Logs: `/tmp/x86-correctness-step5-list-policy.log`,
  `...-list-hal.log`, `...-list-loader.log`, `...-list-guest.log`,
  `...-list-xtask.log`.
* `env LINUX_KVM_CYCLES=1 LINUX_KVM_TIMEOUT_SECONDS=600 cargo xrun x86 --nested
  --release`: **6 PASS, 2 FAIL**, process exit 1;
  `/tmp/x86-correctness-step5-list-nested-first.log`. All added Direct cases
  pass, including the deliberately faulting root-MSR/XSTATE fixture. The only
  failures remain both reference native partial-store assertions.
* `cargo xbuild x86`: **PASS**; `/tmp/x86-correctness-step5-list-xbuild.log`.
* The same release suite with `LINUX_KVM_CYCLES=4096`: **6 PASS, 2 FAIL**,
  process exit 1; `/tmp/x86-correctness-step5-list-nested-final.log`.
  All three Linux runs completed 4096 cycles with explicit teardown and clean
  poweroff; all Direct native cases pass. Both reference failures are the same
  partial-store assertions, not a Direct regression. Guest lifecycle elapsed
  times: reference 38.341233s, Direct 314.687588s, Direct FP-clobber/root-MSR-fault
  fixture 313.779621s. These are instrumented QEMU lifecycle timings, not
  physical-machine or daily-use performance claims.
* `cargo fmt`, `cargo fmt --check`, `git diff --check`: **PASS**;
  `/tmp/x86-correctness-step5-list-fmt.log`. No AArch64 production changes.

No Windows/Hyper-V, S3 or physical-hardware qualification was performed here.

## Stage 5 prerequisite: private PAT/EFER and real nested control matrix

Confirmed at `52534f0`: the carrier left L1 PAT and non-mode EFER bits live in
L0, and Direct entry accepted only selected all-clear or paired LOAD control
combinations. This is now replaced by private L0 restoration, without adding
capability bits or replacing Direct-VMCS.

* `arch_hal/x86_64_hal/src/host_state.rs`: `bind_monitor_data` / `monitor_data`
  bind an opaque, retained CPU-owned object through private GS. Separate
  environments have separate bindings; no borrowed object crosses VM entry.
* `nested_vmx/src/lib.rs`: the 33-field patch manifest additionally retains
  original entry/exit controls and guest PAT/EFER, including their ignored values.
* `nested_vmx/src/msr_list.rs`: `PatEfer::entry` / `exit` implement inheritance
  and original LOAD semantics. `failed_load` tests ordered successful-prefix
  reconstruction for the subsequent nonempty-list work; this helper is not yet
  connected to a nonempty-list runtime.
* `x86_uefi_loader/src/vmx_smoke.rs`: the carrier and Direct VMCS always restore
  private L0 PAT/EFER. `prepare_direct_msr_fields` loads the values that original
  L1 controls specify, using stopped-carrier state rather than L0 RDMSR values.
  `reflected_direct_msrs` updates guest-field shadows only when L1 requested SAVE,
  and applies L1 host LOAD controls before resuming its carrier. Original control
  words remain distinct from forced words throughout validation and caching.
  CPU-private inherited state uses one reserved page; unused future list arrays
  were removed before committing. Most other VMX state is still BSP-global.
* `x86_guest_uefi_test/src/nested_contract.rs`: ignored guest PAT/EFER values
  survive immediate failed entry (`guest_msr_shadow=2`). Allocation-map checking
  is reused with a const-generic page count by the additional fixture.
* `x86_guest_uefi_test/src/msr_contract.rs`, `Cargo.toml`: the existing nested
  binary's `msr-contract` feature performs real L2 entries for all 64 combinations
  of six PAT/EFER LOAD/SAVE controls, with two EFER.SCE polarities: **128 cases**.
  L1 live, guest-field, L2-written and host-field PAT values differ. EFER checks
  distinguish inherited/loaded and saved/unsaved state without toggling NXE or
  changing the page tables. The disposable q35 fixture owns its descriptor/IST
  storage, clears the VMCS, executes VMXOFF and powers off; it never returns to
  firmware with disposable host descriptors. No firmware identity is changed.
* `scripts/x86_64/run-uefi-smoke.sh`, `xtask/src/main.rs`: build/stage the extra
  feature image and run it in both backends through the existing runner. Strict
  gates require ordered cases 0..127, VMXOFF, backend provenance and Direct
  private-host evidence. Missing/duplicated/reordered cases, NUL, wrong counts,
  root failure or an unexpected firmware return fail host runner tests.

Validation through `nix develop --accept-flake-config --command`:

* All five `cargo xtest -p <package>` checks: **161 PASS, 0 FAIL** (nested 21,
  HAL 51, loader 47, guest 10, xtask 32). Logs are
  `/tmp/x86-msr-matrix-{nested-policy,hal,loader,guest,xtask}.log`; the final
  one-page CPU-state loader retest is `...-small-state-loader.log`.
* `cargo xbuild x86 --release`: **PASS**, including all three monitor ISA gates;
  `/tmp/x86-msr-matrix-xbuild.log`. The one-page state is also rebuilt by xrun.
* The standalone real-entry matrix: **128/128 PASS per backend**, **2 QEMU runs
  PASS**, with required poweroff. Logs: `/tmp/x86-msr-reference-first.log` and
  `/tmp/x86-msr-direct-first.log`. Reference evidence is not Direct evidence.
* An accidental `LINUX_KVM_CYCLES=4096 cargo xrun x86 --nested --release`
  invocation omitted the previously used `LINUX_KVM_TIMEOUT_SECONDS=600`.
  Its default 300-second limit produced **6 PASS, 4 FAIL**: two known reference
  partial-store failures and two Direct lifecycle timeouts while still making
  progress. Direct plain reached cycle 3827 without a state assertion failure.
  Log: `/tmp/x86-msr-matrix-nested-4096.log`. This is retained as a failed run,
  not evidence of a new hang or a successful 4096-cycle completion.
* The comparable 4096-cycle/600-second release run is in progress at this
  intermediate checkpoint; its final result is recorded in the follow-up below.
  No runner timeout or upstream selftest source was modified.

Nonempty MSR lists remain unsupported at this checkpoint. Original timing-
sensitive KVM failure, physical platform mapping, pCPU/AP, root NMI and S3 work
remain open. Windows/Hyper-V and physical hardware have not been retested here.

### Comparable 4096-cycle PAT/EFER follow-up

The `c044844` runtime and fixtures completed:

```sh
LINUX_KVM_CYCLES=4096 LINUX_KVM_TIMEOUT_SECONDS=600 \
  nix develop --accept-flake-config --command cargo xrun x86 --nested --release
```

**8 PASS, 2 FAIL**, process exit 1, in
`/tmp/x86-msr-matrix-nested-4096-comparable.log`. All Direct native profiles,
both real-entry MSR matrices and all three Linux lifecycle runs passed. The
only failures are the two previously recorded outer-KVM native partial-store
assertions. The runner did not stop testing after those failures.

All Linux runs completed 4096 cycles and powered off. Guest lifecycle times:
reference **38.203544s**, Direct **321.315544s**, Direct FP-clobber/root-MSR-fault
fixture **321.066644s**. Direct is approximately 2% slower than the prior
314.687588s / 313.779621s measurements; that cost is retained for subsequent
correctness-instrumented optimization, not hidden by changing a timeout. The
600-second setting is the same established extended-lifecycle bound used before
this patch. The mistaken default-300-second run remains a recorded FAIL.

These are QEMU/KVM results only. No Windows, S3 or physical hardware run is
implied by this follow-up. Nonempty-list implementation work is subsequent to
the tested `c044844` snapshot.

## Stage 5: bounded nonempty VM-entry MSR lists

The entry-list half now executes; nonempty **exit** store/load lists remain
gated until the next increment. The architecture remains Direct-VMCS.

* `arch_hal/x86_64_hal/src/platform_memory.rs`: `FirmwareMap<N>` owns a bounded
  validated descriptor copy. Shared `validate_firmware_descriptors` preserves
  the EPT planner's existing format/type/overlap checks; EPT-specific limits
  remain in `PlatformMap::new`. `allows_ram_access` checks full contiguous RAM
  coverage, read/RO permissions, overflow and physical-width boundaries. It
  distinguishes the UEFI WP cache capability from RO protection. Two host tests
  cover holes, MMIO, unusable/unaccepted RAM, read-only/protected pages, physical
  zero, adjacent descriptors, overlap and addresses at the 52-bit limit.
* `nested_vmx/src/lib.rs`: original entry-list address/count join the patch
  manifest (`EntryMsrLoad`, 35 fields). VMREAD/VMWRITE and materialization retain
  L1's original values rather than exposing private list addresses.
* `x86_uefi_loader/src/vmx_smoke.rs`: the CPU's GS-bound `DirectMsrState` owns
  512 aligned entry slots and an immutable 205-descriptor firmware snapshot.
  `prepare_entry` validates backing/ownership before copying ordinary RAM, never
  MMIO or monitor-private image/block storage. Every VMLAUNCH/VMRESUME recopies
  current source contents and publishes only the completely prepared mirror.
  Hardware performs entry MSR loads in order, including model-specific/value
  checks, after architectural entry checks. Invalid contents cause late reason
  34 and its one-based qualification, not VMfail 7 or a root WRMSR exception.
  `reflected_direct_msrs` reconstructs PAT/EFER from only the successful prefix;
  neither original guest-field shadows nor exit stores change on late failure.
  No mutable Rust borrow crosses entry and no per-exit heap allocation is used.
* `x86_guest_uefi_test/src/msr_contract.rs`: 20 additional cases cover one/many/
  512 items, duplicates, unsupported indices, reserved bits, invalid PAT, FS/GS,
  x2APIC and SMM-only exclusions, failure at item 512, preserved prefix state,
  ignored EFER.LMA writes, ignored zero-count address, early errors 5/8, late
  guest-state error 33, late MSR error 34, and recovery. One extra VMRESUME changes
  the source without rewriting list controls and requires the new value.
  Coverage is 7 normal entries (including the empty-list case and one resume),
  10 late MSR failures, 2 early failures and 2 late guest-state failures.
* `scripts/x86_64/run-uefi-smoke.sh`, `xtask/src/main.rs`: require ordered entry
  cases, exact counts and `MSR late-failure guest-field changes=0`. All deferred
  mismatches remain a final FAIL; no reference exception is accepted as PASS.

Additional reference finding, verified against Intel SDM 29.8 and the pinned
Linux 7.1.5 source: after a later item fails, successful earlier PAT list loads
change `guest_ia32_pat` on outer KVM when SAVE_PAT is set. In
`arch/x86/kvm/vmx/vmx.c`, the `MSR_IA32_CR_PAT` write case eagerly updates the
VMCS12 guest PAT field in guest mode. `nested_vmx_enter_non_root_mode` has already
entered guest mode before `nested_vmx_load_msr`; the late failure does not undo
this change. Intel specifies that the guest-state area is unchanged on these
failures. The fixture observes **3 changes on reference, 0 on Direct**. Direct's
original-field shadow avoids this visible corruption. Physical Intel behavior
has not been tested. Only safe field comparisons are deferred so both backends
execute all 20 cases; each subsequent case explicitly resets both fields.

Validation (all cargo commands through the existing Nix environment):

* Five package `cargo xtest -p` runs: **163 PASS, 0 FAIL** (nested 21, HAL 53,
  loader 47, guest 10, xtask 32). Logs: `/tmp/x86-msr-entry-policy.log`,
  `...-hal.log`, `...-loader2.log`, `...-guest.log`, `...-final-xtask.log`.
  The first loader host compile failed on a test-only unqualified `efi` name;
  this was fixed and both affected feature variants rerun successfully.
* `LINUX_KVM_CYCLES=64 cargo xrun x86 --nested --release`: **7 PASS, 3 FAIL**,
  process exit 1; `/tmp/x86-msr-entry-nested-final.log`. All Direct cases and all
  three Linux runs PASS with the unchanged default 300-second bound. Failures:
  two known reference partial-store assertions, plus the new reference PAT
  shadow assertion above. Earlier one-cycle / resume-development runs retained
  the same reference failure; they did not establish reference MSR PASS.
* `cargo xbuild x86` and `cargo xbuild x86 --release`: **PASS**;
  `/tmp/x86-msr-entry-xbuild-debug.log`, `...-xbuild.log`.
* `cargo xrun x86 --release`: **9 PASS, 0 FAIL**, including the expected private
  root-exception stop; `/tmp/x86-msr-entry-smoke.log`. Seven QEMU/KVM cases and
  two QEMU/TCG cases, never physical-machine results.
* `LINUX_SELFTEST_BACKEND=direct-vmx LINUX_SELFTEST_NAME=xcr0_cpuid_test
  LINUX_SELFTEST_ELF=/tmp/thin-hv-kvm-selftests-7.1.5.MUloxc/out/x86/xcr0_cpuid_test
  ./scripts/x86_64/run-linux-selftest.sh`: **PASS**, unmodified upstream ELF,
  process exit 0; `/tmp/x86-msr-entry-xcr0.log`.
* `cargo fmt`, `cargo fmt --check`, `git diff --check`: **PASS**.

Remaining ceiling: a list outside retained firmware RAM or the current fixed
8-GiB host map is an explicit unsupported L0 backing-layout error, not a forged
guest #PF/VMfail result and not a QEMU fallback. The physical map is still not
wired, exit MSR lists still require implementation, and this is not physical
qualification. No AArch64 production path, Windows/activation state, firmware
identity, S3 or physical device was modified or tested in this increment.

## Stage 5: ordered VM-exit MSR stores and L1 host loads

Nonzero exit-list counts no longer stop Direct entry. No VMCS12/VMCS02 layer,
new capability bit or external dependency was introduced.

* `nested_vmx/src/msr_list.rs`: `ExitStoreSource` identifies hardware-saved guest
  fields, private DEBUGCTL/PERF captures, masked VMX capability MSRs, and live
  registers not modified by VMX/L0. Its host test prevents reading private L0
  PAT/EFER/FS/GS/SYSENTER values as if they belonged to L2.
* `x86_uefi_loader/src/vmx_smoke.rs`: the GS-owned `DirectMsrState` now contains
  two bounded capture slots and 512 aligned L1 host-load slots. Hardware stores
  only known-readable DEBUGCTL and, if available, PERF_GLOBAL_CTRL to private
  storage before host loading overwrites them. `store_guest_msrs` reads L1's
  original store list at the actual reflected exit, validates each item, and
  writes only its value half in order. A model-specific read is guarded; invalid
  items cannot raise a root #GP. Earlier stores remain visible on later failure.
  `reflect_l2_vmexit` skips stores on late failed entry, then copies the host
  list **after** all stores, including when the two original lists overlap.
  It publishes the host mirror as the carrier's next entry list. Hardware
  therefore applies L1's ordered MSR writes only after L0 Rust has finished;
  arbitrary L1 host MSRs never run as an L0 WRMSR loop. The first carrier exit
  removes that one-use list via `complete_reflected_msr_load`.
* Invalid exit stores and host loads become `nested_vmx_abort` code 1 or 4,
  respectively. The original VMCS abort indicator is written and this virtual
  CPU is parked with no locks held; private state and VMX ownership stay alive.
  No VMfail, guest instruction advance, firmware return or generic L0 panic is
  substituted. This is still the existing BSP-only monitor; root NMI handling
  and complete physical reset/S3 lifecycle qualification remain separate work.
* `x86_guest_uefi_test/src/msr_contract.rs`: 12 live exit-list cases plus one
  extra VMRESUME cover one/multiple/512 items, duplicates, overlapping lists,
  PAT/EFER and FS/GS/SYSENTER values, DEBUGCTL, capability-mask visibility,
  combined entry/store/host lists, ignored empty addresses, no replay on the
  next CPUID, fresh source reads, and no stores on late failures 33/34.
  Separate `msr-abort-store`/`msr-abort-load` feature images never return from
  their intentionally invalid second item. Only the earlier PAT store and
  four-byte VMCS abort indicator are inspected from outside QEMU.
* `scripts/x86_64/run-uefi-smoke.sh` / `xtask/src/main.rs`: the existing runner
  requires ordered cases and exact counts. Ordinary tests reject all nested
  aborts. The separate terminal fixtures require timeout status 124, matching
  backend provenance, no L1 continuation/root-failure markers, and matching
  physical values read through the existing QEMU monitor. Transcript host tests
  reject wrong/missing/duplicate addresses, codes, values, status, NULs and
  contradictory backend/terminal evidence. Both serial and memory-read evidence
  are retained under ignored `bin/x86_64/` paths. No new test framework or sudo.

Two additional observations must not be mistaken for Direct implementation
success on physical hardware:

1. The first exit-list fixture assumed DEBUGCTL.BTF=2 remained enabled. Both
   Direct and reference observed 0. Pinned Linux 7.1.5
   `prepare_vmcs02_full` / `vmx_get_supported_debugctl` mask unsupported BTF/LBR
   state. The fixture now compares the MSR-store result with an actual L2 RDMSR,
   and prints `requested=2 observed=0` explicitly. This verifies capture of the
   live virtual register, **not** nonzero BTF preservation on physical Intel.
   The first two runs remain recorded FAILs; no existing test was weakened.
2. Reference KVM stops on both invalid exit lists but leaves the VMCS abort
   indicator zero. Its `nested_vmx_abort` only requests a triple fault and logs
   the supplied indicator; it does not write the architectural header. Direct
   writes 1/4 and preserves the earlier PAT store. The strict reference tests
   remain FAIL; the expected value was not changed to zero.

Validation so far (all cargo commands use `nix develop --accept-flake-config
--command`):

* Five package `cargo xtest -p` checks: **165 PASS, 0 FAIL** (nested 22, HAL 53,
  loader 47, guest 10, xtask 33). Logs `/tmp/x86-msr-exit-{nested,hal,loader,guest}-unit.log`
  and `/tmp/x86-msr-exit-xtask-final.log`. The first xtask iteration had one gate
  failure because an impossible reference log containing a project abort marker
  was accepted; the shared backend check was corrected and rerun successfully.
* `cargo xbuild x86 --release`: **PASS**, including baseline-ISA checks;
  `/tmp/x86-msr-abort-build.log` and the nested-suite rebuild.
* Direct standalone positive fixture: **PASS**, matrix 128 + entry 20 + exit 12
  + two extra resumes; `/tmp/x86-msr-exit-direct-second.log`.
* `LINUX_KVM_CYCLES=64 cargo xrun x86 --nested --release`: **9 PASS, 5 FAIL**,
  exit 1; `/tmp/x86-msr-exit-nested-64.log`. All six Direct native profiles and
  all three Linux lifecycle/S5 runs PASS. The five FAILs are reference-only:
  two existing partial-operand-store cases, one existing late-entry PAT shadow
  case, and the two newly checked missing VMX-abort indicators.
* `cargo fmt --check`, `git diff --check`: **PASS**.

* `LINUX_KVM_CYCLES=4096 LINUX_KVM_TIMEOUT_SECONDS=600 cargo xrun x86 --nested
  --release`: **9 PASS, 5 FAIL**, exit 1; `/tmp/x86-msr-exit-nested-4096.log`.
  The same five reference failures persist; every Direct profile and all three
  4096-cycle Linux/S5 runs PASS. Guest cycle-4096 timestamps: reference
  **38.433422s**, Direct **324.798553s**, Direct FP-clobber **323.960386s**.
  The established extended-test bound remains 600 seconds, unchanged. These
  timings are correctness baselines, not a claim of improved performance.
* `cargo xrun x86 --release`: **9 PASS, 0 FAIL**; `/tmp/x86-msr-exit-smoke.log`.
  Seven QEMU/KVM profiles (including the intentional root-exception fixture)
  and two QEMU/TCG profiles. `cargo xbuild x86`: **PASS**;
  `/tmp/x86-msr-exit-build-debug.log`.
* The original unmodified `xcr0_cpuid_test`, using the exact Direct selftest
  command and ELF from the previous increment: **PASS**, exit 0;
  `/tmp/x86-msr-exit-xcr0.log`.
* Final reason classification masks the basic exit reason and entry-failure
  flag instead of comparing the entire reason word. The code-4 Direct fixture
  was rebuilt and rerun afterward: **PASS**, including physical-header readback;
  `/tmp/x86-msr-abort-load-direct-final.log`. Final format/diff checks PASS.

The current 8-GiB host map/RAM-snapshot backing ceiling remains explicit; unsupported physical backing
is not converted into a forged architectural guest fault. No Windows/Hyper-V,
S3 or physical machine was tested for this increment. Outer KVM remains
reference evidence only. No AArch64 production path was changed.

## Control provenance audit and original capability enforcement

The audit found an additional real validation gap: original entry/exit words
with unsupported MSR-state controls still reached a terminal `unsupported nested
VM-entry state` path. Other physical control bits hidden by the advertised L1
mask were delegated to hardware, which knows the physical capabilities, not the
project's narrower virtual contract. This could report host error 8 instead of
the higher-priority invalid-control error 7.

* `nested_vmx::ControlProvenance::requested_supported` checks original L1 bits
  against allowed-zero/one capabilities. Forced L0 bits cannot repair a missing
  required L1 bit or authorize a hidden feature. Unit coverage includes shared
  L0/L1 causing bits, L0-only bits, combined causes and invalid original words.
* `vmx_smoke::l1_direct_controls_supported` validates all five control words
  using the same masked capability function as L1 RDMSR. TRUE/legacy selection
  follows VMX_BASIC, and inactive secondary controls remain ignored. The
  existing guarded early-entry helper records error 7, preserving VMRESUME-on-
  clear error 5 and control-before-host priority, without loading guest state or
  MSRs. The terminal unsupported-control path was removed. No advertised
  capability changed.
* `DIRECT_ENTRY_POLICY` is invalidated on every successful write of pin,
  primary, secondary, entry or exit controls. Only a fully checked set is
  cached, with the original exit word and VMCS owner checked on cache hits.
  `prepare_direct_msr_fields` uses `ControlProvenance::effective` for required
  private PAT/EFER loads/saves; original values remain in the existing manifest.
* `nested_contract::control_bit_boundaries` adds five invalid-word checks, five
  clear-VMRESUME priority checks and one ignored-secondary check. A null
  original HOST_CS independently guards every attempt. Preferred test bits
  include WBINVD exiting, hidden by Direct while physical KVM may support it.
  The exact FP-entry check count becomes 79. `msr_contract::control_cache`
  warms a real launched VMCS, invalidates each word individually, requires error
  7, restores valid controls, and successfully resumes after each failure.
  Secondary controls are already enabled before warming, so that case cannot
  accidentally rely on a primary-control write to invalidate the cache.
* Existing runner/xtask gates require the new cold-boundary counts and the
  complete five-case warm-cache/recovery marker. Missing or altered evidence
  remains FAIL.

The separate candidate claim that current Direct reflection exposes **L0-forced
execution exits** was not confirmed. `configure_and_launch` and
`set_carrier_interrupt_controls` set interrupt controls only on the carrier.
`patch_direct_vmcs` does not copy those controls to L2. Its forced PAT/EFER save/
load bits change state handling, not which L2 instructions/events cause an exit.
Direct's pin/primary/secondary words and interception bitmaps remain L1-owned;
its exits are therefore currently unconditional architectural exits or L1-owned
conditions. The manifest regression now explicitly rejects execution-control,
exception/MSR-bitmap and CR-mask fields. No new forced intercept was invented
solely to make an L0-only branch reachable. **An L0-only Direct exit handler and
end-to-end L2 test are not claimed**; introducing such controls later requires
that handler and MSR-store discard behavior first. Existing carrier L0-only
interrupt handling remains covered by the native interrupt/FP tests.

Validation (cargo via the existing Nix environment):

* Five required `cargo xtest -p` checks: **166 PASS, 0 FAIL** (nested 23, HAL 53,
  loader 47, guest 10, xtask 33); `/tmp/x86-control-final-{nested,hal,loader,guest,xtask}.log`.
  The final manifest addition was separately rerun: 23 PASS;
  `/tmp/x86-control-manifest-unit.log`.
* `LINUX_KVM_CYCLES=64 cargo xrun x86 --nested --release`: **9 PASS, 5 FAIL**,
  exit 1; `/tmp/x86-control-nested-64.log`. All Direct profiles, including both
  new boundary/cache checks, and all three Linux/S5 runs PASS. The five existing
  reference-only failures are unchanged. This increment does not claim a new
  4096-cycle measurement; the preceding MSR commit's measurement is above.
* `cargo xrun x86 --release`: **9 PASS, 0 FAIL**, seven QEMU/KVM and two QEMU/TCG
  profiles; `/tmp/x86-control-smoke.log`.
* `cargo xbuild x86` and the release build performed by xrun: **PASS**;
  `/tmp/x86-control-build-debug.log` and the xrun logs.
* `cargo fmt`, `cargo fmt --check`, `git diff --check`: **PASS**.

Before these control edits, the original unmodified
`vmx_exception_with_invalid_guest_state` ELF was retested against `36283f6`
using the same Direct selftest runner/name/path as earlier: **FAIL, process exit
137**, `/tmp/x86-msr-exit-invalid-guest-state.log`. The runner's unchanged guest
KILL bound is 600 seconds; its existing outer QEMU bound is 900 seconds. The
upstream 200-microsecond signal interval was not modified. The MSR fixes do not
resolve this timing-sensitive regression, and it remains on the performance/
event-correctness work list. No Windows/Hyper-V, S3 or physical machine was
tested here; outer-KVM evidence is reference only.

## Exclusive per-CPU VPID namespace lifetime

Confirmed: Direct accepted L1 tags unchanged without an explicit L0 lifetime.
The carrier already has VPID disabled, so its untagged translations do not need
a nonzero tag. The minimum trusted policy is therefore an exclusive namespace
lease, not per-VMCS tag substitution: VPIDs identify address spaces, and L1 may
legitimately share one between VMCSes.

* `nested_vmx::vpid::Namespace` owns tags 1..65535 for one L1 VMX execution
  period on its pinned CPU. Acquire and release publish no ownership change
  unless a local all-context INVVPID succeeds. Reusing the same VMXON address
  advances a checked generation; wrong owner, duplicate acquisition, failed
  invalidation and generation exhaustion have tested error paths. Independent
  CPU metadata cannot transfer another CPU's lease. No heap is used.
* The private GS-bound `CpuRuntimeState` (renamed from `DirectMsrState`) stores
  the lease beside existing per-CPU MSR mirrors. `handle_l1_vmxon` acquires it,
  `handle_l1_vmxoff` releases it, and nested entry/INVVPID check it. Setup rejects
  a tagged carrier. `invalidate_namespace` uses advertised hardware type 2 on
  the owning root CPU, with the carrier current and no guest executing.
* L1 VPIDs and all four advertised INVVPID types remain unchanged hardware
  operands. This preserves descriptor faults, VPID-zero rules and invalidation
  scope. L1 remains responsible for its own address-space reuse **within** a
  lease. The monitor invalidates the entire namespace between unrelated leases;
  Intel does not require VMXON/VMXOFF themselves to invalidate translations.
  This stronger boundary is an L0 ownership rule, not a new claim about Intel
  instruction semantics. Add real remapping before a tagged carrier or another
  L1 shares this namespace. No capability was hidden or newly advertised.
* `msr_contract::vpid_lifetime` executes actual tagged L2 loads using VPIDs 1
  and 65535, rejects enabled VPID 0 with error 7, and changes a leaf followed by
  each of four INVVPID types. It then reuses the exact VMXON/VMCS/CR3/VPID
  addresses over 64 VMXOFF/VMXON cycles, replacing the leaf without an L1
  invalidation between periods. Direct must observe the new page 64/64 times.
  The reference may retain or invalidate at this boundary; its observation is
  recorded but is not a Direct guarantee. Both observed 64/64 on this machine.
  An unsuccessful VMXON restart does not execute cleanup VMX instructions while
  outside VMX operation.
* `l1_memory::prepare_pages` factors the existing copied-root/absent-slot helper
  for both operand-fault and L2 translation probes; existing firmware mappings
  are untouched. HAL adds the checked `VIRTUAL_PROCESSOR_ID` encoding. The
  existing runner and xtask require the complete new markers, rejecting absent,
  malformed, duplicated or insufficient Direct lease evidence.

This is **not physical SMP support**: most carrier/VMXON/nested/cache/diagnostic
globals still require the later pCPU conversion. GS-based ownership applies
only to the current pinned CPU; no VMCS migration or AP launch is claimed.

Validation so far (cargo via `nix develop --accept-flake-config --command`):

* `cargo xtest -p x86_uefi_loader -p x86_guest_uefi_test -p xtask` and
  `cargo xtest -p nested_vmx -p x86_64_hal`: **169 PASS, 0 FAIL**, comprising
  nested 26, HAL 53, loader 47, guest 10 and xtask 33;
  `/tmp/x86-vpid-host-initial.log`, `/tmp/x86-vpid-host-policy-hal.log`.
  The standalone policy iteration also passed 26 tests.
* Debug and release `cargo xbuild x86`: **PASS**;
  `/tmp/x86-vpid-build-{debug,release}.log`.
* `cargo xrun x86 --release`: **9 PASS, 0 FAIL**, seven QEMU/KVM and two TCG
  profiles; `/tmp/x86-vpid-smoke.log`.
* The 64-cycle nested iteration reported **9 PASS, 5 FAIL** (the same five
  reference failures); `/tmp/x86-vpid-nested-64.log`. A concurrent debug build
  could restage its shared EFI artifacts, so this iteration is **not used as
  final release provenance**. The final 4096-cycle matrix is run without any
  concurrent artifact-producing build or QEMU runner.
* An initial manual MSR invocation produced all VPID/MSR PASS markers but used
  ordinary return-marker/shutdown defaults and ended as a harness FAIL (124,
  missing `vmx guest PASS`); `/tmp/x86-vpid-direct-msr.log`. The existing xtask
  profile supplies the correct poweroff/marker gates; no timeout was increased
  and the failed manual invocation is not counted as a passing test.

No Windows/Hyper-V, S3 or physical hardware validation is claimed here.

Final serialized release result:

* `LINUX_KVM_CYCLES=4096 LINUX_KVM_TIMEOUT_SECONDS=600 nix develop
  --accept-flake-config --command cargo xrun x86 --nested --release`:
  **9 PASS, 5 FAIL**, process exit 1; `/tmp/x86-vpid-nested-4096.log`.
  All six Direct native profiles and all three Linux profiles PASS. The five
  reference-only failures remain the two cross-page partial-store cases, the
  late-entry PAT-field case, and the two missing physical VMX-abort indicators.
  No Direct failure was replaced by reference success.
* 4096-cycle lifecycle PASS timestamps: reference **38.476460 s**, Direct
  **328.492424 s**, Direct host-XSTATE-clobber **328.499646 s**. These are the
  existing end markers, not an isolated VM-exit benchmark or an optimization
  claim. The substantial-cycle profile retains its existing 600-second bound.
* The final common MSR marker is clarified to `final_vmxoff=1`; it counts only
  final cleanup, separate from the 64 lease-boundary VMXOFF/VMXON pairs. The
  program's assertions and monitor runtime are unchanged by this label edit.
  Release rebuild, the Direct MSR fixture with the xtask-equivalent 30-second
  poweroff/marker settings, and `--check-msr-contract-log direct-vmx` all PASS;
  `/tmp/x86-vpid-final-build.log`, `/tmp/x86-vpid-final-direct-msr.log`.
  `cargo xtest -p xtask` also passes all 33 tests after the label change;
  `/tmp/x86-vpid-final-gates.log`.
* `cargo fmt`, `cargo fmt --check`, `git diff --check`: **PASS**. Only x86
  runtime/HAL, nested policy, x86 test/runner, xtask and this evidence changed.
  The user's unrelated `AGENTS.md` edit remains unstaged.

## Proven 2 MiB nested EPT capability

Confirmed: `TRUSTED_EPT_VPID_CAPABILITIES` hid both large-page sizes. Pinned
Linux 7.1.5 `arch/x86/kvm/vmx/capabilities.h::ept_caps_to_lpage_level` therefore
selected 4 KiB. Earlier Direct logs had zero 2 MiB pages where the unmodified
huge-page tests required them. This was a capability/coverage limitation, not
evidence that KVM ignored the advertised mask.

Before changing that mask, `msr_contract::ept_large_pages` was added and executed
through project Direct VMCS. Its marker explicitly recorded **advertised=0**:

* a hardware 2 MiB leaf backed by owned, aligned WB pages;
* write permission removal, EPT violation, and recovery at the same guest RIP;
* 2 MiB -> 512 x 4 KiB splitting, permission changes and recovery;
* replacement with a second owned 2 MiB backing region;
* a reserved address bit producing a reflected EPT misconfiguration;
* an absent leaf producing the correct EPT violation/GPA/GLA/qualification;
* single-context and all-context INVEPT, ten successful invalidations in total.

The controlled probe uses the existing q35-only 1 GiB identity helper for its
code/stack and a separate 4 GiB guest-physical alias for the payload. It is **not
the production platform map**. Physical 2 MiB support was already mandatory for
Direct's smoke carrier. Ordinary guest software must obey advertised capabilities;
the initial hidden-bit experiment was explicitly a native test proof. The test
reuses `l1_memory::prepare_pages`, retains all allocation/descriptor lifetimes,
and powers off the disposable VM. `enter` resume mode 2 restores the test's live
RAX/RDX operands after wrapper MSR writes without changing the faulting RIP;
`ept_store_guest` then proves the denied store actually completes after repair.

Only after that proof passed was the existing HAL `EPT_CAP_PDE_2MB` added to the
allowed capability set. `restrict_ept_vpid_capability` still intersects hardware
support; missing hardware support never invents the bit. **1 GiB remains hidden**.
The new unit test checks both boundaries. No Direct VMCS software EPT composition,
new control bit, dependency, or fixed production map was added. Native runner/
xtask gates now require advertised=1 and every proof count, rejecting missing,
duplicated, hidden-bit or incomplete evidence.

Validation (existing Nix/cargo/xtask runners, all disposable QEMU/KVM):

* Before advertisement, the correctly built native Direct proof and complete
  MSR fixture **PASS**; `/tmp/x86-ept2m-proof-direct-2.log`, build
  `/tmp/x86-ept2m-proof-build-3.log`. Two earlier compile iterations failed on
  the descriptor field name and an inferred integer type, then were corrected.
  `/tmp/x86-ept2m-proof-direct.log` ran an older artifact after the first build
  failure and has no EPT proof marker; it is **excluded** as proof evidence.
* Five required host packages: **170 PASS, 0 FAIL** (nested 27, HAL 53, loader
  47, guest 10, xtask 33); `/tmp/x86-ept2m-policy-unit.log`,
  `/tmp/x86-ept2m-host-rest.log`. The guest-only iteration also passed 10 tests.
* `LINUX_KVM_CYCLES=64 nix develop --accept-flake-config --command cargo xrun
  x86 --nested --release`: **9 PASS, 5 FAIL**, exit 1;
  `/tmp/x86-ept2m-nested-64.log`. All Direct native and Linux profiles PASS;
  the same five reference-only failures remain. Both native backends emit the
  new advertised=1 EPT proof marker. The latest 4096-cycle run is the preceding
  VPID increment; no new 4096-cycle EPT measurement is claimed here.
* Unmodified pinned `dirty_log_page_splitting_test` and `nx_huge_pages_test`:
  **Direct PASS, exit 0**, fixing both previously recorded failures.
* Unmodified `memslot_modification_stress_test` and `kvm_page_table_test`:
  **Direct PASS, exit 0**.
* Unmodified `memslot_perf_test`: **Direct FAIL, guest exit 142**. Five subtests
  complete; the RW subtest's own `host_perform_sync::alarm(10)` expires. This is
  not the outer runner or guest BusyBox timeout. The same unchanged `-s 4096`
  test **also FAILS with 2 MiB advertisement temporarily removed**. That
  temporary change was restored; reference **PASS, exit 0**. The 2026-09-08
  Direct run also failed in RW, but used different slot defaults and is not
  substituted for the new controlled comparison. No timeout, iteration count,
  guest timer or assertion was weakened. This remains a Direct performance/
  event-correctness failure for the next instrumentation increment.

The five Direct selftests used `LINUX_SELFTEST_BACKEND=direct-vmx`,
`LINUX_SELFTEST_NAME=<name>`, and `LINUX_SELFTEST_ELF=` the corresponding
unmodified ELF under `/tmp/thin-hv-kvm-selftests-7.1.5.MUloxc/out/` (`x86/` for
the two huge-page tests), then `nix develop --accept-flake-config --command
./scripts/x86_64/run-linux-selftest.sh`. Logs are
`/tmp/x86-ept2m-direct-<name>.log`; the additional control logs are
`/tmp/x86-ept2m-direct-memslot_perf_test-capoff-control.log` and
`/tmp/x86-ept2m-reference-memslot_perf_test.log`.

No Windows/Hyper-V, S3 or physical machine was tested for this increment. The
fixed active platform map, AP ownership, root NMI and S3 gaps remain; this is not
physical readiness. Outer-KVM evidence remains reference only.

Final checks after restoring the intended mask: `cargo xtest -p nested_vmx`
**27 PASS**, `/tmp/x86-ept2m-restored-policy-unit.log`; `cargo xbuild x86`
**PASS**, `/tmp/x86-ept2m-build-debug.log`; `cargo xrun x86 --release`
**9 PASS, 0 FAIL** (seven QEMU/KVM, two QEMU/TCG),
`/tmp/x86-ept2m-smoke.log`. The unmodified Direct `xcr0_cpuid_test` also
**PASS**, `/tmp/x86-ept2m-direct-xcr0_cpuid_test.log`. Formatting and diff checks
PASS; no AArch64 production code or user `AGENTS.md` change was included.

## VMCS operation telemetry before performance changes

`vmx_smoke::{vmcs_read,vmcs_write,vmcs_load,vmcs_write_reflected}` now count
actual hardware attempts, including failures. `vmx::VmcsAccessCounts` retains
the completed operation prefix inside guarded entry/error helpers, including
their raw VMPTRLD assembly. Software mirror hits do not count as VMREADs.
Counters saturate, never print from the hot path, and do not overwrite the
last exit phase/reason. Reflection writes are a subset of total VMWRITEs.
No required VMCS operation has been removed in this instrumentation increment.

The existing bounded diagnostic record is version 2, 176 bytes. The Python
decoder and Windows HMP capture agree on this exact length; old versions,
misaligned/out-of-image addresses and torn/exhausted records remain rejected.
Storage is still explicitly BSP-only, not physical SMP support. The Windows
reader change is host-tested only, not a Windows boot/Hyper-V result.

Validation:

* `cargo xtest -p x86_64_hal -p x86_uefi_loader -p xtask` and
  `cargo xtest -p nested_vmx -p x86_guest_uefi_test`, through the Nix environment:
  **173 PASS, 0 FAIL** (HAL 54, loader 49, xtask 33, nested 27, guest 10).
  Logs: `/tmp/x86-vmcs-telemetry-host-complete.log` and
  `/tmp/x86-vmcs-telemetry-host-rest.log`.
* `cargo xbuild x86 --release`: **PASS**;
  `/tmp/x86-vmcs-telemetry-build-release.log`.
* `LINUX_KVM_CYCLES=64 nix develop --accept-flake-config --command cargo xrun
  x86 --nested --release`: **9 PASS, 5 FAIL**; every Direct native/Linux profile
  passes, with the same five previously explained reference failures.
  `/tmp/x86-vmcs-telemetry-nested-64.log`.
* Unmodified Direct `memslot_perf_test`, same pinned ELF and runner parameters
  as the preceding increment: **FAIL, guest exit 142**, RW alarm after five
  completed subtests; `/tmp/x86-vmcs-telemetry-memslot-baseline.log`.
* Separate diagnostic-only repetition: five paused snapshots of **only the
  published 176-byte counter record**, using the existing HMP FIFO and decoder.
  `/tmp/x86-vmcs-counter-samples.log` and
  `/tmp/x86-vmcs-telemetry-memslot-sampled.log`. All five records validate.
  Across the sampling interval, 44,348 reflected exits correspond to about
  11.00 VMPTRLD, 83.57 VMREAD, 91.08 VMWRITE attempts per reflected exit;
  54.00 are reflected-state writes. These include intervening L1 exits and
  partial boundary exits, not an isolated per-exit instruction benchmark.
  Pauses make this run **ineligible for timing qualification**; its eventual
  RW alarm is not substituted for the unpaused failure above. No timeout or
  upstream test was changed, and no guest/firmware data was dumped.

This identifies VMCS switching and the 54 unconditional reflection writes as
measurable optimization candidates, not a demonstrated watchdog root cause.
No Windows, S3 or physical hardware test is claimed; outer KVM remains reference
evidence only. The active 8 GiB platform-map and physical ownership gaps remain.

Final instrumentation checks: Nix `cargo xbuild x86` **PASS**
(`/tmp/x86-vmcs-telemetry-build-debug.log`), `cargo xrun x86 --release`
**9 PASS, 0 FAIL**, seven KVM/two TCG (`/tmp/x86-vmcs-telemetry-smoke.log`).
Nix `cargo fmt --check` and `git diff --check` **PASS**. An accidental non-Nix
`cargo fmt --check` first tried updating the floating rustup nightly and failed
installing rust-src due to an existing file conflict; that is a local toolchain
failure, not formatting evidence. No toolchain repair was attempted; validation
continues with the repository's Nix-pinned environment.

## Exit snapshots and live reflection dirtiness

After the telemetry baseline, `nested_vmx::exit_snapshot::ExitSnapshot` retains
ten hardware read-only exit-information fields in `CpuRuntimeState`, selected
through the owning CPU's private GS. It snapshots the actual direct VMCS after
an exit; it does not compose a VMCS12/VMCS02, guest state or L1 host state.
VMREAD hits avoid switching to the direct VMCS and back. Exact encodings and
the GPA high-half alias are handled; unsupported encodings still use hardware.
`VM_INSTRUCTION_ERROR` is deliberately excluded because later VMfail changes it.
Snapshot validity requires the existing hidden VMX_MISC[29] policy, checked by
a unit assertion. Entry attempts, VMCLEAR, VMPTRLD and VMXON/OFF invalidate the
CPU's snapshot conservatively before emulation, including failed operations.
No incomplete capture is published and no lock/borrow spans guest execution.

`vmcs_write_reflected` additionally compares each target with the **current
carrier VMCS field**, writing only differences. It never trusts a stale copy
across L1 execution: L1 can change CRs, MSRs, descriptors and selectors without
exiting. All original 54 reflection targets and read/write error paths remain.
This trades 54 VMREADs for fewer VMWRITEs, not an assumption that host fields
never change. Raw operation telemetry measures both sides of that trade.

Native `msr_contract::ept_large_pages` now also proves 24 repeated GPA high-half
reads, eight warm reason reads, unsupported-field and bad-host-entry VMfail with
fresh instruction-error values, read-only write rejection, and switching between
two actual VMCSes with different CPUID/VMCALL exits. The second VMCS page is
separate from EPT tables/payloads and is cleared before fixture completion.
The strict shell/xtask gate requires complete, ordered snapshot evidence.
Reference CPUs advertising writable exit fields take an explicitly tested
success/write/restore branch; **Direct must take readonly_reject=1**. An initial
fixture mistakenly demanded rejection from the writable reference CPU; that
test assumption was corrected without relaxing Direct's requirement. Reference
then reaches its already-recorded late-entry PAT-save failure again.

Validation so far (QEMU/KVM, never physical hardware):

* Snapshot-only host iteration: nested **29 PASS**, loader **49 PASS**;
  `/tmp/x86-exit-snapshot-unit.log`. Full host checks after live comparison:
  **175 PASS, 0 FAIL** (nested 29, HAL 54, loader 49, guest 10, xtask 33), using
  `cargo xtest -p nested_vmx -p x86_64_hal -p x86_uefi_loader
  -p x86_guest_uefi_test -p xtask` through Nix;
  `/tmp/x86-reflection-dirty-host.log`.
* Snapshot-only release build **PASS**, `/tmp/x86-exit-snapshot-build.log`.
  Its first nested-64 run has all Direct cases **PASS**, but the initial
  reference fixture assumption above fails before the prior reference failure.
  `/tmp/x86-exit-snapshot-nested-64.log` is intermediate evidence only.
* With corrected fixture and live reflection comparison:
  `LINUX_KVM_CYCLES=64 nix develop --accept-flake-config --command cargo xrun
  x86 --nested --release`: **9 PASS, 5 FAIL**; all Direct native/Linux and
  reference Linux cases pass, with the same five reference-only failures.
  `/tmp/x86-reflection-dirty-nested-64.log`.
* Unpaused, unmodified Direct `memslot_perf_test`: **FAIL, guest exit 142** for
  both snapshot-only and live-comparison variants. Map averages in these single
  runs are 1.7594 and 1.6680 seconds (telemetry baseline 1.9288); these are not
  controlled production-speed claims. RW still expires its own alarm. Logs:
  `/tmp/x86-exit-snapshot-memslot-unpaused.log` and
  `/tmp/x86-reflection-dirty-memslot-unpaused.log`.
* Separate paused diagnostic repetitions each yield five valid 176-byte records.
  Snapshot-only: 12,409 reflected-exit delta, 5.425 VMPTRLD, 135.264 VMREAD,
  110.181 VMWRITE, 53.996 reflected writes per reflected exit. With live comparison:
  16,837 reflected-exit delta, 4.632 VMPTRLD, 168.480 VMREAD, 50.124 VMWRITE,
  **2.447 reflected writes** per reflected exit. Logs:
  `/tmp/x86-exit-snapshot-counter-samples.log` and
  `/tmp/x86-reflection-dirty-counter-samples.log`.
  Workload phases/intervening L1 exits differ; these ratios confirm reduced
  switching/writes, not latency or throughput equivalence. Both diagnostic
  repetitions still reach the RW alarm and are excluded from timing qualification.

The original timing-sensitive regressions are not waived, and no timeouts or
capabilities were weakened. Windows Hyper-V, S3 and physical hardware remain
unverified for this increment; outer-KVM results remain reference evidence only.

### VM-exit benchmarks and controlled Direct A/B

All twelve existing `vmexit_*` manifest cases pass on both backends:
**24 PASS, 0 FAIL**, through `run-linux-kunit-test.sh`, with
`LINUX_KUNIT_BACKEND=<backend>`, `LINUX_KUNIT_CASE=<case>`,
`LINUX_L2_KUNIT_DIR=/tmp/thin-hv-kvm-unit-tests-20260908/x86`, and the same pinned
L2 test QEMU used by the previous evidence:
`LINUX_L2_QEMU=/nix/store/dz3ivvcn2916ac16l95vzgshikxrbicr-qemu-host-cpu-only-for-vm-tests-10.1.5/bin/qemu-system-x86_64`.
Each invokes `nix develop --accept-flake-config --command bash
scripts/x86_64/run-linux-kunit-test.sh`. No manifest case, guest argument, CPU
setting, adaptive benchmark loop or time limit was changed. Logs:
`/tmp/x86-reflection-vmexit-final-<backend>-<case>.log`; batch result:
`/tmp/x86-reflection-vmexit-final-batch.log`.

| Benchmark | Reference ticks/iteration | Direct ticks/iteration |
| --- | ---: | ---: |
| CPUID | 12,949 | 361,080 |
| VMCALL | 39,062 | 1,125,321 |
| CR8 read | 8 | 9 |
| CR8 write | 17 | 12 |
| PM timer IN | 19,271 | 417,728 |
| IPI | 63,160 | 2,775,465 |
| IPI + halt | 62,358 | 2,819,064 |
| PLE round robin | 5,462,094 | 5,465,527 |
| TSC deadline | 12,970 | 850,246 |
| Immediate TSC deadline | 25,925 | 1,207,403 |
| CR0.WP toggle | 54,332 | 2,299,058 |
| CR4.PGE toggle | 1,534 | 2,203 |

These are QEMU/KVM observations, not physical-L0 measurements or CPU-isolated
results. Multi-vCPU L2 cases still share one L1 CPU and do not prove physical SMP.
An initial launch omitted the recorded L2 QEMU override: four reference cases
failed before their guests ran with `Failed loading SDL3 library`, exit 134.
That batch was stopped (exit 143; the fifth case did not finish setup). Its
non-`final` logs remain infrastructure failures, not architectural benchmark
results. Explicitly restoring the previously recorded QEMU path fixed setup;
no desktop dependency or test bypass was added to the repository.

To measure the optimization itself, the immediately preceding telemetry commit
`c5d8ece` was built in the separate **detached** worktree
`/tmp/x86-telemetry-baseline.YhT7nE`. The main branch never changed. Baseline
release build/ISA checks pass (`/tmp/x86-reflection-ab-baseline-build.log`).
For CPUID, VMCALL and PM timer IN, one immutable UKI per case was used by the
existing `run-uefi-smoke.sh` and strict `run-linux-kunit-test.sh --check-log`
gate, alternating baseline/current loader+monitor pairs three times. All
QEMU settings match the corresponding manifest runner. **18 PASS, 0 FAIL**;
artifact hashes and statuses are in `/tmp/x86-reflection-ab.log`, individual
logs `/tmp/x86-reflection-ab-<case>-<baseline|optimized>-<1|2|3>.log`.

| Case | Baseline ticks (three runs) | Optimized ticks (three runs) | Median reduction |
| --- | --- | --- | ---: |
| CPUID | 378611, 378131, 381084 | 345276, 344174, 343867 | 9.10% |
| VMCALL | 1186845, 1180898, 1188367 | 1095071, 1092606, 1098096 | 7.73% |
| PM timer IN | 458047, 449060, 463299 | 421242, 412741, 413742 | 9.67% |

The controlled comparison supports retaining these two bounded optimizations,
but does not resolve RW's alarm or establish physical/Windows performance.

Final pre-commit checks: Nix `cargo xbuild x86` **PASS**
(`/tmp/x86-reflection-dirty-build-debug.log`), `cargo xrun x86 --release`
**9 PASS, 0 FAIL** (seven KVM/two TCG;
`/tmp/x86-reflection-dirty-smoke.log`), `cargo fmt --check` and
`git diff --check` **PASS**. The latest full 4096-cycle and isolated invalid-
guest-state results still precede this performance increment; they remain to
be rerun and are not claimed as current qualification here.

### Post-performance long regressions (`f08b671`)

The six unmodified pinned selftests were rerun, serialized, with
`LINUX_SELFTEST_BACKEND=direct-vmx LINUX_SELFTEST_NAME=<name>
LINUX_SELFTEST_ELF=<pinned ELF> nix develop --accept-flake-config --command
./scripts/x86_64/run-linux-selftest.sh`. The ELF root remains
`/tmp/thin-hv-kvm-selftests-7.1.5.MUloxc/out/` (architecture-specific executables
under `x86/`). **5 PASS, 1 FAIL**: `xcr0_cpuid_test`,
`dirty_log_page_splitting_test`, `nx_huge_pages_test`,
`memslot_modification_stress_test`, and `kvm_page_table_test` pass.
`vmx_exception_with_invalid_guest_state` still reaches its existing 600-second
guest kill, **process exit 137**. No test source, alarm or runner timeout changed.
Logs: `/tmp/x86-reflection-final-direct-<name>.log`; batch summary:
`/tmp/x86-reflection-selftest-batch.log`.

`LINUX_KVM_CYCLES=4096 LINUX_KVM_TIMEOUT_SECONDS=600 nix develop
--accept-flake-config --command cargo xrun x86 --nested --release` completed with
**8 PASS, 6 FAIL**, exit 1 (`/tmp/x86-reflection-final-nested-4096.log`).
All six Direct native profiles pass. Reference Linux completes 4096 cycles
at guest time 39.081 seconds; ordinary Direct Linux completes all 4096 at
324.206 seconds. These are not CPU-isolated timing comparisons.
The same five reference-only architectural differences remain failures.
The sixth failure is **runner infrastructure, not a completed guest result**:
editing the live Bash runner's preflight function while its last Direct XSTATE
clobber guest ran invalidated the interpreter's later file offsets, producing
`suspended_now: unbound variable`. The last fixture was interrupted before its
PASS marker. This was an agent test-execution error; it must be replayed with
the runner frozen, and cannot be counted as PASS or diagnosed as guest corruption.
Source, runners and generated boot artifacts must stay fixed throughout a suite,
even when an edit changes only a function not used by the active guest.

## Platform map increment: read-only GCD MMIO discovery

Confirmed the active Direct carrier still calls `build_identity_8g` and
`build_host_identity_8g`; this increment **does not yet replace either**.
It first validates the missing resource input through non-VMX preflight.
The existing disposable `PlatformMap` / `build_platform_identity` audit now
consumes MMIO obtained from PI `GetMemorySpaceMap`, alongside its captured UEFI
memory map and MTRRs. The
[PI DXE table ABI](https://uefi.org/specs/PI/1.9/V2_UEFI_System_Table.html) and
[GCD service/descriptor semantics](https://uefi.org/specs/PI/1.9/V2_Services_DXE_Services.html)
define this read-only discovery and caller-owned temporary pool buffer.

Changed files/symbols:

* `x86_uefi_loader/src/platform_resources.rs`: `collect`, `collect_mmio`,
  `MmioMap`, checked DXE prefix and numeric GCD descriptor ABI. Validates
  table/count/pointer bounds, unknown types, arithmetic/physical-width limits,
  overlapping descriptors, UC capability/current-cache conflicts, and page
  rounding across RAM ownership. At most 4096 descriptors and 128 merged MMIO
  intervals; no new dependency or unbounded monitor allocation. Firmware-owned
  buffer cleanup runs on every successful-call inspection result. Missing DXE
  discovery is explicitly unsupported, never an empty-map success/fallback.
* `platform_snapshot.rs`: `MemoryMap::system_table` and
  `configuration_tables` reuse the prior preflight validation for both ACPI and
  DXE consumers. `physical_preflight.rs::inventory` gathers the MMIO input;
  `platform_ept_audit.rs::AuditStorage::inspect` materializes it without
  publishing an EPTP. `main.rs` wires the module only into physical-preflight.
* `scripts/x86_64/run-uefi-smoke.sh::check_preflight_ept_log` and
  `xtask/src/main.rs::preflight_ept_gate_distinguishes_construction_from_capability_skip`
  require complete ordered GCD evidence, bounded matching counts, sorted aligned
  nonoverlapping ranges, UC and the honest incomplete/readiness markers before
  accepting either EPT construction or TCG's no-VMX capability skip.

Five added host tests cover ABI/header/count bounds, unordered high MMIO merging,
physical-width/overflow boundaries, malformed/overlapping/cache-conflicting
descriptors, subpage MMIO versus adjacent RAM, and output-capacity failure.
Existing package filtering already includes the preflight feature, so
`xtest.txt` needs no change. No ACPI/SMBIOS/MSDM payload or identity is changed
or logged, no Runtime Services hook is installed, and no project VMX executes.

Validation:

* Required five `cargo xtest -p` packages under Nix: **180 PASS, 0 FAIL**
  (nested 29, HAL 54, loader 54, guest 10, xtask 33). Nix `cargo xbuild x86`
  and `cargo fmt --check` **PASS**. `/tmp/x86-gcd-final-checks.log`.
  The earlier affected-package-only run passes 87 tests
  (`/tmp/x86-gcd-host-first.log`).
* Nix `cargo xbuild x86 --release` **PASS**. Then
  `X86_UEFI_BACKEND=physical-preflight X86_UEFI_ACCEL=kvm
  X86_UEFI_CPU=host,+vmx,-hypervisor X86_UEFI_MEMORY=4G
  X86_UEFI_PCI_PROFILE=firmware-default X86_UEFI_TIMEOUT_SECONDS=30
  scripts/x86_64/run-uefi-smoke.sh bin/x86_64/x86-uefi-preflight.efi`
  under Nix **PASS**. `/tmp/x86-gcd-preflight-high-first.log`.
  This is the default q35 PCI layout, not `q35-smoke-1g`: 47 GCD descriptors,
  eight merged MMIO intervals, including **[56 TiB, 64 TiB)**; the disposable
  EPT uses 38 tables and 17,112 leaves. Its 288 private audit pages are excluded.
* Nix `cargo xrun x86 --release`: **9 PASS, 0 FAIL** (seven QEMU/KVM, two TCG),
  `/tmp/x86-gcd-smoke.log`. Default-size KVM preflight constructs 35 EPT tables /
  15,192 leaves. TCG collects GCD resources but correctly skips EPT for absent
  VMX. Existing Direct/reference and physical-chainload policy fixtures pass.
* `git diff --check` **PASS**; only x86/test/documentation paths changed, apart
  from the untouched pre-existing user `AGENTS.md` edit. No generated artifacts
  are staged. No physical machine or Windows/Hyper-V/WSL2 was tested.

This is deliberately **not complete MMIO discovery**: the same QEMU firmware
reports `[0xe0000000, 0xf0000000)` as reserved UEFI memory, not GCD MMIO.
PCI ECAM must therefore be discovered/checked through MCFG/PCI firmware data;
GCD alone cannot establish complete device coverage. Markers retain
`mmio_complete=0 direct_vmx_ready=0`. A successful disposable preflight is not a
successful Direct Linux high-BAR boot. Active EPT/HOST_CR3 integration, bootstrap
separation, pCPU/AP ownership, root NMI, S3 and Windows remain pending.
Outer-KVM remains reference evidence only.

### Frozen-runner XSTATE replay

After commit `c0540c1`, the interrupted profile was replayed with source, runner
and boot artifacts fixed for the whole run:
`LINUX_KVM_BACKEND=direct-vmx LINUX_KVM_HOST_XSTATE_TEST=1
LINUX_KVM_CYCLES=4096 LINUX_KVM_TIMEOUT_SECONDS=600 nix develop
--accept-flake-config --command bash scripts/x86_64/run-linux-kvm-test.sh`.
**PASS**, process exit 0; all 4096 ordered cycle records and the explicit XSTATE
clobber-arm marker are present. Guest completion time is 323.355 seconds.
`/tmp/x86-gcd-direct-clobber-4096-replay.log` contains the full transcript.
This supplies the missing Direct profile result, but does not rewrite the
earlier full-suite infrastructure failure into a clean suite run. The unmodified
invalid-guest-state and memslot RW timing failures remain unresolved.

## Platform map increment: ACPI ECAM and APIC resources

The missing QEMU ECAM interval above is now obtained from its actual MCFG,
not inferred from q35. New `x86_uefi_loader/src/platform_acpi.rs` reuses the
previous RSDP/root/header scanner and its MSDM regression, moved from
`physical_preflight.rs`. `Tables` retains only an MSDM presence boolean and the
allowlisted MCFG/MADT addresses. `resource_payload` refuses any other signature
before a full-payload read, including MSDM. Firmware tables remain unchanged.

`parse_mcfg` / `mcfg_window` validate checksums, table/revision/reserved-field
layout, bounded allocation count, MiB-aligned bus windows, bus ordering,
physical width, overflow and overlapping segment/bus or physical ranges.
The CPU-relative base remains relative to **bus zero**: a nonzero starting bus
adds its own MiB offset, matching the
[Linux ECAM resource calculation](https://raw.githubusercontent.com/torvalds/linux/master/drivers/acpi/pci_mcfg.c).
`parse_madt` validates subtable extents, IOAPIC layouts/duplicate IDs, page
alignment/width, and a single 64-bit LAPIC override. The override replaces the
header's LAPIC address, per the
[ACPI MADT specification](https://uefi.org/specs/ACPI/6.6/05_ACPI_Software_Programming_Model.html#local-apic-address-override-structure).
Other MADT entries remain untouched; this limited inventory is not a claim
that every possible platform-specific resource has been discovered.

`platform_resources.rs::collect_with_acpi` checks the added intervals against
the complete live GCD map before freeing its descriptor buffer. ACPI cannot
convert GCD system/persistent/reliable/unaccepted RAM to MMIO. Reserved/absent
GCD ranges may gain an explicit ACPI MMIO description; enclosing GCD MMIO
apertures are unioned without duplication. `MmioMap::insert` also checks the
current captured physical width when a range originated in a wider context.
`physical_preflight.rs::inventory` passes the combined map into
`platform_ept_audit.rs::AuditStorage::inspect`; `main.rs` wires the moved parser
only into preflight. The shell gate and its `xtask` host regression now require
ordered MCFG/MADT and combined-source evidence, with honest incomplete markers.

Six added host tests cover MCFG start-bus/high-address/last-page behavior,
malformed/overlapping MCFG tables, MADT override/address discovery and negative
subtables, 32/64-bit root traversal with header-only MSDM and duplicate-table
rejection, and ACPI-versus-GCD RAM/width conflicts. No external crate, runtime
heap allocation, firmware identity modification or new unsafe block was added.

Validation:

* Nix `cargo xtest -p x86_uefi_loader` / `-p xtask`: **93 PASS, 0 FAIL**,
  `/tmp/x86-acpi-mmio-host-first.log`. The header-only fixture refinement is
  retested in the subsequent commands, with no MSDM payload constructed.
* Nix `cargo xtest -p x86_uefi_loader`, `cargo xbuild x86 --release`, then the
  same explicit 4G/default-q35 preflight command recorded for GCD above:
  **PASS**, `/tmp/x86-acpi-mmio-high-first.log`. MCFG/MADT produce three ACPI
  MMIO resources. The checked union has eight intervals and now includes
  `[0xe0000000, 0xf0000000)` in the low aperture; high `[56 TiB, 64 TiB)` remains.
  The EPT has **38 tables / 17,240 leaves**, exactly 128 additional 2 MiB leaves
  for ECAM compared with the GCD-only capture.
* All five required Nix `cargo xtest -p` packages: **186 PASS, 0 FAIL**
  (nested 29, HAL 54, loader 60, guest 10, xtask 33). Nix `cargo xbuild x86`,
  `cargo fmt --check`, and `cargo xrun x86 --release` also pass; the latter is
  **9 PASS, 0 FAIL**, seven QEMU/KVM and two TCG.
  `/tmp/x86-acpi-mmio-final-checks.log`.
* `git diff --check` passes. No AArch64 production path changed and the user
  `AGENTS.md` edit remains unstaged. No physical machine or Windows was tested.

The active Direct carrier and HOST_CR3 still use their fixed maps. PCI root
aperture/BAR protocol cross-checks, active map integration and the later
architectural milestones remain pending. Markers still state
`mmio_complete=0 direct_vmx_ready=0`; these non-VMX preflight results are not
Direct high-BAR Linux boot or physical-readiness evidence. Outer KVM is reference
evidence only.

## PCI firmware aperture/BAR cross-checks (stage 10 increment)

Added `platform_pci::{collect, parse_resources, Roots}` to the non-VMX preflight.
Read-only RootBridgeIo `Configuration()` supplies root memory and bus windows;
PCI I/O `GetLocation()`, configuration-header reads and `GetBarAttributes()`
cross-check assigned BARs against their owning segment/bus/root window. No
driver connection, BAR sizing writes, command-bit writes or attribute changes
occur. Root buffers remain firmware-owned; temporary handle/BAR pools are freed
on success and validation errors. Existing `MmioMap` and GCD-versus-RAM checks
are reused for the final ACPI/PCI/GCD union.

The bounded parser accepts UEFI QWORD descriptors plus a checked End Tag;
rejects missing/truncated terminators, malformed types/flags/lengths, checksum
errors, arithmetic/physical-width overflow, overlapping root windows/bus
ownership, BAR/root/config mismatches and capacity exhaustion. Host minima stay
host addresses; signed host-to-PCI translation offsets are used only for the
configuration-register comparison, with interval-wrap checks. EDK2's precise
power-of-two BAR alignment-mask form is accepted alongside the specification's
ending-address form, not as an unchecked arbitrary maximum. Multi-descriptor
BARs must be contiguous and consistent; duplicates/gaps are rejected. Limits
are 32 roots, 4096 handles, 128 descriptors/windows; unsupported CardBus header
layouts or unavailable root resources fail explicitly instead of falling back.

Primary references inspected: [UEFI 2.11 PCI protocols](https://uefi.org/specs/UEFI/2.11/14_Protocols_PCI_Bus_Support.html),
[EDK2 RootBridgeIo ABI](https://github.com/tianocore/edk2/blob/master/MdePkg/Include/Protocol/PciRootBridgeIo.h),
and [EDK2 PciIoGetBarAttributes](https://github.com/tianocore/edk2/blob/master/MdeModulePkg/Bus/Pci/PciBusDxe/PciIo.c).

Validation (all commands use `nix develop --accept-flake-config --command`):

* Initial `cargo fmt && cargo xtest -p x86_uefi_loader`: **66 PASS, 0 FAIL**,
  including six new PCI tests; `/tmp/x86-pci-mmio-host-first.log`.
* `cargo fmt && cargo xtest -p xtask && cargo xbuild x86 --release`, followed by
  the explicit 4G/default-q35 preflight command below: **33 host PASS, 0 FAIL**,
  build and QEMU preflight **PASS**; `/tmp/x86-pci-mmio-high-first.log`.
* After multi-descriptor BAR validation, all five `cargo xtest -p` packages:
  **192 PASS, 0 FAIL** (nested 29, HAL 54, loader 66, guest 10, xtask 33).
  `cargo xbuild x86`, `cargo fmt --check`, `cargo xrun x86 --release` also pass.
  Standard suite: **9 PASS, 0 FAIL**, seven KVM (including the expected-stop
  private-host exception fixture), two TCG; `/tmp/x86-pci-mmio-final-checks.log`.
* Final built artifact replay: **PASS**, `/tmp/x86-pci-mmio-high-final.log`:

  ```sh
  nix develop --accept-flake-config --command env \
      X86_UEFI_BACKEND=physical-preflight X86_UEFI_ACCEL=kvm \
      X86_UEFI_CPU=host,+vmx,-hypervisor X86_UEFI_MEMORY=4G \
      X86_UEFI_PCI_PROFILE=firmware-default X86_UEFI_TIMEOUT_SECONDS=30 \
      scripts/x86_64/run-uefi-smoke.sh bin/x86_64/x86-uefi-preflight.efi
  ```

  OVMF reports one root, five devices, two memory windows and three memory BARs.
  Combined MMIO remains eight intervals, including `[56 TiB,64 TiB)` and ECAM;
  EPT construction uses 38 tables / 17,240 leaves. Runner gates require the PCI
  summary before combined MMIO/EPT PASS and reject missing/malformed provenance.

No Direct runtime, AArch64 production path, Windows or physical machine changed
or was tested by this increment. The active Direct EPT/HOST_CR3 integration is
still pending; `mmio_complete=0 direct_vmx_ready=0` is intentional. These
non-VMX QEMU preflight results do not resolve the Direct high-PCI Linux failure.
Outer-KVM successes remain reference evidence only. The pre-existing user edit
to `AGENTS.md` remains unstaged.

## Active Direct carrier platform EPT (stage 10, HOST_CR3 still pending)

`vmx_smoke::run_direct_monitor` now calls `build_carrier_ept`, not
`ept::build_identity_8g`. The complete checked UEFI RAM map, CPU physical width,
effective MTRRs and shared GCD/ACPI/PCI resource inventory feed the existing HAL
platform planner/materializer. `platform_acpi::Tables::from_system_table` and
`platform_resources::platform_mmio` keep the preflight and carrier selection
logic identical. The retained monitor block has a bounded 256-page EPT arena;
all page offsets derive from that size. The arena is excluded from ordinary L1
EPT mappings and all of its pages must be runtime-owned, writable and genuinely
WB under the captured MTRRs. Old q35 WB address buckets no longer decide cache
validity. No EPTP is returned before complete construction and snapshot cleanup;
allocation/capacity/attribute failures have no fixed-map or outer-KVM fallback.
The HAL fixed smoke builders remain available for explicit test consumers.

This is an incremental carrier-EPT change, not completed physical support.
`HOST_CR3` and operand-access bounds still use the explicit 8 GiB host limit.
Guest bootstrap and variable overlay still share the retained runtime image;
other monitor pages are not yet excluded from the normal L1 EPT. The marker
states `host_map=fixed-8g bootstrap=shared-runtime physical_ready=0`. Production
host mapping, bootstrap/private-page separation, overlay removal, pCPU/AP/NMI/S3
and Windows qualification remain pending. No new nested EPT capability is
advertised: outer EPT leaf sizes use hardware capabilities independently of the
already restricted L1-visible capability mask.

The runner now requires an active Direct platform-EPT construction marker even
for the expected terminal exception/abort fixtures. A preflight-only or
outer-KVM success cannot satisfy that gate. PCI inventory records the highest
actual BAR end; `X86_UEFI_REQUIRE_HIGH_PCI=1` rejects a run without a BAR above
8 GiB or with the reduced q35 smoke aperture. `LINUX_KVM_MEMORY=2G|4G` selects a
bounded Linux regression size. `cargo xrun x86 --nested --release` now includes a
mandatory 4G, firmware-default, high-BAR Direct Linux lifecycle case with separate
evidence `bin/x86_64/nested-linux-direct-vmx-high-pci-4g.log`.

Validation, all through `nix develop --accept-flake-config --command`:

* `cargo fmt && cargo xtest -p x86_uefi_loader && cargo xbuild x86 --release`:
  **120 host PASS, 0 FAIL**, release build/ISA checks **PASS**;
  `/tmp/x86-platform-carrier-first-build.log`. Shared inventory tests now also
  execute under both Direct feature entries; counts include those variants.
* Minimal Direct UEFI smoke: **PASS**, `/tmp/x86-platform-carrier-uefi-first.log`.
* `cargo fmt && cargo xtest -p xtask`, followed by
  `LINUX_KVM_BACKEND=direct-vmx LINUX_KVM_MEMORY=4G LINUX_KVM_CYCLES=64
  X86_UEFI_PCI_PROFILE=firmware-default X86_UEFI_REQUIRE_HIGH_PCI=1
  scripts/x86_64/run-linux-kvm-test.sh`: **33 host PASS, 0 FAIL; Direct Linux
  64 cycles PASS**, `/tmp/x86-platform-carrier-linux-high-first.log`.
  Actual highest BAR end is **0x380000004000**, in the 56 TiB aperture. Linux
  boots, performs the deterministic KVM_RUN I/O/state/remap checks and powers off
  without the previous high-PCI-placement EPT violation.
* All five required `cargo xtest -p` packages: **246 PASS, 0 FAIL** (nested 29,
  HAL 54, loader 120, guest 10, xtask 33). `cargo xbuild x86`, `cargo fmt --check`
  and `cargo xrun x86 --release`: **PASS**; standard suite **9 PASS, 0 FAIL**
  (seven KVM including expected host exception, two TCG).
  `/tmp/x86-platform-carrier-final-checks.log`.
* Exact release nested command: **10 PASS, 5 FAIL** across 15 cases; all nine
  Direct cases pass (six native/MSR contracts and three Linux lifecycle modes).
  Reference Linux passes; the same five previously documented outer-KVM
  partial-operand/PAT-shadow/abort-indicator comparisons fail. Overall command
  exit is **1**, not a clean-suite PASS. `/tmp/x86-platform-carrier-nested-64.log`.
* High-BAR plus live-XSTATE clobber lifecycle, unchanged existing bounded 600s
  long-run limit: **4096 cycles PASS**, poweroff at guest time 322.657906s;
  `/tmp/x86-platform-carrier-high-clobber-4096.log`:

  ```sh
  nix develop --accept-flake-config --command env \
      LINUX_KVM_BACKEND=direct-vmx LINUX_KVM_MEMORY=4G LINUX_KVM_CYCLES=4096 \
      LINUX_KVM_TIMEOUT_SECONDS=600 LINUX_KVM_HOST_XSTATE_TEST=1 \
      X86_UEFI_PCI_PROFILE=firmware-default X86_UEFI_REQUIRE_HIGH_PCI=1 \
      scripts/x86_64/run-linux-kvm-test.sh
  ```

The subsequent fixed-commit replay below retains the unmodified timing-sensitive
failures. No Direct Windows Hyper-V or physical machine was tested. Outer-KVM
results are reference-only.

### Platform carrier replay and host-map materializer

The seven upstream selftests were replayed at detached commit `1c9ac1f` in an
isolated temporary worktree, without editing its sources or running boot artifacts.
Each invocation used the existing `scripts/x86_64/run-linux-selftest.sh` under
`nix develop --accept-flake-config --command`, `LINUX_SELFTEST_BACKEND=direct-vmx`,
`LINUX_SELFTEST_NAME=<basename>` and `LINUX_SELFTEST_ELF=<original built ELF>`.
The ELF root was `/tmp/thin-hv-kvm-selftests-7.1.5.MUloxc/out/`.

* **5 PASS**: `x86/xcr0_cpuid_test`, `x86/dirty_log_page_splitting_test`,
  `x86/nx_huge_pages_test`, `memslot_modification_stress_test`,
  `kvm_page_table_test`.
* **2 FAIL**: `memslot_perf_test` still exits 142 under its original alarm;
  `x86/vmx_exception_with_invalid_guest_state` still reaches the unchanged
  guest 600-second kill (137). No timeout, signal interval or source was weakened.
* Batch: `/tmp/x86-platform-carrier-selftest-batch.log`; individual logs:
  `/tmp/x86-platform-carrier-direct-<basename>.log`. Host compilation overlapped
  part of this replay, so these are correctness regressions, not isolated timing
  benchmarks. The earlier timing failures are not claimed fixed.

The HAL now reuses the checked EPT table materializer for a separate private
host map. `PlatformMap::host_mappings`, `HostPagingPolicy`, `HostTables` and
`build_host_identity` include RAM (including monitor reservations), omit guest
MMIO apertures, preserve RAM RO/RP constraints, validate low-canonical four-level
addresses, and use CPUID rather than VMX page-size capabilities. Captured PAT WB
selection retains native MTRR typing (Intel SDM Vol. 3A table 12-7); no PAT write
is performed. A private, initially absent strong-UC scratch PTE is provided for
bounded later MMIO access; no hardware mapping is activated by the HAL builder.
All construction errors leave an empty root. This commit is the testable
materializer step, **not yet active HOST_CR3 integration**.

`nix develop --accept-flake-config --command bash -c 'cargo fmt && cargo xtest -p
x86_64_hal'`: **59 PASS, 0 FAIL**, including five new host-map tests (RAM above
8 GiB, private RAM versus PCI exclusion, independent large-page/PAT encoding,
canonical/physical boundaries, malformed PAT, scratch PTE, ownership/cache and
capacity failures). `/tmp/x86-host-platform-window-tests.log`. The initial test
fixture omitted EFI_MEMORY_RUNTIME and correctly failed with RuntimeAttribute;
the fixture was corrected, not the validator. No AArch64 implementation changed.

### Active platform HOST_CR3 and checked guest physical backing

`vmx_smoke::build_carrier_maps` now constructs both roots from the same validated
UEFI/MTRR/GCD/ACPI/PCI inventory. The fixed eight-GiB host builder is removed.
Two separate 256-page EPT/HOST arenas are excluded from the carrier EPT; host RAM
mappings include retained monitor storage and readable firmware RAM, not the
guest PCI apertures. Allocation respects MAXPHYADDR, the low-canonical four-level
limit and BASIC's independent 32-bit VMX-region restriction. Unlocked feature
control is initialized only after platform/host validation, immediately before
VMX use. `CpuSnapshot::host_paging` retains captured PAT and independent CPUID
large-page support; `write_host_state` installs that exact PAT with HOST_CR3.
Preflight additionally sizes (does not activate) the host map.

`CpuRuntimeState` owns MMIO/window metadata and the captured BASIC contract.
RAM operands, paging A/D updates, VMXON/VMCS headers and MSR lists now check actual
RAM coverage/permissions and ownership before dereferencing physical backing.
The L1 bootstrap stack is the explicit exception inside the monitor allocation;
the rest of its private storage cannot be accessed by these operand helpers.
Integer-address scalar assembly avoids Rust null-pointer dereferences for valid
RAM at physical zero, including MSR entries. Page-crossing stores retain their
complete first validation pass. The optional strong-UC MMIO scratch mapping is
per-CPU, serialized by the existing short CPU-state lock and removed/INVLPG'd
before return; no entire guest PCI aperture is copied into HOST_CR3. Its live
device-access branch still requires a focused QEMU fixture; host tests validate
the backing classification and PTE encoding, not actual device transactions.

L0 carrier/error-recording VMCS validation is now explicitly separate from L1
VMCS ownership. The first integration replay exposed the old helper's mixed
callers: adding L1 exclusions rejected L0's own carrier. That run was **1 PASS,
14 FAIL** (nine new Direct failures plus the five known reference failures),
`/tmp/x86-host-platform-nested-first.log`. The complete caller audit and ownership
regression test fixed it; no failed intermediate implementation was committed.
The subsequent 15-case replay was **10 PASS, 5 FAIL**, all Direct passing,
`/tmp/x86-host-platform-nested-owner-fixed.log`.

Final available validation for this increment:

* Under `nix develop --accept-flake-config --command`: `cargo fmt`,
  `cargo fmt --check`, all five required package-filtered `cargo xtest` commands,
  `cargo xbuild x86`, `cargo xrun x86 --release`, and
  `cargo xrun x86 --nested --release` ran sequentially in
  `/tmp/x86-host-platform-final-checks.log`.
* Host tests: **253 PASS, 0 FAIL** (nested 29, HAL 59, loader 122, guest 10,
  xtask 33). Debug x86 build/format: **PASS**. Standard QEMU: **9 PASS, 0 FAIL**
  (seven KVM including the expected root-exception fixture, two TCG).
* Nested now includes mandatory default-layout Direct Linux **12 GiB** as well
  as 4 GiB high-PCI cases: **11 PASS, 5 FAIL** across 16 cases. All ten Direct
  cases pass; reference Linux passes and the same five reference contracts fail.
  The overall nested command still exits **1**.
* The final BASIC-width audit also fixed VMXON/VMCLEAR's missing independent
  32-bit-region check, with pure regression coverage preserving high-address
  RAM/MSR-list access. Afterward `cargo fmt`, loader tests (**122 PASS**),
  `cargo xbuild x86` and the complete release nested suite (**11 PASS, 5 FAIL**,
  all ten Direct PASS) were repeated in
  `/tmp/x86-host-platform-region-width-final.log`.
* Standalone initial 12-GiB Direct Linux lifecycle: **64 cycles PASS**,
  `/tmp/x86-host-platform-linux-12g-first.log`, using
  `LINUX_KVM_BACKEND=direct-vmx LINUX_KVM_MEMORY=12G LINUX_KVM_CYCLES=64
  X86_UEFI_PCI_PROFILE=firmware-default X86_UEFI_REQUIRE_HIGH_PCI=1
  scripts/x86_64/run-linux-kvm-test.sh` under Nix. Active roots were EPT 40 tables /
  18046 leaves and HOST 24 tables / 9621 leaves. This establishes the 12-GiB
  configuration, not that every high RAM page has independently been exercised.

The runner requires ordered paired EPT/HOST completion markers, rejects the old
fixed-8g marker, malformed counts, missing/duplicate/out-of-order roots and
non-UC windows; high-PCI cases still require an actual BAR above 8 GiB. There is
no fallback to outer KVM. Remaining physical blockers include shared runtime
bootstrap/overlay, full private-page reservation, post-launch MTRR/UEFI memory
permission lifecycle, AP ownership, root NMI and S3 re-entry. The previous two
timing-sensitive Direct KVM failures await replay at this new increment. No
Windows/Hyper-V/WSL2 or physical hardware was tested; outer-KVM is reference only.

### Live host MMIO-window regression fixture

`probe_q35_host_window` runs only in the existing explicitly QEMU-only
`host-xstate-test` build, on its first intercepted L1 CPUID. It reads the immutable
q35 host-bridge header and an absent PCI function through two different scratch
mappings, returns to the first page, and verifies that each of 12 accesses left
the private PTE non-present. The firmware-derived MMIO inventory must separately
authorize each byte; there is no direct identity-map fallback. No PCI write or
device/firmware identity modification occurs. The q35 ECAM fixture addresses are
compiled out of production, and neither observed value is logged. Guest XSTATE
is protected by the normal exit bracket throughout the test.

Native and Linux XSTATE fixture transcript gates now require exactly one
`host MMIO window PASS reads=12 mappings=12 pages=2 returns=1 pte_clear=1` record.
Unit coverage rejects missing/duplicate/malformed/early records and double-CR
line endings, while accepting ordinary LF and CRLF. The first actual probe
passed but emitted CRCRLF because the existing serial writer already expands LF;
its strict gate correctly rejected the extra CR. The emitter was fixed, not the
gate. Initial command: **155 host tests PASS; nested 9 PASS, 7 FAIL** (two new
transcript failures plus five known reference failures),
`/tmp/x86-host-window-live-fixture.log`.

After the newline fix, under Nix: `cargo fmt`, `cargo xtest -p xtask` (**33 PASS**)
and `cargo xrun x86 --nested --release`: **11 PASS, 5 FAIL**, all ten Direct cases
PASS. The live MMIO proof appears once in native/XSTATE and once in Linux/XSTATE,
with the full existing guest-state/lifecycle tests still passing.
`/tmp/x86-host-window-live-fixture-fixed.log`. `cargo fmt --check` and
`cargo xbuild x86`: **PASS**, `/tmp/x86-host-window-debug-format.log`.

In parallel, the preceding production commit `611bc3c` is frozen in a separate
temporary worktree (no running source/artifact edits). Its **12-GiB, default high
PCI, live-XSTATE-clobber 4096-cycle Direct lifecycle PASS** is recorded in
`/tmp/x86-host-platform-clobber-4096.log`; clean guest poweroff at 337.431829s.
The command uses the existing runner with `LINUX_KVM_CYCLES=4096`, unchanged
bounded `LINUX_KVM_TIMEOUT_SECONDS=600`, `LINUX_KVM_MEMORY=12G`,
`LINUX_KVM_BACKEND=direct-vmx`, `LINUX_KVM_HOST_XSTATE_TEST=1`,
`X86_UEFI_PCI_PROFILE=firmware-default`, `X86_UEFI_REQUIRE_HIGH_PCI=1`.
The five previously passing upstream KVM regressions also pass at `611bc3c`;
`memslot_perf_test` still fails with its original alarm/exit 142. The final
`vmx_exception_with_invalid_guest_state` replay remains running when this
fixture commit is recorded; its result is not presumed.

These are QEMU/KVM results, not physical-machine or Hyper-V qualification. The
live scratch probe establishes MMIO reads/remapping/cleanup, not a device-write
transaction. No AArch64 implementation or third-party dependency changed.

The fixed-`611bc3c` batch subsequently completed: **6 PASS, 2 FAIL** across the
4096-cycle run plus seven upstream selftests. The original 600-second
`vmx_exception_with_invalid_guest_state` limit again killed the guest test (137),
so neither timing-sensitive Direct failure is fixed. Batch exit **1**;
`/tmp/x86-host-platform-frozen-batch.log` and
`/tmp/x86-host-platform-direct-vmx_exception_with_invalid_guest_state.log`.
No live files in that worktree were edited. Main-worktree QEMU work overlapped;
this is not an isolated performance comparison.

Before removing the overlay or excluding all L0 image pages, audit the runtime
PE's own lifetime, not just its host tables. EDK2 registers runtime image bases
and relocation data while loading them
([Core/Dxe/Image/Image.c](https://github.com/tianocore/edk2/blob/master/MdeModulePkg/Core/Dxe/Image/Image.c))
and applies runtime relocations during `SetVirtualAddressMap`
([Core/RuntimeDxe/Runtime.c](https://github.com/tianocore/edk2/blob/master/MdeModulePkg/Core/RuntimeDxe/Runtime.c)).
The checked-out monitor still executes a firmware-loaded runtime PE through an
identity HOST_CR3. Whether this rewrites particular monitor pointers in the
current QEMU run needs direct measurement; it is not yet asserted as the cause
of either KVM timing failure or the old Windows watchdog. Bootstrap separation
and an independently owned L0 code copy remain architectural work, not merely a
variable-overlay switch.

## Step 10c: independent resident L0 image and explicit L1 bootstrap

The runtime-image lifetime concern above was confirmed, not merely inferred.
At `bab204b`, an HMP diagnostic read eight DIR64 slots in the project monitor
before Linux startup and after its UEFI virtual-address transition. **8/8 changed
by `0xfffffffe7fc00000`**, although HOST_CR3 remained physical/identity-addressed.
Only eight-byte project pointer slots were captured, not firmware payloads.
The accompanying default-q35/4-GiB Direct lifecycle completed **1024 cycles**.
`/tmp/x86-runtime-relocation-probe.log`, `/tmp/x86-runtime-relocation-linux.log`.
Pausing for these reads makes this diagnostic unsuitable as a timing benchmark.
This establishes a code-pointer lifetime bug, not the cause of the two upstream
timing failures or the old Windows watchdog.

Changed files and principal symbols:

* `x86_uefi_loader/src/resident_image.rs`: `LoadedPe`, `image_pages`, and
  `GuardedAllocation`. The bounded, heap-free copier accepts this project's
  already authenticated/loaded AMD64 PE32+ virtual layout, not an arbitrary
  on-disk executable. It validates all sections and DIR64 relocations before
  publishing a destination: header/size bounds, overflow, executable entries,
  unsupported imports/TLS, unsupported/overlapping/self-modifying relocations,
  external pointers, capacity and source/destination overlap. One-past-image
  pointers and upward/downward rebasing are supported. Four host tests cover
  these cases and the shared guard/private-payload boundaries.
* `x86_uefi_loader/src/vmx_smoke.rs`: `start_resident_core`,
  `with_runtime_pages`, `ResidentHandoff`, `resident_entry`, `GuestBootstrap`,
  `run_direct_monitor`, `build_carrier_maps`, `configure_and_launch`,
  `write_guest_state`, `finish_vmcall`, allocation validation and cold EPT-fault
  diagnostics. Firmware authenticates the source normally. L0 executes a
  separate RuntimeServicesCode allocation that is **not registered as a runtime
  PE**, so firmware cannot relocate its pointers for L1. The original runtime
  image owns the explicit guest trampoline/result atomics and legacy research
  overlay only. VMCS host callbacks, descriptors, diagnostics and VMX globals
  resolve into the private copy. Initial-entry failures restore VMX/CPU state,
  report errors while copied strings remain valid, return only a scalar status,
  roll back the original overlay, and release both owned allocations.
* `x86_uefi_loader/src/main.rs`: the private image module.
* `scripts/x86_64/run-uefi-smoke.sh`, `xtask/src/main.rs`: require ordered,
  bounded resident/EPT/HOST records; reject overlapping/out-of-width copies,
  missing guards, insufficient private-page counts, shared-image/fixed-map
  claims and malformed records. Existing package filters and test kinds remain.

The active EPT now excludes the whole private PE and the VMX/host block except
its explicit L1 stack. This includes VMXON, carrier/error VMCS, EPT/HOST tables,
MSR bitmap, host stack, GDT/IDT/TSS/IST and per-CPU data. The shared firmware
bootstrap is no longer mistaken for L0-private code. Four extra zeroed pages
(one before/after each allocation) remain L1-visible boot guards; they are not
VMXON or other private payload. No protection against a malicious trusted L1,
full physical readiness, or SMP is claimed.

### Failures diagnosed during integration, not committed as working states

1. Excluding all private pages initially broke the four Direct Linux cases:
   **7 PASS, 9 FAIL / 16** (five failures are the existing reference discrepancies).
   Native Direct cases passed. Precise cold GPA/GLA logging identified a read
   at the first VMXON byte, with RSI at that address. Matching the installed
   7.1.5 `System.map` located RIP at `copy_bootdata+0x47`, RDI at
   `boot_command_line+0x1e8`. The pinned Linux `arch/x86/kernel/head64.c` copies
   fixed `COMMAND_LINE_SIZE` (2048) bytes even when the EFI command-line buffer
   is shorter. Its allocation ended immediately before the new L0 reservation.
   Shared, zeroed page bookends cover that bounded read; **private pages were
   not remapped** and the kernel/test was not modified. The guard is explicitly
   not a workaround for arbitrary scans or future unexplained EPT faults.
   Logs: `/tmp/x86-resident-copy-nested-first.log`,
   `/tmp/x86-resident-private-ept-diagnostic.log`.
2. Linux then passed that boundary but faulted on an NX legacy SetVariable hook
   after `SetVirtualAddressMap`. Runtime allocations made after overlay setup
   caused OVMF to republish its MAT, losing the old image's existing test-only
   attribute adjustment. Both runtime allocations now precede original-image
   overlay installation. No additional runtime allocation occurs in copied L0
   preparation. Rollback precedes freeing those allocations. This ordering fix
   preserves the legacy backend; it does not install an overlay in a physical
   no-overlay path. `/tmp/x86-resident-boot-guards.log` (FAIL),
   `/tmp/x86-resident-overlay-order.log` (64-cycle Direct PASS).

### Completed validation

All commands below used `nix develop --accept-flake-config --command`:

* `cargo fmt`, `cargo fmt --check`, all five requested `cargo xtest -p` packages:
  **261 PASS, 0 FAIL** = nested_vmx 29 + x86_64_hal 59 + x86_uefi_loader 130 +
  x86_guest_uefi_test 10 + xtask 33. `cargo xbuild x86`: **PASS**.
* `cargo xrun x86 --release`: **9 PASS, 0 FAIL** (7 QEMU/KVM, including the
  expected private root-exception fixture, and 2 QEMU TCG cases).
* `cargo xrun x86 --nested --release`: **11 PASS, 5 FAIL / 16**, exit **1**.
  **All ten Direct cases PASS**, including native/readonly/live-XSTATE,
  non-empty MSR lists, architectural MSR aborts, and 2/4/12-GiB Linux lifecycle
  runs with default high PCI apertures. The five outer-KVM contract differences
  are unchanged and are not waived. `/tmp/x86-resident-core-validation.log`.
* Existing Linux runner with `LINUX_KVM_BACKEND=direct-vmx`,
  `LINUX_KVM_CYCLES=1024`, `LINUX_KVM_MEMORY=4G`,
  `X86_UEFI_PCI_PROFILE=firmware-default`, `X86_UEFI_REQUIRE_HIGH_PCI=1`:
  **PASS**, including all lifecycle/state checks and S5 poweroff. During this
  run HMP sampled the same eight DIR64 RVAs in both copies before/after the UEFI
  transition: **private 8/8 unchanged**, **original 8/8 converted**, all original
  deltas `0xfffffffe7fc00000`. Only 256 bytes of project pointers were captured;
  this paused experiment is not timing qualification.
  `/tmp/x86-resident-relocation-probe.log`,
  `/tmp/x86-resident-relocation-linux.log`; runner and probe exit **0**.

No Direct Windows, Hyper-V/WSL2, S3 or physical-machine retest is included in
this step. The original firmware/Windows installation was not touched. The two
timing-sensitive Direct selftest failures remain open pending a new frozen-code
replay. Physical selection/no-overlay, full pCPU/AP ownership, root NMI and S3
lifecycle work remain. Outer-KVM results are reference evidence only.

## Step 11 — Direct physical selection without a variable overlay

The checked-out code still installed `runtime_variables::install` for every
Direct launch. Added the explicit `physical-direct-vmx` build variant without
compiling that module. Its `physical-uefi` log mode is separate from the retained
`qemu-research` mode; neither can pass the other's runner gate. This is a
physical-path implementation **tested in QEMU, not physical qualification**.

Changed files and major symbols:

* `x86_uefi_loader/Cargo.toml`, `src/main.rs`: the feature, module exclusion and
  explicit mode markers. No Runtime Services overlay, MAT hook adjustment,
  identity synthesis, product-key handling or outer-KVM fallback in this mode.
* `src/physical_chainload.rs`: extracted `load_selected` from the existing
  baseline. Both callers use its checked UTF-16 path/current-ESP selection,
  self-reference rejection and normal firmware LoadImage; no enumeration-first
  filesystem fallback. `require_single_cpu` queries MP Services read-only before
  OS loading or VMXON. `single_bsp` rejects all additional processors, including
  disabled APs, until real pCPU ownership/AP startup exists. Missing or malformed
  inventory also fails. No processor is disabled or hidden by this gate.
* `src/vmx_smoke.rs`: selects the shared physical loader or retained research
  loader at build time. `runtime_handoff`, `validated_runtime_handoff` and
  `valid_runtime_profile` reject null guests and mixed physical/research handoffs
  before allocation/VMX. Only research builds install/roll back the old overlay.
* `xtest.txt`, `xtask/src/main.rs`: package-filtered physical-variant host tests;
  `build_x86_direct_variant` reuses the existing build/copy/ISA audit pipeline;
  `run_x86_nested` adds physical native, 2/12-GiB Linux and 2-CPU rejection cases.
* `scripts/x86_64/run-linux-kvm-test.sh`: `LINUX_KVM_DIRECT_MODE` chooses explicit
  artifacts; the test UKI is staged at the same-ESP default path. Its Windows
  filename is **not a Windows installation test**.
* `scripts/x86_64/run-uefi-smoke.sh`: `check_direct_mode_log` rejects missing,
  mixed, malformed, excess and overlay-contaminated evidence.
  `check_cpu_ownership_reject_log` accepts only bounded complete pre-VMX rejection
  attempts with QEMU exit 0; timeout, partial attempts, successful guest/runtime
  markers and wrong CPU counts fail. OVMF can retry via another Boot####, so
  complete retries are checked individually, not ignored. The ordinary Direct
  success gate still rejects this negative fixture.

### Completed validation (2026-09-10 local date)

All Cargo commands used `nix develop --accept-flake-config --command`.

* `cargo fmt`, `cargo fmt --check`; `cargo xtest -p nested_vmx`,
  `-p x86_64_hal`, `-p x86_uefi_loader`, `-p x86_guest_uefi_test`, `-p xtask`:
  **314 PASS, 0 FAIL** = 29 + 59 + 183 + 10 + 33.
* `cargo xbuild x86`: **PASS**. `cargo xrun x86 --release`: **9 PASS, 0 FAIL**
  (7 QEMU/KVM, including the expected root-fault case; 2 QEMU TCG).
* `cargo xrun x86 --nested --release`: **15 PASS, 5 FAIL / 20**, exit **1**.
  All **14 Direct cases PASS**, including the explicitly negative CPU gate;
  this is not 14 proofs of successful SMP/nested execution. The existing five
  outer-KVM contract differences remain FAIL. Complete log:
  `/tmp/x86-physical-direct-core-validation.log`.
* Existing Linux runner, `LINUX_KVM_BACKEND=direct-vmx`,
  `LINUX_KVM_DIRECT_MODE=physical-uefi`, `LINUX_KVM_CYCLES=64`,
  `LINUX_KVM_MEMORY=4G`, `X86_UEFI_PCI_PROFILE=firmware-default`,
  `X86_UEFI_REQUIRE_HIGH_PCI=1`: **PASS**, including KVM_RUN, state/lifetime
  checks and poweroff. `/tmp/x86-physical-direct-linux-first.log`.
* Explicit 2-CPU KVM rejection fixture: **PASS**, project VMX never started;
  `/tmp/x86-physical-direct-smp-reject-retries.log`. Its earlier checker rejected
  two valid OVMF retry attempts; the bounded retry regression now covers that
  behavior without accepting partial or post-VMX failures.

### Frozen step-10c replay, separate from this variant

Detached worktree `/tmp/x86-resident-regressions.x35Iff`, commit `8598e86`,
completed **6 PASS, 2 FAIL / 8**; `/tmp/x86-resident-frozen-batch.log`.

* Direct Linux live-XSTATE-clobber **4096 cycles**, 12 GiB/default high PCI:
  **PASS**, unchanged 600-second bound. `/tmp/x86-resident-clobber-4096.log`.
* Original pinned 7.1.5 test ELFs: **PASS** `x86/xcr0_cpuid_test`,
  `x86/dirty_log_page_splitting_test`, `x86/nx_huge_pages_test`,
  `memslot_modification_stress_test`, `kvm_page_table_test`.
* **FAIL** `memslot_perf_test` (original alarm, status 142) and
  `x86/vmx_exception_with_invalid_guest_state` (original 600-second bound,
  killed status 137). No test changes or timeout increases. Logs:
  `/tmp/x86-resident-direct-memslot_perf_test.log` and
  `/tmp/x86-resident-direct-vmx_exception_with_invalid_guest_state.log`.

This replay overlapped other QEMU work and is not an isolated performance
benchmark. No Direct Windows/Hyper-V/WSL2, S3 or physical-machine retest was
performed in step 11. pCPU/AP ownership, root NMI, S3 lifecycle and the two
timing-sensitive Direct failures remain open. Multicore physical boot remains
deliberately gated; do not try the original OEM Windows installation yet.
Outer-KVM evidence is reference-only. No AArch64 implementation or pre-existing
user `AGENTS.md` changes are part of this step.

## Step 12a — CPU-local runtime state, not AP enablement

Confirmed at `ef01276`: MSR mirrors/VPID/physical-access state were already
GS-bound, but `L1_VCPU_STATE`, `CARRIER_PATCH_VALUES`, `DIRECT_PATCH_VALUES`,
`DIRECT_ENTRY_POLICY`, `NESTED_RUN`, `EXIT_DIAGNOSTICS`, original control-register
values and the physical-width cache were still BSP globals.

`x86_uefi_loader/src/vmx_smoke.rs` now initializes a bounded `CpuMonitor` in the
existing CPU-owned reserved block. `current_cpu` obtains that exact object via
the existing `HostEnvironment::bind_monitor_data`/private GS mechanism; all
nested instruction handlers, reflection, immediate nested-entry failure and
diagnostics use its fields. The allocation owns the VMXON/carrier/error VMCS,
host stack, GDT/IDT/TSS/IST, XSTATE scratch, MSR bitmap and private HOST_CR3 arena
as before. No new global lock or shared singleton replaces the deleted globals.

Separate existing SpinLocks protect short metadata borrows. In particular,
diagnostics do not acquire the MSR/runtime lock: list processing can count VMCS
accesses while holding that lock. No guard crosses VM entry. The expanded host
test constructs two independent objects, mutates VMX/current-VMCS/patch/policy/
nested-run/diagnostic state on only one and verifies the other remains empty.
It also checks bounded storage, list alignment and disjoint diagnostic/MSR slots.
This is a host ownership test, **not a two-pCPU hardware launch test**.

Initial preparation has no private GS installed yet. `FirmwareControls` keeps
the original CR0/CR4/XCR0 on that CPU's live preparation stack; VMXON/initial
entry failure restores them only after VMX is inactive. Initial VMCS setup,
logging and `initial_vm_instruction_error` use raw HAL access, not post-exit
diagnostic wrappers. The counters now count runtime accesses from the first VM
exit, excluding one-time boot setup. Normal L2 reflection still does **not**
restore an obsolete L1 extended-state/control snapshot. The immutable checked
physical width resides in each `CpuMonitor`; firmware discovery has no cache.

Only the four intentional **L1 bootstrap** atomics remain in this module
(`GUEST_IMAGE`, `SYSTEM_TABLE`, `GUEST_RAN`, `GUEST_STATUS`). Runtime VMX state is
no longer in those image globals. The separately gated research variable
overlay is unchanged; production-selection builds still omit it.

`scripts/x86_64/decode-vmx-diagnostics.py` recognizes the explicit
`storage=cpu-runtime` publication and checks its reserved allocation bounds,
alignment, ordering, duplication and image disjointness before reading the same
176-byte ABI. Legacy image-scoped records remain explicitly supported for old
A/B captures. Host regressions cover wrong-owner records and allocations above
8 GiB up to the current private host-map ceiling, without reading any guest or
firmware payload.

Completed validation, all Cargo through the repository Nix environment:

* `cargo xtest -p x86_uefi_loader` and `cargo xtest -p xtask`: **183 + 33 PASS,
  0 FAIL**, including the decoder's pure self-tests.
* `cargo xtest -p nested_vmx`, `-p x86_64_hal`, `-p x86_guest_uefi_test`:
  **29 + 59 + 10 PASS, 0 FAIL**. Combined requested packages: **314 PASS**.
* `cargo xbuild x86 --release`, `cargo xbuild x86`: **PASS**.
* `cargo xrun x86 --nested --release`: **15 PASS, 5 FAIL / 20**, exit **1**;
  all **14 Direct cases PASS**, including the negative SMP gate. The five
  reference discrepancies are unchanged. `/tmp/x86-cpu-local-first.log`.
* `cargo xrun x86 --release`: **9 PASS, 0 FAIL** (7 KVM/2 TCG, with the explicit
  expected root-fault case). `/tmp/x86-cpu-local-core.log`.
* Existing Linux runner with `LINUX_KVM_BACKEND=direct-vmx`,
  `LINUX_KVM_DIRECT_MODE=physical-uefi`, `LINUX_KVM_CYCLES=4096`,
  `LINUX_KVM_MEMORY=12G`, `LINUX_KVM_TIMEOUT_SECONDS=600`,
  `X86_UEFI_PCI_PROFILE=firmware-default`, `X86_UEFI_REQUIRE_HIGH_PCI=1`:
  **PASS**, all 4096 cycles and S5 poweroff, runner exit **0**.
  `/tmp/x86-cpu-local-4096.log`. A read-only HMP sidecar captured only the
  published 176-byte CPU-owned record at `0x7e6f3010`: valid even sequence,
  257445 observed L2 entries/reflections and zero nested-entry failures at that
  sample. `/tmp/x86-cpu-local-capture-retry.log`, sidecar exit **0**. The first
  sidecar attempt used a wrong start-marker anchor; its recorded terminal result
  was a shell parse error, exit **2**, before pausing/capturing. Restarting the
  corrected diagnostic script captured and resumed the same workload. The
  original combined wrapper reports exit **1** (`runner=0 probe=2`) for that
  first sidecar failure, not a Linux failure. The
  paused run is lifetime/state evidence, not a performance benchmark.

AP initialization, INIT/SIPI delivery and cross-pCPU lifecycle qualification are
still pending. The physical multi-CPU rejection gate stays enabled. This step
does not claim SMP, S3, Direct Hyper-V/WSL2 or physical readiness. No physical
machine or Windows installation was tested. Outer KVM remains reference-only.

## Step 12b — preserve virtual VMX operation on intercepted CR4 writes

Additional finding confirmed at `53cf4d1`: `dispatch_l1_exit` accepted an L1
write clearing CR4.VMXE while its CPU-local `VcpuState` was still in VMX
operation. It forced hardware VMXE back on but cleared the L1 read shadow,
omitting the architectural #GP. The L0 hardware bit cannot validate the L1
state that the mask/shadow hides. [Intel SDM volume 3C, section 24.7](https://cdrdv2-public.intel.com/671506/326019-sdm-vol-3c.pdf)
requires VMXOFF before clearing VMXE.

* `x86_uefi_loader/src/vmx_smoke.rs`, `dispatch_l1_exit`: checks the calling
  CPU's nested VMX lifecycle before changing either CR4 field. An attempted
  clear inside VMX operation injects #GP(0) with RIP and CR4 unchanged. Outside
  VMX operation the existing successful write path remains. This targeted fix
  is not a claim of complete CR0/CR4 or AP reset emulation.
* `x86_guest_uefi_test/src/nested_contract.rs`, `vmxe_cannot_clear_in_vmx`,
  `instructions`: uses the existing exact-RIP L1 fault fixture twice, without
  and with a current VMCS. It validates exception vector/error and unchanged
  visible CR4, then continues the ordinary VMX contract and VMXOFF cleanup.
  On assertion failure it first restores the known-valid VMXE-enabled CR4 so
  cleanup remains executable. No alternate test framework or L2-only CPUID
  route is used.
* `scripts/x86_64/run-uefi-smoke.sh`, `check_nested_contract_log`, and
  `xtask/src/main.rs`: require the exact two-probe/state-preserved marker
  between contract start and final PASS. Host tests reject missing, duplicated,
  malformed, wrong-count and reordered evidence.

The focused pre-fix QEMU/KVM Direct run **FAILed as intended**, exit **1**:
`cr4-vmxe-guard-vector actual=0xffffffffffffffff expected=0xd` (no exception).
The test recovered, completed VMX cleanup and returned; it did not crash L0.
Command, through Nix: `cargo xbuild x86 --release`, then the existing
`run-uefi-smoke.sh` with the normal Direct loader/native-contract EFI,
`host,+vmx,-hypervisor,kvm=off`, one CPU, 256 MiB and the existing 30-second bound.
Log: `/tmp/x86-cr4-vmxe-before.log`.

After the fix, through `nix develop --accept-flake-config --command`:

* All five requested package-specific host tests: **314 PASS, 0 FAIL**
  (nested_vmx 29, x86_64_hal 59, x86_uefi_loader 183,
  x86_guest_uefi_test 10, xtask 33).
* `cargo xrun x86 --nested --release`: **15 PASS, 5 FAIL / 20**, exit **1**;
  all **14 Direct cases PASS**, including the negative CPU gate. The new CR4
  probe also passes on the outer-KVM reference before its unrelated existing
  operand-contract failure. `/tmp/x86-cr4-vmxe-after.log`.
* `cargo xbuild x86`, `cargo xrun x86 --release`: **PASS**, the latter **9 PASS,
  0 FAIL** (7 KVM/2 TCG including the expected root exception).
  `/tmp/x86-cr4-vmxe-core.log`. Formatting and diff checks: **PASS**.

No capability bits, timeouts, Windows installation or AArch64 implementation
were changed. AP/INIT/SIPI, root NMI and S3 lifecycle work, the two Direct
timing-sensitive failures and Direct Windows/Hyper-V/WSL2 revalidation remain.
No physical hardware was tested; outer KVM is reference evidence only.

## Step 12c — atomic foreign page-table A/D updates before AP enablement

Confirmed at `b222219`: `CpuRuntimeState::paging_word` read a foreign paging
word, ORed A/D bits in software and stored the entire old word. Another CPU's
concurrent PFN/permission update could be overwritten. This was protected only
by the old one-L1-CPU assumption, not a cross-CPU atomic operation.

`x86_uefi_loader/src/vmx_smoke.rs`, `paging_word`, now uses x86 `LOCK OR` for
nonzero A/D updates and an aligned scalar load for reads. It rejects bits other
than A/D before accessing memory, retains the existing RAM/permission/private-
ownership checks and never applies an atomic update to MMIO. The operation
preserves concurrently changed non-A/D bits rather than writing a stale PFN.
The guest page-table/TLB invalidation protocol remains L1's responsibility;
this change alone does not establish SMP support or a new page-table snapshot.

`paging_ad_update_preserves_concurrent_foreign_word_updates` exercises the
actual scalar helper on a host-owned atomic word while a second host thread
performs 65536 atomic non-A/D changes. It also covers unsupported update bits,
unaligned access and unchanged memory on rejection. No VMX, privileged register
instruction, firmware or guest image runs in this test.

Pre-fix command, through Nix:
`cargo xtest -p x86_uefi_loader -- paging_ad_update_preserves_concurrent_foreign_word_updates`.
Result: **1 FAIL, 2 PASS** across the three Direct feature variants, overall exit
**1**. One execution observed `267173987` instead of `268435555`, losing 308
concurrent increments. Scheduling let two other executions pass; the reproduced
lost update is the failure, not a claim that every schedule reproduces it.
`/tmp/x86-paging-ad-before.log`.

Post-fix validation, all through
`nix develop --accept-flake-config --command bash -c`:

```sh
cargo fmt
cargo xtest -p x86_uefi_loader
cargo xtest -p x86_64_hal
cargo xtest -p nested_vmx
cargo xtest -p x86_guest_uefi_test
cargo xtest -p xtask
cargo xbuild x86
cargo xrun x86 --release
cargo xrun x86 --nested --release
```

`/tmp/x86-paging-ad-after.log`: host tests **317 PASS, 0 FAIL** (loader
186, HAL 59, nested policy 29, guest 10, xtask 33); debug build **PASS**;
standard smoke **9 PASS, 0 FAIL** (7 QEMU/KVM, 2 QEMU TCG); release nested
suite **15 PASS, 5 FAIL / 20**, overall exit **1**. All **14 Direct cases PASS**,
including the physical-mode two-CPU rejection test (not SMP execution).
The five unchanged outer-KVM contract failures remain the native/read-only
partial operand stores, late MSR-entry PAT visibility and store/load abort
indicator cases. They are not waived. The atomic regression passes in all
three Direct host feature configurations.

No AP is enabled by this change. Windows/Hyper-V, root NMI and S3 work and
the two timing-sensitive Direct failures remain unverified/unresolved.
No physical hardware was tested. Outer KVM is reference evidence only.

## Windows PCI regression preparation — normal q35 layout by default

Confirmed at `f95bb80`: `windows-test.sh` still unconditionally passed a 1 GiB
PCI aperture and OVMF 1024 MiB override, even though the active Direct carrier
already uses the checked platform map. The runner now defaults to
`WINDOWS_PCI_PROFILE=firmware-default` with neither override. The old layout
remains the explicit `q35-smoke-1g` A/B fixture, matching the existing UEFI
runner. Invalid profiles fail before building or modifying test state; no
fallback is possible. Each boot records its selected PCI profile separately
from its Direct/reference provenance.

Changed symbols: Windows `configure_pci_profile`, `run_windows`, `usage` and
the read-only `print-pci-args` CLI; xtask
`windows_pci_profile_defaults_to_firmware_layout_without_fallback`. The UEFI
runner's old cross-reference comment was corrected, without behavior change.
`nix develop --accept-flake-config --command bash -c 'cargo fmt && cargo xtest -p xtask'`:
**34 host PASS, 0 FAIL**, `/tmp/x86-windows-pci-host.log`. The test exercises the
actual array builder, exact default/fixture arguments and rejection before
normal/Hyper-V/outer-reference runs. Windows QEMU revalidation follows this
commit; this host check does not establish Windows boot or physical readiness.

## Windows platform regression — discover TPM2 CRB resources

Frozen commit `41f215d`, worktree `/tmp/x86-windows-platform-validation`,
artifacts `/tmp/x86-windows-platform.FqD7UD`. The existing QEMU Windows runner
used `WINDOWS_PCI_PROFILE=firmware-default`, `WINDOWS_MEMORY=4G`, separate
`WINDOWS_TEST_DIR`s and disposable qcow2 overlays/copied TPM and variable
stores. The base evaluation installation was not changed. These are **not**
tests of the motherboard's original Windows installation.

- `windows-test.sh monitor`: **FAIL**, runner exit **1**.
- `windows-test.sh monitor-hyperv`: **FAIL**, runner exit **1**.
- `windows-test.sh trusted-kvm-wsl`: **PASS**, runner exit **0**, exact stamped
  WSL2 marker and final poweroff. This is **outer-KVM/reference only**.

Both Direct boots stopped before the desktop at the same read of physical
`0xfed40044`: exit reason `0x30` (EPT violation), qualification `0x181`.
The platform inventory included the high PCI aperture
`[0x380000000000,0x400000000000)`, but omitted TPM CRB registers. Inspection of
both captured screens showed the TianoCore boot screen, not a Windows bugcheck.
The terminal L0 failure preceded the runner result: both failed QEMUs were
explicitly quit after screenshot capture instead of waiting 600/900 seconds.
An auxiliary 176-byte HMP counter-save attempt failed command parsing
(`invalid char 't' in expression`); **no valid counter capture is claimed**.
The main matrix exited **0** because it records each independent case's status;
the matrix is **1 PASS, 2 FAIL**, not an overall Direct PASS. All three qcow2
checks passed, source EFI hashes matched, and before/after seed metadata and
variable-store hashes were identical. `/tmp/x86-windows-platform-batch.log`.

Confirmed cause in `platform_acpi::Tables`: only MCFG and MADT resource payloads
were allowlisted. Added `tpm2`, duplicate/header discovery and
`tpm2_crb_range`; `Tables::mmio` now contributes the firmware-derived CRB range
to the existing GCD/PCI union and platform EPT materializer. There is no fixed
TPM physical address, synthetic device, runtime mapping-on-fault or QEMU
fallback. Existing conflict/private-region/physical-width checks still apply.

The supported profile is x86 PC-client CRB start method 7, TPM2 table revisions
4/5, with the control address at locality-zero offset `0x40` and five 4 KiB
locality banks. Table checksum, permitted lengths, class/reserved fields,
alignment, overflow and complete extent are checked before publication.
Other start methods fail with `UNSUPPORTED`; RAM-backed control areas and
additional non-PTP apertures are not inferred. This is not complete TPM
transport discovery or physical qualification. Layout references:
[TCG ACPI specification](https://trustedcomputinggroup.org/wp-content/uploads/TCG-ACPI-Specification-Version-1.4-Revision-14_28November23.pdf),
[PC Client PTP §6.5.3.4](https://trustedcomputinggroup.org/wp-content/uploads/PC-Client-Specific-Platform-TPM-Profile-for-TPM-2p0-v1p07_rc1_12Dec2025.pdf).

No TPM registers, buffers, event-log contents or keys are read by this
discovery. Firmware tables and TPM/activation identity are not rewritten.
The existing ACPI MMIO marker remains unchanged; a separate TPM2 marker records
the source with `device_probes=0`. An initial attempt to add a field inside the
existing marker failed the strict preflight transcript check despite the UEFI
preflight itself returning PASS. That log-ABI mistake was corrected, not waived.
`/tmp/x86-tpm2-crb-validation.log`: **323 host PASS, 0 FAIL**, debug build
**PASS**, standard suite interrupted by that **1 transcript FAIL**; nested
suite not reached in that invocation.

Windows `serial_has_direct_failure` and its read-only CLI detect terminal L0
failure records each polling iteration. `run_windows` now uses its existing
diagnostic/screenshot/owned-process cleanup immediately on such failure.
An unreadable failure log remains an error. xtask covers LF/CRLF terminal
records, successful records and read errors; existing success gates/timeouts
are unchanged. `tpm2_crb_uses_firmware_address_and_checks_complete_locality_extent`
tests valid revisions/lengths, non-q35/high addresses, the last valid physical
extent, malformed tables, unsupported transports and overlapping resources;
root-discovery tests include duplicate TPM2 headers without any MSDM payload.

Corrected validation, through Nix:
`cargo fmt && cargo xtest -p x86_uefi_loader && cargo xtest -p xtask && cargo xbuild x86 && cargo xrun x86 --release && cargo xrun x86 --nested --release`.
`/tmp/x86-tpm2-crb-regression.log`: loader **190 PASS**, xtask **35 PASS**,
**0 host FAIL**; debug build **PASS**; standard smoke **9 PASS, 0 FAIL**
(7 QEMU/KVM, 2 QEMU TCG); nested **15 PASS, 5 FAIL / 20**, exit **1**,
all **14 Direct cases PASS**. The five unchanged outer-reference contract
failures remain visible. Together with the unchanged-package results from
`/tmp/x86-tpm2-crb-validation.log`, required host coverage is **323 PASS, 0 FAIL**.
Both real failed Windows serial transcripts also pass the new failure-detector
CLI (detector exit 0 means a failure was found, not a Windows PASS).
Windows reruns on the CRB fix follow; no physical machine was tested.

## Windows rerun at `773316d`: CRB fixed, PPI and Hyper-V boot still FAIL

Frozen worktree `/tmp/x86-windows-crb-validation`, matrix
`/tmp/x86-windows-crb.5IibIJ`, orchestration log
`/tmp/x86-windows-crb-batch.log`. Release EFI build passed. The existing runner
used `firmware-default` PCI, 4 GiB, separate fresh disposable disk overlays,
copied firmware variables and software TPM state. No real OEM installation
or physical TPM was involved.

- `windows-test.sh monitor`: **FAIL**, exit **1**. The new failure gate stopped
  promptly at the terminal EPT violation: reason `0x30`, qualification `0x181`,
  GPA `0xfed45005`, Windows kernel RIP `0xfffff8048117c877`. CRB discovery and
  the high PCI aperture passed; the new missing range belongs to TPM PPI.
  Automatic diagnostics: `direct-normal/monitor-failure-diagnostics.m2nMdN`.
  The validated 176-byte counter record has 390,806 L1 exits, 25,860 CPUID,
  357,832 external-interrupt, 7,106 interrupt-window exits, no L2 entries and
  no nested entry failures.
  The final automatic screenshot was also inspected: the Windows desktop was
  already visible when L0 stopped. Desktop visibility does not override the
  terminal failure or establish successful startup validation.
- `windows-test.sh monitor-hyperv`: **FAIL**, exit **1**, unchanged **600 s**
  timeout without the required completion marker. No terminal L0 failure
  marker was recorded. Automatic diagnostics:
  `direct-hyperv/monitor-hyperv-failure-diagnostics.i4I80d`. The validated
  counter record has 3,690,287 actual Direct L2 entries, 3,690,286 reflections,
  zero nested entry failures, 32,851,561 L1 exits, 6,222 INVEPT and 5,694
  INVVPID operations. It was captured during a nested RDMSR exit; one in-flight
  entry is not a lost-reflection finding. A mid-run screenshot was inspected
  and showed the TianoCore screen with Windows spinning dots, not a bugcheck.
  This does **not** establish Hyper-V/WSL2 usability or a watchdog fix.
  The final automatic screenshot was inspected separately: black background
  with Windows spinning dots, still not a desktop or a bugcheck screen.

Matrix **0 PASS, 2 FAIL**. Orchestration exit 0 only means the independent case
results were recorded. Both qcow2 checks passed; EFI/script hashes matched;
seed disk metadata and variable-store hashes were unchanged. Outer-KVM was
not rerun in this matrix; its earlier success remains reference evidence only.

Read-only TPM preflight found no UEFI descriptor covering the missing PPI page.
The installed PI ACPI SDT protocol is present (version bitmap `0x3e`) in OVMF.
Two initial diagnostic-wrapper runs reached firmware preflight PASS but ended
with runner timeout 124 because the background wrapper dropped monitor stdin.
Preserving its input FD fixed the wrapper; `/tmp/x86-tpm-sdt-preflight-fixed-wrapper.log`
records runner **PASS**, without changing the production timeout or firmware.

## Static ACPI SystemMemory resource discovery

Confirmed that GCD, UEFI GetMemoryMap, PCI BARs and the TPM2 CRB table omit
the PPI page in this QEMU configuration. The solution reuses the firmware's
[PI ACPI SDT protocol](https://uefi.org/specs/PI/1.8/V5_ACPI_System_Desc_Table_Protocol.html),
not a QEMU address constant or a new AML interpreter. The static `TPP2`/`TPP3`
OperationRegions describe bytes in the same physical page accessed by PPI.
[QEMU's TPM PPI definition](https://github.com/qemu/qemu/blob/master/hw/acpi/tpm.c)
was used to diagnose that relationship, not as the mapping address source.

`platform_aml::{Sdt,Inventory,literal,region_range,collect}` reads only DSDT/SSDT
payloads through the installed parser. It validates protocol entry-point RAM,
table lengths/checksums, option types and in-table bounds, arithmetic/physical
width, maximum table/object/depth counts, and closes parser handles on success
and error. The previous child stays live until GetChild obtains its successor.
PI/EDK2 represents an integer TermArg as `CHILD=6`, not `OP=3`; the first probe
rejected that distinction before EPT publication, and the corrected ABI now has
an executable callback regression. No SetOption, notification registration,
AML method, device register or TPM operation is called.

Only literal SystemMemory regions in static Scope/Device/Processor/
PowerResource/ThermalZone containers are mapped. Nonliteral static operands or
a missing SDT protocol return explicit unsupported errors. Methods and
conditional bodies remain unevaluated and are counted in the log; this is
**not complete dynamic AML resource coverage or physical qualification**.
No MSDM payload, key, SMBIOS identity or Secure Boot database is read/replaced.
Normal/ACPI/runtime RAM keeps its existing map and memory types; only holes,
reserved pages and MMIO become UC resources. Existing GCD conflicts, EPT
private ownership and full materialization checks still run before VMXON.

Shared `platform_resources::platform_mmio` now merges this inventory for both
preflight and the active carrier. `main.rs` includes the module only for these
backends. `MemoryMap::test_snapshot` is test-only and uses the unchanged
production map validator. Three host tests cover all literal widths, malformed
operands and extents, non-q35/high addresses, RAM/runtime/NVS preservation,
unavailable memory types, actual PI callback types and handle cleanup.

`/tmp/x86-aml-initial-preflight.log`: initial option-type **FAIL**, no fallback.
`/tmp/x86-aml-child-preflight.log`: corrected TPM-equipped QEMU/KVM preflight
**PASS**, 3 static regions, 2 normalized additional ranges, 71 unevaluated
bodies. Final UC union includes `[0xfed40000,0xfed46000)` and the default high
PCI aperture `[0x380000000000,0x400000000000)`.

`nix develop --accept-flake-config --command bash -c 'cargo fmt && cargo xtest -p x86_uefi_loader && cargo xtest -p x86_64_hal && cargo xtest -p nested_vmx && cargo xtest -p x86_guest_uefi_test && cargo xtest -p xtask && cargo xbuild x86 && cargo xrun x86 --release && cargo xrun x86 --nested --release'`:
`/tmp/x86-aml-full-regression.log`, **335 host PASS, 0 FAIL** (loader 202,
HAL 59, nested policy 29, UEFI guest 10, xtask 35); debug build **PASS**;
standard smoke **9 PASS, 0 FAIL** (7 QEMU/KVM, 2 TCG); nested **15 PASS,
5 FAIL / 20**, all **14 Direct PASS**. The five existing outer-reference
contract differences still make the combined command exit 1. They are not
waived or confused with Direct results. AArch64 production code is unchanged.
Windows revalidation on this map follows separately; no physical machine
or original OEM installation has been tested.
Final `cargo fmt --check`, `git diff --check` and TPM-equipped preflight passed
again on the final code, `/tmp/x86-aml-final-preflight.log`.

## `79cac98` Windows and extended Linux revalidation

Frozen Windows worktree `/tmp/x86-windows-aml-validation`, matrix
`/tmp/x86-windows-aml.hpYBST`, `/tmp/x86-windows-aml-batch.log`:

- `windows-test.sh monitor`: **PASS**, exit **0**, exact
  `thinhvwindowsdesktop` marker from the keyboard/COM2 desktop probe. Default
  q35 high PCI and TPM remain enabled; the PPI EPT violation did not recur.
  Pre-success QEMU running/resume validation and the protected counter decode
  passed. Automatic screenshot was inspected and shows the Windows desktop
  with PowerShell. Counters: 468,919 L1 exits, 430,823 external-interrupt exits,
  no L2 entries, no nested entry failures. This is a QEMU research-mode boot
  with the variable overlay, **not physical Windows qualification**.
- `windows-test.sh monitor-hyperv`: **FAIL**, exit **1**, unchanged 600-second
  timeout, no success marker. Final screenshot shows TianoCore with Windows
  spinning dots, not a bugcheck or desktop. Counters: 3,515,990 actual Direct
  L2 entries/reflections, zero nested entry failures, 31,501,997 L1 exits,
  506,989 external-interrupt exits, 330,493 interrupt-window exits, 576 INVEPT,
  520 INVVPID, 47,787,866 VMPTRLD, 614,065,991 VMREAD, 140,605,917 VMWRITE and
  7,408,217 reflected field writes. The final sampled L1 reason is VMWRITE.
  PPI mapping is fixed, but Hyper-V/WSL2 is still not validated.

Matrix **1 PASS, 1 FAIL**. Both image checks, artifact hashes and seed
metadata/variable-store comparisons passed. Counter/screenshot directories:
`direct-normal/monitor-pre-success-diagnostics.KRKHw1` and
`direct-hyperv/monitor-hyperv-failure-diagnostics.ZJqf70`.
A manual normal-VM capture raced its successful cleanup and obtained no data;
only the valid automatic capture is used above. No outer-KVM run was included;
previous reference success is reference evidence only.

The 12 GiB, default-high-PCI Direct Linux live-XSTATE-clobber fixture was run
with `LINUX_KVM_CYCLES=4096 LINUX_KVM_HOST_XSTATE_TEST=1`:

- Default 300-second runner bound: **FAIL** (exit 1/QEMU 124), cycle 3,487
  completed successfully before timeout. `/tmp/x86-aml-linux-4096.log`.
- The historical 4,096-cycle comparison bound of 600 seconds used at
  `8598e86`: **PASS**, exit 0, all 4,096 cycles and final poweroff. First/last
  cycle guest timestamps are 1.002/352.820 seconds.
  `/tmp/x86-aml-linux-4096-historical-bound.log`.

Both commands use `nix develop --accept-flake-config --command env
LINUX_KVM_BACKEND=direct-vmx LINUX_KVM_CYCLES=4096 LINUX_KVM_MEMORY=12G
LINUX_KVM_HOST_XSTATE_TEST=1 bash scripts/x86_64/run-linux-kvm-test.sh`;
only the comparison run adds `LINUX_KVM_TIMEOUT_SECONDS=600`. **The longer
bound is not a production fix or a performance improvement.** Each successful
cycle exercises two VM contexts, eight rounds, I/O/HLT, remapping, CR2, paging,
XMM0–15/MXCSR, MSR/debug state, TSC and explicit teardown.

Frozen Linux worktree `/tmp/x86-aml-linux-regressions` at `79cac98` used the
unchanged pinned upstream ELFs and `run-linux-selftest.sh` with
`LINUX_SELFTEST_BACKEND=direct-vmx`, `LINUX_SELFTEST_NAME`, `LINUX_SELFTEST_ELF`:
**5 PASS, 3 FAIL / 8**, `/tmp/x86-aml-linux-batch.log`.
PASS: `xcr0_cpuid_test`, `dirty_log_page_splitting_test`, `nx_huge_pages_test`,
`memslot_modification_stress_test`, `kvm_page_table_test`.
FAIL: `memslot_perf_test` (original alarm, process 142),
`vmx_exception_with_invalid_guest_state` (original 600-second limit, process
137), and `LINUX_SUSPEND_BACKEND=direct-vmx run-linux-suspend-test.sh`.
S3 resumed cycle 1 but the fresh KVM probe hit an invalid-opcode Oops while
creating its VMCS (`alloc_loaded_vmcs`); the runner failed immediately on the
Oops. There is still no post-S3 L0 re-entry/lifecycle implementation, and
no capability check was weakened. All these Direct failures remain open.

## Windows no-overlay same-ESP QEMU fixture

The existing `windows-test.sh monitor` / `monitor-hyperv` now accept
`WINDOWS_DIRECT_MODE=physical-uefi`, selecting the already-built physical
Direct loader/monitor pair. Default `qemu-research` stays separate. Invalid
modes, or physical selection with reference/ordinary boot modes, fail before
test-state writes. Success also requires the existing Direct mode/overlay and
platform-map validators; default q35 requires an observed PCI BAR above 8 GiB.
The previous normal-Windows PASS transcript satisfies these additional gates.

The physical loader deliberately accepts only its actual current ESP. Rather
than weakening that rule, `copy_windows_test_esp` / `prepare_monitor_media`
create a fresh disposable QEMU staging ESP containing an unmodified copy of
the evaluation installation's `EFI/Microsoft` boot files and the project EFI
pair. `sfdisk --json` plus `windows-esp-offset.py::{select_offset,integer,
unique_object}` reject unsupported GPT geometry, overflow, overlaps and absent
or ambiguous ESPs. Installed mtools reads the source filesystem; no mount,
sudo, partition write, BCD edit or new dependency is needed. For Hyper-V the
boot files come from its own qcow2 overlay, using a temporary sparse raw
conversion removed after the copy, not the different normal-boot BCD.
This is explicitly **QEMU copied-ESP coverage**, never evidence that an actual
motherboard ESP, original Windows identity or activation has been qualified.

`configure_direct_mode` and read-only `print-direct-images` have an xtask host
regression including exact image names, rejection before fallback, and the
ESP helper's standard-library boundary tests. `cargo xtest -p xtask`:
**36 PASS, 0 FAIL** in `/tmp/x86-windows-physical-fixture-final-host.log`;
`cargo fmt --check`, `git diff --check`, `bash -n` and `cargo xbuild x86` pass.
Read-only selection on the actual evaluation image returns byte offset
1,048,576 from GPT metadata. `/tmp/x86-windows-physical-fixture-build.log`.
No physical machine or OEM installation was touched.

Frozen `4b79eed`, `/tmp/x86-windows-physical-validation`, completed the
no-overlay matrix `/tmp/x86-windows-physical.Orhw0p`: **1 PASS, 1 FAIL**.
Both cases used `WINDOWS_DIRECT_MODE=physical-uefi WINDOWS_MEMORY=4G
WINDOWS_PCI_PROFILE=firmware-default`, fresh disposable image/vars/TPM state,
and the unchanged marker bounds (900 seconds normal, 600 seconds Hyper-V).

- `windows-test.sh monitor`: **PASS**, desktop keyboard/COM2 marker
  `thinhvwindowsdesktop`, mode/map gates and final poweroff. Automatic screen
  inspection confirms the desktop and shell. Pre-success counters: 456,927 L1
  exits, 419,150 external-interrupt exits, 9,478 interrupt-window exits,
  4,845,651 VMREADs, 1,975,946 VMWRITEs, no nested entries/failures.
- `windows-test.sh monitor-hyperv`: **FAIL**, marker timeout, not a monitor
  terminal fault. Final screen is black with the Windows progress spinner,
  not a BSOD. Counters: 3,643,929 observed/reflected L2 entries, zero entry
  failures, 32,361,269 L1 exits, 515,451 external-interrupt exits, 302,311
  interrupt-window exits, 7,000 INVEPTs, 5,981 INVVPIDs, 49,146,628 VMPTRLDs,
  635,034,860 VMREADs, 147,327,850 VMWRITEs and 7,660,268 reflected field
  writes. Last sampled L1 exit is VMWRITE. Hyper-V/WSL2 remains unvalidated.

Both qcow2 checks, artifact hashes, seed metadata and variable-store comparisons
passed. `/tmp/x86-windows-physical-batch.log`; automatic snapshot directories
`direct-normal/monitor-pre-success-diagnostics.dgjesG` and
`direct-hyperv/monitor-hyperv-failure-diagnostics.cYX4iv`. This copied-ESP QEMU
test does not qualify a motherboard ESP, physical SMP or Windows activation.

## Bounded per-layer VM-exit reason profiling

`ExitCounterValues::count_reason` now records actual L1 and L2 hardware exit
reasons separately, once per exit, with saturating counters. Reasons 0..63
have individual bins; newer reasons share a bounded overflow bin. VM-entry
failures are excluded from these histograms and retain their separate counter.
Reflection does not double-count a hardware exit. No guest RIP, register, MSR
payload or firmware data is collected, and no formatted hot-path logging or
new lock/allocation is added. This is instrumentation, not a performance fix.

Diagnostics ABI v3 appends two 65-word arrays to the v2 prefix, for 1,216 bytes.
The existing CPU-owned record/seqlock remains authoritative. The decoder and
Windows capture accept only the exact v2/v3 version/size pairs (176/1,216
bytes), wholly inside the published monitor-owned allocation; mismatched,
truncated, torn and exhausted records fail closed. Existing saved v2 captures
still decode. Both layer histograms are exposed in bounded JSON.

Validation through `nix develop --accept-flake-config --command`:

- `cargo xtest -p x86_uefi_loader` **205 PASS**, `-p xtask` **36 PASS**,
  `-p x86_64_hal` **59 PASS**, `-p nested_vmx` **29 PASS**,
  `-p x86_guest_uefi_test` **10 PASS**: **339 host PASS, 0 FAIL**.
- `cargo xbuild x86`: PASS. `cargo xrun x86 --release`: **9 PASS, 0 FAIL**
  (7 QEMU/KVM, 2 QEMU/TCG).
- `cargo xrun x86 --nested --release`: **15 PASS, 5 FAIL / 20**; all
  **14 Direct cases PASS**. The same five outer-KVM instruction-contract
  differences documented above remain failures; overall command exits 1.
- `python3 scripts/x86_64/decode-vmx-diagnostics.py --self-test`:
  **11 PASS, 0 FAIL**, including legacy/new layout and ownership boundaries.
  `bash -n scripts/x86_64/windows/windows-test.sh` and `git diff --check`: PASS.

Logs: `/tmp/x86-exit-reasons-host.log`, `/tmp/x86-exit-reasons-regression.log`.
No physical machine was tested; outer-KVM is reference evidence only.

The frozen `f97d3b9` no-overlay Hyper-V run's early sample has 320,039
actual L2 entries, zero failed entries, 234,598 L2 RDMSR exits and 1,742,152
L1 VMREAD exits out of 2,875,100 total L1 exits. A bounded paused read of the
owned 1,216-byte record was followed by confirmed `VM status: running`;
this sampled run is diagnostic evidence, not a timing benchmark.
`/tmp/x86-reasons-hyperv-early.bin`, matrix
`/tmp/x86-exit-reasons-windows.F6kFDb`. Full run completion is recorded below
when available; an early progressing counter is not a Hyper-V PASS.

To identify which of those VMREADs actually require a hardware VMCS switch,
ABI v4 appends 16 saturating miss counters (total 1,344 bytes): 15 explicitly
selected exact field encodings and one `other` bucket. Counting occurs only
after both original-patch and exit-snapshot lookups miss, before selecting the
Direct VMCS. Writes, cache hits and L0's own VMREADs do not enter these bins;
field *contents* are never captured. High aliases/unknown fields are not
conflated with the selected full-width fields. Existing v2/v3 decoding stays
available, with the same complete-extent/version/sequence checks.

The same five package commands and debug/release/nested commands above were
rerun: **342 host PASS, 0 FAIL** (loader 208, xtask 36, HAL 59, nested 29,
guest 10), debug build PASS, standard QEMU **9 PASS, 0 FAIL**, release nested
**15 PASS, 5 FAIL / 20**, including **14 Direct PASS** and the unchanged five
reference failures. Python decoder: **12 PASS, 0 FAIL**; fmt/fmt-check,
bash syntax and diff checks PASS. Logs `/tmp/x86-vmread-fields-host.log` and
`/tmp/x86-vmread-fields-regression.log`. No architectural VMX operation,
capability, timeout or guest-state policy was changed by this profiling step.

`f97d3b9` completed **FAIL** at the unchanged 600-second Hyper-V marker bound.
The final serial transcript contains a second complete L0 boot/publication,
so an unexpected reboot occurred; its cause was not captured. The final
screen shows TianoCore and the Windows spinner, not a visible BSOD. The
decoder correctly rejected the ambiguous multi-boot publication instead of
reading an old monitor address. Only the earlier validated sample is evidence
for counters. Image integrity/artifact/seed checks passed. Final screenshot:
`/tmp/x86-exit-reasons-windows.F6kFDb/direct-hyperv/monitor-hyperv-failure-diagnostics.JEqQZ6/screen.ppm`.

The early `c44f6c1` sample has 636,510 L2 entries and zero entry failures.
Hardware-backed L1 VMREADs: RIP 597,844; RFLAGS 205,315; CS attributes 597,847;
interruptibility 669,161; other selected/overflow bins total 86,559. Thus
those four stopped-guest fields account for about 96% of actual read misses.
L2 RDMSR exits are 506,054; L1 VMREAD exits 3,437,637; VMPTRLDs 8,556,554.
The bounded pause/copy resumed successfully. Saved publication and 1,344-byte
record: `/tmp/x86-vmread-hyperv-early-serial.log` and
`/tmp/x86-vmread-hyperv-early.bin`. Full run is not yet a PASS.

## CPU-local immutable nested host-validation limits

`CpuMonitor` now owns the fixed host-validation CPUID/MSR limits captured by
`capture_host_validation_limits` on its physical CPU before the first entry.
Only static width, NX, LAM, SHSTK/IBT and VMX CR fixed-bit capabilities are
retained; no dynamic OSXSAVE/OSPKE/XCR0/XSS state is cached. Physical reset,
resume or CPU ownership transition requires rebuilding this monitor object.
`host_limits_for_efer` combines those limits with the same current-carrier
EFER value already read by `handle_l1_vmentry` for PAT/EFER inheritance.
This removes repeated static CPUID/four RDMSR operations and one duplicate
EFER VMREAD per nested entry without removing original-host validation,
changing exception priority or caching L1's LMA across entries.

The existing CPU-ownership host test now checks two distinct CPU capabilities
and repeated LME/LMA/NX mode transitions, proving that the immutable owner
values remain unchanged. All five package checks: **342 PASS, 0 FAIL**;
debug build PASS; standard QEMU **9 PASS, 0 FAIL**; release nested **15 PASS,
5 FAIL / 20**, all **14 Direct PASS** and the same five reference differences.
`/tmp/x86-host-limits-cache-regression.log`. Timed A/B and the original
timing-sensitive regressions are still required before quantifying speedup.

## Write-through stopped Direct guest-field snapshot

`c44f6c1` Hyper-V profiling completed **FAIL**, unchanged 600-second marker
timeout, no second L0 publication and no terminal monitor fault. Final screen
shows the Windows spinner on black. The verified ABI v4 final record contains
3,605,523 L2 entries/reflections, zero failed entries, 2,713,092 L2 RDMSR exits,
489,558 L2 WRMSR exits, 19,395,011 L1 VMREADs and 48,758,970 VMPTRLDs.
Actual L1 VMREAD misses for RIP/RFLAGS/CS attributes/interruptibility are
3,293,556 / 1,443,510 / 3,294,655 / 3,797,430 respectively. This confirms the
early sample's hotspot throughout the run. Snapshot directory:
`/tmp/x86-vmread-fields-windows.FpTEJP/direct-hyperv/monitor-hyperv-failure-diagnostics.mekD5l`;
batch `/tmp/x86-vmread-fields-windows-batch.log`. Image/artifact/seed checks
pass. This is still not a Hyper-V/WSL2 PASS.

`nested_vmx::exit_snapshot::ExitSnapshot` now captures those four measured
guest fields alongside the existing exit fields while the stopped hardware
Direct VMCS is already current. Hardware remains authoritative: every guest
VMWRITE still executes before `written` updates the exact-owner saved value.
Failed writes do not update it; 32-bit writes truncate exactly; reserved
aliases, other VMCS owners and VM_INSTRUCTION_ERROR are never synthesized.
The existing invalidation before entry, clear, switch and VMX lifetime
transitions also covers these fields, including immediate entry failures.
No write is deferred and no VMCS12/VMCS02 or guest-context switching is added.

The native MSR contract now proves all four fields' changed/restored values,
four rejected aliases, guest values retained after a failed host-state entry,
and a second VMCS resuming through NOP to VMCALL with a newly advanced RIP.
The existing runner and xtask transcript test require the added exact counters;
an older or incomplete PASS line cannot satisfy the new gate. The snapshot
unit tests cover complete capture failure, owner separation, high aliases,
successful/failed write-through, widths and new-exit replacement.

All five host package checks: **343 PASS, 0 FAIL** (nested 30, loader 208,
guest 10, HAL 59, xtask 36). Debug build PASS; standard QEMU **9 PASS, 0 FAIL**;
release nested **15 PASS, 5 FAIL / 20**, all **14 Direct PASS**. The new snapshot
contract itself also passes on the reference before its unchanged later MSR
failure. `/tmp/x86-idle-guest-snapshot-regression.log`. Timed A/B, the original
timing-sensitive tests and post-optimization Hyper-V remain to be run.

### Timed Direct A/B of the two measured optimizations

Frozen baseline `c44f6c1` and optimized `d031d65` (CPU-limit retention plus
write-through guest-field snapshot) used the same three existing immutable
`linux-kunit-ab-*.efi` guests. `run-uefi-smoke.sh` plus the existing strict
`run-linux-kunit-test.sh --check-log` validator ran each baseline/optimized
pair alternately three times, with unchanged q35-smoke-1g benchmark settings,
CPU arguments, guest workload and time limits. **18 PASS, 0 FAIL**; all EFI,
UKI and driver hashes remained unchanged. `/tmp/x86-idle-guest-ab-batch.log`,
individual logs `/tmp/x86-idle-guest-ab.l9bvXi`.

| Case | Baseline ticks/iteration (3 runs) | Optimized ticks/iteration (3 runs) | Median reduction |
| --- | --- | --- | ---: |
| CPUID | 344990, 341911, 345125 | 290541, 290869, 289582 | 15.78% |
| VMCALL | 1105674, 1098774, 1108646 | 906879, 917283, 923532 | 17.04% |
| PM timer IN | 413903, 412687, 416556 | 346647, 346723, 349701 | 16.23% |

These are upstream guest TSC ticks per iteration under QEMU/KVM, not a
physical-L0 cycle cost or an isolated CPU/frequency-controlled measurement.
Windows and other project QEMU tests had finished before this A/B. The
high-PCI default-layout regressions remain separate mandatory tests; this
historical benchmark profile does not replace them. The table measures both
optimizations together, not their individual contributions. No timeout increase
is an optimization result.

### Post-optimization complete finite matrix (`d031d65`)

The existing twelve-case VM-exit manifest passed on both backends: **24 PASS,
0 FAIL**. Commands: `LINUX_KUNIT_BACKEND={outer-kvm,direct-vmx}` and each
`LINUX_KUNIT_CASE=vmexit_*` through `run-linux-kunit-test.sh`, using the same
pinned upstream test binaries and L2 QEMU. The subsequent Direct 4,096-cycle
lifecycle run, `LINUX_KVM_MEMORY=12G LINUX_KVM_HOST_XSTATE_TEST=1`, still
**FAILs at the unchanged default 300-second bound**: 3,931 fully checked cycles
at guest time 299.313484, versus the earlier 3,487. No individual state check
failed, but the required final 4,096-cycle completion marker is absent.
Combined matrix: **24 PASS, 1 FAIL / 25**.
`/tmp/x86-idle-full-vmexit-batch.log`, `/tmp/x86-idle-full-vmexit.N40qKb`.

Original Linux selftests plus S3: **5 PASS, 3 FAIL / 8**. PASS:
`xcr0_cpuid_test`, `dirty_log_page_splitting_test`, `nx_huge_pages_test`,
`memslot_modification_stress_test`, `kvm_page_table_test`. FAIL:
`memslot_perf_test` exits 142 on its original alarm;
`vmx_exception_with_invalid_guest_state` exits 137 at its original 600-second
bound; S3 resumes but fresh KVM creation hits an invalid-opcode Oops in
`alloc_loaded_vmcs`, following a `kvm_resume` warning. No S3 lifecycle fix or
weakened capability check is present. No debugger pause was used on the
timing-sensitive selftests. `/tmp/x86-idle-guest-linux-batch.log`, individual
logs `/tmp/x86-idle-guest-linux.g5q759`.

No-overlay Windows, default high-PCI layout, 4 GiB and one CPU: **1 PASS,
1 FAIL / 2**. Normal Windows boots to the required COM2 desktop marker and
powers off (900-second bound). Hyper-V still misses its 600-second marker;
the live screen shows **Please wait**, not a captured bugcheck. The final
validated v4 record has 4,052,723 L2 entries/reflections, zero entry failures,
27,686,534 VMPTRLDs, 699,222,778 VMREADs and 161,456,441 VMWRITEs. This is
6.83 VMPTRLDs per L2 entry versus 13.52 for the prior `c44f6c1` run, but it is
not a Hyper-V/WSL2 PASS. Guest RIP/RFLAGS/CS-attribute/interruptibility hardware
misses drop to 1,245 / 37,885 / 0 / 29,778. A brief diagnostic pause makes this
Windows run unsuitable as an isolated timing benchmark. Image checks,
artifact hashes and original seed comparisons pass.
`/tmp/x86-idle-guest-windows-batch.log`, matrix
`/tmp/x86-idle-guest-windows.vzVZBZ`, Hyper-V final counters under
`direct-hyperv/monitor-hyperv-failure-diagnostics.w30K4B`.

All of these are **QEMU/KVM** results (outer-KVM is reference evidence only),
not physical-machine qualification. The 300-second lifecycle FAIL and the
original timing failures remain recorded even when a longer historical
comparison bound is used separately to complete state-lifetime coverage.

Frozen `d031d65` also completed a separate 4,096-cycle state-lifetime test:
**PASS** at guest time 315.761469 with `LINUX_KVM_TIMEOUT_SECONDS=600`, the
existing historical comparison bound. `/tmp/x86-idle-guest-4096-historical-bound.log`.
This run overlapped host compilation and is not a controlled timing comparison.
The unchanged default 300-second test above remains **FAIL**.

## Idempotent stopped guest-field VMWRITE

`ExitSnapshot::write_is_redundant` recognizes only the same owner's four
retained, mandatory writable guest fields and their exact width-truncated
hardware values. `handle_l1_vmcs_access` performs its existing VMX/CPL/current
pointer and full memory-source checks before this decision. A same-value write
needs no hardware mutation or VMCS switch; changed writes still reach hardware
immediately, and successful completion still updates flags without clearing
VM_INSTRUCTION_ERROR. Reserved/high aliases and read-only fields never use the
shortcut. The Intel SDM VMWRITE operation checks encoding/access, not entry
validity of a field's value. No new VMCS composition, capability or deferred
state is introduced. The existing measured fields and invalidation lifetimes
are unchanged.

Native `msr_contract`/`l1_memory::repeat_vmwrite` now check four register and
four memory same-value writes, 32-bit truncation, retained error 12, four #PF
and four #GP source faults followed by continued execution, and the unchanged
owner/failed-entry/VMRESUME tests. Seven disposable fixture pages keep fault
scratch disjoint from the stopped L2's CR3. Runner/xtask gates require the new
exact coverage counts; reduced or missing evidence cannot pass.

The first test run caught the shared `l1_fault::with_handler` assuming
IDTR.limit <= 4095. VM exit architecturally loads 0xffff. The helper now copies
at most the 256 addressable IDT gates, uses that bounded temporary limit, and
restores the exact original limit on every ordinary return. It never reads an
extra 60 KiB merely because hardware loaded 0xffff. Initial regression log:
`/tmp/x86-repeat-vmwrite-regression.log`; the corrected native test passes on
both Direct and reference (the reference's later existing MSR failure remains).

Corrected validation: `cargo xtest -p nested_vmx`, `-p x86_uefi_loader`,
`-p x86_guest_uefi_test`, `-p x86_64_hal`, `-p xtask`: **344 PASS, 0 FAIL**
(31 + 208 + 10 + 59 + 36). `cargo xbuild x86` PASS; standard release QEMU
**9 PASS, 0 FAIL** (7 KVM, 2 TCG). Release nested suite **15 PASS, 5 FAIL / 20**,
all **14 Direct PASS**, including all six Linux modes/layouts and the separate
physical-SMP rejection fixture (not SMP support). The five reference failures
are unchanged. `/tmp/x86-repeat-vmwrite-regression-fixed.log`.
`cargo fmt --check`, shell syntax and `git diff --check` PASS in
`/tmp/x86-repeat-vmwrite-format.log`. No AArch64 implementation file changed.

The same immutable-UKI, alternating three-pair A/B against `d031d65` passed
**18/18** with unchanged artifact hashes and no other project QEMU running.
Baseline/optimized ticks per iteration:

* CPUID: 290702/292468, 289640/291818, 289269/289412.
* VMCALL: 911318/906674, 914843/929045, 918537/907123.
* PM timer IN: 340981/342192, 354459/342171, 342269/349495.

Median changes are +0.75%, -0.84%, -0.02% respectively: **no demonstrated
Linux benchmark speedup** from this additional optimization. Windows's actual
VMCS-operation counts still need remeasurement before claiming usefulness for
Hyper-V. `/tmp/x86-repeat-vmwrite-ab-batch.log`, `/tmp/x86-idle-guest-ab.1SxLMq`.

Post-commit `0505598` original Linux/S3 matrix remains **5 PASS, 3 FAIL / 8**,
with the same five passing cases and the same memslot alarm, invalid-guest-state
600-second bound, and post-S3 KVM failure. `/tmp/x86-repeat-vmwrite-linux-batch.log`,
`/tmp/x86-idle-guest-linux.oecdZW`. No timing-sensitive selftest was paused.
Frozen no-overlay Windows normal boot **PASS**; the paired Hyper-V run failed
in **test setup**, before QEMU launch, because whole-disk qcow2-to-raw conversion
exhausted the approximately 32 GiB free space. Its temporary conversion was
removed by the existing trap. This is not a Direct runtime failure or Hyper-V
result. Both seed and EFI comparisons pass.
`/tmp/x86-repeat-vmwrite-windows-batch.log`, `/tmp/x86-idle-guest-windows.ijwHQq`.

## Bounded Windows test ESP extraction

`windows-test.sh::copy_windows_test_esp` no longer converts the entire 80 GiB
evaluation disk to obtain one ESP. For qcow2 it reads only the first/last 1 MiB
GPT windows into a disposable sparse metadata image, uses the existing sfdisk
validation and unique-ESP parser, then reads the exact validated ESP extent.
Both GPT windows and the ESP require exact output sizes. This retains the
Hyper-V overlay's own original BCD and changes no source/backing image. Raw
test-image reading is unchanged. Unsupported/ambiguous GPTs fail without a
different disk/backend fallback; temporary files are individually cleaned up.
The physical firmware chainloader itself is unaffected.

`windows-esp-offset.py::select_extent` reuses the existing overlap, bounds and
partition checks. `virtual_size` validates explicit qcow2 geometry, minimum
metadata window size, 512-byte alignment and signed-64-bit arithmetic bounds;
the original offset-only CLI remains compatible. Its existing xtask entry runs
both Python tests. Host validation **2 Python PASS, 36 xtask PASS**, shell syntax
PASS (`/tmp/x86-esp-bounded-host-fixed.log`).

Native exploration first rejected a truncated GPT image as intended; preserving
its virtual size in a sparse metadata file permits read-only GPT validation.
The QEMU 10.1.5 [`img_dd` implementation](https://github.com/qemu/qemu/blob/v10.1.5/qemu-img.c)
applies the count boundary before skip, unlike dd(1). The runner therefore uses
the checked exclusive end for ESP count, omits count for the last GPT window,
and rejects any short or differently sized output. The actual fixture needs
2 MiB of metadata plus a 260 MiB ESP, not the complete Windows volume.
No new tool dependency, mount, NBD attachment, sudo, activation operation or
physical-disk access is introduced.

The corrected existing runner reaches Direct Hyper-V using bounded extraction:
ESP staging/cleanup **PASS**, runtime Hyper-V **FAIL** at its original
600-second marker bound. Final screen again shows **Please wait**; no bugcheck
was captured. EFI/script hashes, qcow2 checks and original seed comparisons
pass. `/tmp/x86-esp-bounded-hyperv-batch.log`,
`/tmp/x86-esp-bounded-hyperv.fc5R9c`. Native diagnostic snapshots are under
`direct-hyperv/monitor-hyperv-failure-diagnostics.VFEhvl`.

The validated `0505598` v4 record contains 4,244,881 L2 entries/reflections,
zero entry failures, 21,288,148 VMPTRLDs, 731,376,560 VMREADs and 164,809,381
VMWRITEs. VMPTRLDs per entry fall from 6.83 to **5.02** versus `d031d65`,
supporting the idempotent-write optimization's relevance to Hyper-V. This
does not demonstrate Windows readiness or a controlled wall-time speedup;
the run overlapped the separate VM-exit regression matrix. No live debugger
pause was used; final diagnostic capture occurs only after the deadline.

That complete twelve-case matrix separately passed **24/24** on reference and
Direct, `/tmp/x86-repeat-vmwrite-full-vmexit-batch.log`,
`/tmp/x86-idle-full-vmexit.6fYf1T`. Outer-KVM remains reference-only evidence.

The isolated `0505598` 4,096-cycle run still **FAILs** at the unchanged
300-second default: 3,931 checked cycles at guest time 299.270456, QEMU status
124 and no final completion marker. No other x86 QEMU was running at start,
and no concurrent project benchmark/Windows test or debugger pause was used.
`/tmp/x86-repeat-vmwrite-4096-default.log`. Do not substitute the separate
historical 600-second state-lifetime run for this default-bound failure.

The separate frozen `0505598` 600-second state-lifetime comparison completes
**PASS**, 4,096 cycles at guest time 318.407711, followed by normal poweroff.
`/tmp/x86-repeat-vmwrite-4096-historical-bound.log`. This run overlapped host
compilation/regression work and is not a controlled speedup measurement.

## Keep an empty carrier MSR load disarmed

All carrier VM_ENTRY_MSR_LOAD_COUNT writers were traced: initialization sets
zero; `reflect_l2_vmexit` arms a nonempty per-CPU host mirror; the first
subsequent carrier exit clears it in `complete_reflected_msr_load`, before L1
dispatch can attempt another nested entry. `prepare_host_load` rejects any
outstanding owner/count even when the requested list is empty. An empty host
reflection therefore no longer redundantly writes zero count/address. The
address is architecturally ignored at count zero. The separate Direct L2
entry mirror is unchanged, as are nonempty ordering, failure and abort paths.

The existing CPU-state host test now checks pending-owner/count combinations,
unchanged rejection, repeated empty preparation, retained payload and another
CPU's independent state. Native MSR tests cover alternating nonempty/empty
host loads and actual reflected execution, not merely metadata assertions.
All five package checks **344 PASS, 0 FAIL**, debug build PASS, standard QEMU
**9 PASS, 0 FAIL** (7 KVM, 2 TCG), release nested **15 PASS, 5 FAIL / 20**;
all **14 Direct PASS**, the same five reference differences.
`/tmp/x86-empty-host-msr-regression.log`. No new cached field, allocation,
global lock, capability or AArch64 implementation change is introduced.

Alternating three-pair A/B against frozen `0505598`, unchanged immutable UKIs
and benchmark settings: **18 PASS, 0 FAIL**, artifact hashes unchanged. No
other project QEMU was running during this A/B. Baseline/optimized ticks:

* CPUID: 289850/287177, 289762/281951, 288409/283460; median **-2.17%**.
* VMCALL: 905245/893746, 908366/897678, 908806/894820; median **-1.49%**.
* PM timer IN: 343328/337288, 343461/338913, 342879/335442; median **-1.76%**.

These small measured reductions are QEMU/KVM guest TSC results, not a
physical-L0 performance claim. `/tmp/x86-empty-host-msr-ab-batch.log`,
`/tmp/x86-idle-guest-ab.LflVd8`. Original-bound timing regressions remain
required; no timeout change is part of the implementation.

The frozen `05fa76d` follow-up still **FAILs** the unchanged 300-second,
4,096-cycle default: 3,946 checked cycles at guest time 299.319313, QEMU
status 124, no final completion marker. It ran without another project QEMU
or heavy compilation. `/tmp/x86-empty-host-msr-4096-default.log`.
The seven original Linux selftests plus S3 again give **5 PASS, 3 FAIL**:
memslot_perf exits 142, invalid-guest-state exits 137 at its existing 600-second
bound, and S3 loses VMX interception/produces a post-resume KVM Oops.
`/tmp/x86-empty-host-msr-linux-batch.log`,
`/tmp/x86-idle-guest-linux.pL2lBX`. No original timer or capability check was
weakened; later functional tests overlapped the separate Windows fixtures.

Windows on frozen `05fa76d`, no overlay and default q35 PCI layout:
**normal boot PASS, Direct Hyper-V FAIL** at its unchanged 600-second bound.
The final screenshot was inspected and shows **Please wait**, not a proven
watchdog bugcheck. No live debugger pause was used. Source/EFI hashes, both
qcow2 checks and original seed comparisons pass.
`/tmp/x86-empty-host-msr-windows-batch.log`,
`/tmp/x86-idle-guest-windows.QtKRzL`. The validated v4 record in
`direct-hyperv/monitor-hyperv-failure-diagnostics.XyGGwe` contains 4,161,888
L2 entries/reflections, zero entry failures, 20,919,177 VMPTRLDs,
717,326,806 VMREADs and 153,511,535 VMWRITEs. This is functional/counter
evidence, not a controlled wall-time comparison or a Hyper-V/WSL2 PASS.

The original twelve VM-exit cases also complete **24 PASS, 0 FAIL** on frozen
`05fa76d` (12 Direct, 12 reference).
`/tmp/x86-empty-host-msr-full-vmexit-batch.log`. The ELF/UKI cases and original
per-case timers were unchanged.

## Share checked firmware image ownership before AP handoff work

Inspection confirmed that Direct still duplicated weaker image-loading helpers:
successful null interfaces/handles, unvalidated returned device-node length,
unchecked handle-array length, ignored pool/image cleanup errors, leaked
StartImage ExitData, and an unconditional second UnloadImage after a returning
runtime driver. A boot application installed as MONITORX64.EFI could recursively
load another application rather than being rejected before handoff.

`vmx_smoke` now reuses `chainload`'s checked path/protocol/loading helpers.
The existing reference-only filesystem enumerator is shared with the research
Direct backend; it is compiled out of the physical backend, whose deterministic
current-ESP selection is unchanged. `unload_image` propagates firmware failures.
No new dependency, allocator or firmware hook is introduced.

`start_runtime_monitor` checks both runtime code/data memory types before
publishing its scoped handoff. On a returned StartImage it queries a fresh
LoadedImage protocol: a returning error driver has already been unloaded,
whereas a rejected-before-entry or successfully returning driver can remain
registered. Only a newly confirmed live image is unloaded, and any retained
load options are restored first. The still-unstarted guest is cleaned up even
if monitor cleanup fails. An unexpected success return cannot claim resident
Direct VMX. Both monitor and guest StartImage calls use the existing ExitData
cleanup. This follows the [UEFI image-service lifecycle](https://uefi.org/specs/UEFI/2.10_A/07_Services_Boot_Services.html#image-services).

The existing QEMU runner/xtask now includes two separately named ownership
negatives: application-as-runtime and research-loader/physical-runtime mix.
Their strict transcript gate requires complete ordered rejection and successful
retirement of both images, rejects normal guest execution, extra VMX/overlay
events, wrong errors, mixed backends, partial retries, NULs and timeouts.
They are not alternative Direct boot successes. Pure loader tests check all
code/data type combinations; xtask checks malformed negative transcripts.
The two initial ordinary-runner invocations correctly returned FAIL rather
than accepting these expected negatives as ordinary boot PASS.

An exploratory EPT-disabled invocation did not reach complete return/cleanup
evidence inside the ordinary runner's 10-second bound; it is **unverified**,
not counted as a successful lifecycle test. The deterministic cross-mode
fixture exercises an actual returning driver without relying on that timeout.
`/tmp/x86-runtime-returned-error.log` retains this exploratory result.

AP startup, physical SMP, root NMI forwarding and S3 VMX reconstruction remain
unimplemented. This firmware ownership change does not remove the physical
single-CPU gate or qualify an OEM Windows installation. No physical machine
was tested; outer-KVM evidence remains reference-only.

Firmware ownership validation commands (Nix development environment):

```sh
cargo xtest -p x86_uefi_loader
cargo xtest -p xtask
cargo xtest -p nested_vmx
cargo xtest -p x86_64_hal
cargo xtest -p x86_guest_uefi_test
cargo xbuild x86
cargo xrun x86 --release
cargo xrun x86 --nested --release
cargo fmt
cargo fmt --check
```

**348 host PASS, 0 FAIL** (211 loader across six existing feature entries,
37 xtask, 31 nested_vmx, 59 HAL, 10 guest); debug/release builds PASS.
Standard QEMU **11 PASS, 0 FAIL** (9 KVM including two ownership negatives,
2 TCG); nested **15 PASS, 5 FAIL / 20**, all **14 Direct PASS**, unchanged
five reference differences. `/tmp/x86-runtime-ownership-regression.log`.
Shell syntax and diff whitespace checks PASS. The changes are confined to
x86 loader/runner/xtask and this evidence; no AArch64 implementation changed.
Windows/full upstream timing/S3 results above identify the separately frozen
`05fa76d` build; they are not silently attributed to the firmware ownership
change, which does not modify VM-exit semantics or implement AP/S3 support.

### Frozen firmware-ownership Linux and Windows follow-up

Frozen `f01e1a5` repeats the original seven Linux cases plus S3 with
**5 PASS, 3 FAIL**. The failures remain memslot_perf (142), invalid guest state
(137 at the original 600-second guest bound), and post-S3 loss of interception
with a KVM Oops. `/tmp/x86-runtime-ownership-linux-batch.log`,
`/tmp/x86-idle-guest-linux.Jm7ec4`.

The first concurrent Windows batch (`/tmp/x86-idle-guest-windows.5MQYq1`)
returned normal=0/Hyper-V=1, but its source-artifact check detected two changed
EFI files while the separate Linux runner rebuilt that worktree. It is **not
accepted as an artifact-stable Windows result**. No seed was intentionally
modified; this batch stopped before its final seed comparison. The frozen
worktree must not be rebuilt while its Windows validation is running.

A fresh sequential replay with no concurrent build in that worktree gives
**normal Windows PASS, Direct Hyper-V FAIL** at its unchanged 600-second
bound. All four source/EFI hashes, both qcow2 checks and before/after seed
comparisons pass. `/tmp/x86-runtime-ownership-windows-stable-batch.log`,
`/tmp/x86-idle-guest-windows.869Vbc`. The final Hyper-V screen was inspected:
black background with a boot spinner, not a visible watchdog bugcheck or the
earlier Please wait screen. Serial contains two complete firmware/Direct boot
sequences. The diagnostics parser correctly rejects the repeated backend
markers; no counter record from an ambiguous pre-reset lifetime was read.
The cause of this reboot/stall is unresolved. This is not Hyper-V/WSL2 PASS.

### Make Windows Hyper-V CPU-count comparisons explicit

Inspection confirmed that `windows-test.sh::run_windows` ignored
`WINDOWS_SMP` in Hyper-V modes: Direct always used one CPU, while the reference
always used two. Previous two-CPU reference success was not a CPU-count-matched
control for Direct. The shared `windows_smp` selection now allows an explicit
one- or two-CPU Hyper-V reference, retains defaults, and rejects unsupported
overrides before compilation or disk/firmware operations. Direct remains
strictly one CPU; WSL/S4/soak retain their qualified two-CPU configuration.
Startup logs record the selected L1 CPU count. This does not implement SMP.

The existing xtask host tests exercise every mode, malformed values, explicit
overrides and early rejection in the live runner: **38 PASS, 0 FAIL**.
`nix develop --accept-flake-config --command cargo xtest -p xtask`,
`/tmp/x86-windows-cpu-policy-host.log`; shell syntax, formatting and diff
whitespace checks PASS.

Actual frozen `f86c587` reference runs now complete **2 PASS, 0 FAIL**:
one and two L1 CPUs both reach `thin-hv: windows hyperv PASS` and clean
poweroff. They use separate disposable overlays from the same existing
evaluation seed, `WINDOWS_MEMORY=4G`, `WINDOWS_PCI_PROFILE=firmware-default`,
`WINDOWS_CPU=host,+vmx,-hypervisor,kvm=off`, and the original 600-second bound:

```sh
WINDOWS_SMP=1 scripts/x86_64/windows/windows-test.sh trusted-kvm-hyperv
WINDOWS_SMP=2 scripts/x86_64/windows/windows-test.sh trusted-kvm-hyperv
```

Both commands ran in the Nix environment with distinct `WINDOWS_TEST_DIR`
directories; `/tmp/x86-windows-cpu-reference.sh` records the exact invocation.
`/tmp/x86-windows-cpu-reference-batch.log`,
`/tmp/x86-windows-cpu-reference.0CwOVG`. Source/EFI hashes, both qcow2 checks
and unchanged seed comparisons PASS. Thus one CPU alone does not prevent
this evaluation Windows Hyper-V workload from booting under outer KVM.
This does **not** explain the Direct failure or qualify Direct SMP/Hyper-V.
Direct's default outer CPU string omits `kvm=off`; the project hides KVM's
hypervisor CPUID leaves itself. No claim of a wholly identical backend or
controlled timing comparison is made.

All five required host package commands and `cargo xbuild x86` also pass at
`f86c587`: **349 host PASS, 0 FAIL** (211 loader, 38 xtask, 31 nested, 59 HAL,
10 guest). Formatting and diff whitespace checks PASS.
`/tmp/x86-cpu-policy-full-host.log`.

### Complete pinned Linux selftest inventory at frozen f01e1a5

Every one of the existing runner's 70 named ELF tests ran on both backends.
The wrapper only invokes `run-linux-selftest.sh` with its original per-case
arguments, timers and strict transcript gate; upstream ELF hashes match before
and after. No capability was enabled/hidden, no assertion weakened, and a
SKIP remains a nonzero runner result rather than PASS.

| QEMU/KVM backend | PASS | Actual FAIL | Upstream SKIP / non-PASS | Total |
| --- | ---: | ---: | ---: | ---: |
| Direct / project L0 | 56 | 2 | 12 | 70 |
| outer-KVM / reference | 60 | 0 | 10 | 70 |

Direct failures are `memslot_perf_test` (process exit 142) and
`vmx_exception_with_invalid_guest_state` (137 at the existing 600-second
guest bound). The same original ELF tests pass on the reference backend.
Direct-only SKIPs are `tsc_scaling_sync` (no VM TSC control capability) and
`rseq_test` (requires at least two L1 CPUs; no physical AP ownership exists).
The reference's existing rseq profile explicitly uses two L1 CPUs, so its
PASS is not Direct SMP evidence.

Ten SKIPs are shared: `monitor_mwait_test`, `amx_test`, `pmu_counters_test`,
`pmu_event_filter_test`, `xen_vmcall_test`, `xen_shinfo_test`,
`private_mem_kvm_exits_test`, `private_mem_conversions_test`,
`aperfmperf_test`, `kvm_buslock_test`. Each records upstream exit 4 and the
missing feature/capability. Among the actual Direct PASS cases are xcr0/CPUID,
debug/state, all existing Hyper-V enlightenment selftests, dirty-log large-page
splitting, NX huge pages, memslot modification, guest page-table, MMU stress,
and maximum-vCPU creation. These test an L1 KVM API, not Windows Hyper-V boot.

Exact existing-runner invocation for each backend/name:

```sh
LINUX_SELFTEST_BACKEND="$backend" LINUX_SELFTEST_NAME="$name" \
    LINUX_SELFTEST_ELF="$pinned_elf" scripts/x86_64/run-linux-selftest.sh
```

Nix orchestration and the full name/ELF list are in
`/tmp/x86-runtime-all-selftests.sh`; inputs are the previously pinned Linux
7.1.5 selftests under `/tmp/thin-hv-kvm-selftests-7.1.5.MUloxc/out`.
Logs: `/tmp/x86-runtime-all-selftests-direct-batch.log`,
`/tmp/x86-runtime-all-selftests-reference-batch.log`;
matrices `/tmp/x86-runtime-all-selftests-direct-vmx.RRAnYq` and
`/tmp/x86-runtime-all-selftests-outer-kvm.OXthNJ`.
Functional runs overlapped other finite QEMU tests (at most three host QEMUs),
so these are not isolated performance measurements. The separate 61-case KVM
unit matrices are still running and are not counted as completed here.

### Native RFLAGS completion regression before optimization

The native contract now tries all 64 combinations of CF/PF/AF/ZF/SF/OF for
each of VMsucceed, VMfailInvalid (no current VMCS), and VMfailValid (unsupported
field). It verifies the complete resulting PUSHFQ value and read destination,
then restores the fixture's original flags before returning to Rust. These
192 probes execute the project's actual L1 VMREAD interception, not merely
KVM's L2 CPUID path. The existing strict runner requires the complete ordered
RFLAGS marker; xtask rejects missing, duplicated, malformed and late markers.

Before changing monitor completion, `cargo xtest -p x86_guest_uefi_test`,
`cargo xtest -p xtask` give **48 host PASS, 0 FAIL**. The existing release
`cargo xrun x86 --nested --release` remains **15 PASS, 5 FAIL / 20**, all
**14 Direct PASS**, with the unchanged five reference differences described
above. `/tmp/x86-rflags-contract-baseline.log`. All commands use Nix. The
baseline monitor code is still `f86c587`; only the native fixture and gate
changed. Its two release EFI artifacts were retained outside Git in
`/tmp/x86-rflags-baseline-images` for a subsequent measured comparison.
