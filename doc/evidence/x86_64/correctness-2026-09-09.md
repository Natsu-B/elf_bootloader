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
