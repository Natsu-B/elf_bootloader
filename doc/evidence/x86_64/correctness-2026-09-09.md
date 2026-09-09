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
