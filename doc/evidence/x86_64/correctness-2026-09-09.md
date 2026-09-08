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
