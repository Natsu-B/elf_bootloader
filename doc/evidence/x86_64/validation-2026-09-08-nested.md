# Expanded QEMU nested validation, 2026-09-08

Branch: `feat/x86-thin-monitor`; starting commit: `24da228`. The requested branch,
short status and last 20 commits were recorded before edits. No branch switch,
Claude use, new dependency, AArch64 production change or physical-machine write.
The user's pre-existing `AGENTS.md` change is excluded from the implementation commits.
Production fix: `9d36bc9`; expanded test coverage: `92c13df`; APIC tests: `a709f73`.
Atomic lifecycle-record correction: `e40ffff`.

**Verdict: NO-GO for physical daily use.** The available finite QEMU matrix below
was executed, including substantial actual L2 execution. Remaining failures are
Direct Hyper-V, Direct S3, unrestricted high PCI MMIO, and reference long-soak final
shutdown. Unsupported/absent additional suites are listed explicitly, not counted
as passes. Physical qualification must wait for these and the platform/SMP work in
the [physical L0 plan](../../x86_64_physical_l0.md).

This is **QEMU evidence, not physical-hardware qualification**. All VMX/L2 execution
uses QEMU/KVM. TCG covers only the non-VMX firmware paths. The architecture remains
trusted Direct-VMCS; reference successes are never substituted for Direct failures.
Generic UEFI/Linux runners execute serially because they share ESP/vars/log paths.
At most two private Windows VMs run beside that one generic QEMU. New agent work
was stopped after the user's request to minimize subagents; existing work was handed off.

## Changes and coverage

* `nested_vmx::vmcs_revision_is_supported`, HAL `RecordedFailure::VmptrldIncorrectRevision`
  and monitor `handle_l1_vmptrld`/`publish_l1_instruction_error`: reject shadow VMCS headers
  when L1's advertised secondary capabilities prohibit shadowing. Address error 9 and
  VMXON-pointer error 10 retain precedence over revision error 11. Hardware error
  publication uses a disjoint runtime-owned invalid-revision page, including when
  physical VM-exit fields are read-only. No opaque VMCS writes or VMCS12/02 layer.
  The added page has allocation/physical-width/WB ownership checks. These are explicitly
  QEMU-smoke checks, not a replacement for platform-derived MTRR/EPT construction.
* `x86_guest_uefi_test::nested_contract`: four temporary firmware pages, two VMCSs,
  eight switching cycles; invalid VMXON revision/shadow headers; no-current operations;
  VMPTRLD shadow policy; failed entry errors 5/7 and restoration; all advertised INVEPT
  types 1/2 and INVVPID types 0..3; invalid descriptor boundaries; exact VMfail flags/errors.
  No successful L2 entry occurs in this fixture; Linux provides that evidence.
* `linux-l1-kvm-probe.c`: preserve the real-mode/SSE test, then run the same two VMs
  through 16 long-mode checkpoints. Two four-level roots with 4 KiB leaves exercise
  PTE replacement plus INVLPG, CR3 reload and root switching. Guest execution checks
  XMM0–15/MXCSR, EFER/PAT, disabled-debug DR0 and nondecreasing TSC. Per process:
  2 VMs, 144 KVM_RUN, 16 IN, 96 OUT, 32 HLT, 14 memslot remaps, 64 paging checks,
  16 INVLPG and 48 CR3 writes. All resources must be explicitly released successfully.
  PAT uses distinct unused entries: this is state preservation, not cache-type benchmarking.
* Existing Linux soak/reboot and S3 runners now select `direct-vmx` (one L1 CPU) or
  `outer-kvm` (two). Strict host gates require backend/topology, complete ordered coverage
  and shutdown. S3 creates fresh L2s before/after sleep, not live-L2 retention. Scratch
  `DriverFFFF` runtime-variable tests remain confined to disposable QEMU firmware stores.
* The S3 fixture sends its records through `/dev/kmsg` so PM messages cannot splice
  themselves into a tty marker. The gate removes only the kernel timestamp prefix;
  truncated, reordered and duplicate records still fail. Kernel diagnostics remain enabled.
  The lifecycle fixture uses the same approach for delayed TSC calibration messages.
  Each bounded record opens/closes the logging device; no persistent per-file rate-limit
  bucket silently drops later cycles. All 4096 ordered records remain mandatory.
* The shared runner now stops promptly on terminal monitor/kernel failure; it still
  rejects the run. An explicit `X86_UEFI_PCI_PROFILE=q35-smoke-1g` A/B fixture reuses
  the existing Windows runner's 1 GiB PCI aperture. Default is `firmware-default`;
  no automatic retry, fallback or claim of general MMIO support is made.
* `build-linux-uki.sh` accepts an optional bounded static non-PIE x86_64 upstream test
  ELF. `linux-l1-selftest-init` bounds execution to 120 seconds, invokes its absolute
  path and reports process status. `run-linux-selftest.sh` uses the existing UEFI runner
  and rejects wrong backend, partial TAP, skip, nonzero status and missing poweroff.
  New negative fixtures run in the existing `xtask` package entry in `xtest.txt`.

## Pinned upstream KVM tests

Unmodified Linux 7.1.5 source, commit
[`155b42bec9cbb6b8cdc47dd9bd09503a81fbe493`](https://github.com/gregkh/linux/tree/155b42bec9cbb6b8cdc47dd9bd09503a81fbe493/tools/testing/selftests/kvm).
Archive SHA256: `f734c376f0bb4ce8c4c607f59a9504e765a442a34db3b0a65c4441605aec4f96`.
The programs use Linux L1's KVM API to execute L2; `kvm_intel nested=0` prevents
advertising VMX to L2 and does not disable the L1-to-L2 nesting being tested.

| Program | Required evidence | outer-KVM/reference | Direct-VMX/project L0 |
| --- | --- | --- | --- |
| `tsc_msrs_test` | Exact TAP 1..5, stages 2..6, zero skips/failures, process exit 0, S5 | PASS | PASS |
| `userspace_msr_exit_test` | Exact TAP 1..4, all four named MSR cases and totals, exit 0, S5 | PASS | PASS |
| `cr4_cpuid_sync_test` | Upstream guest assertions/UCALL_DONE, process exit 0, S5 | PASS | PASS |
| `xcr0_cpuid_test` | Supported XCR0 and #GP for unsupported bits, UCALL_DONE, exit 0, S5 | PASS | PASS |
| `debug_regs` | INT3, DR0–3 instruction/data breakpoints, single-step/BLOCKIRQ, DR7.GD, UCALL_DONE, exit 0, S5 | PASS | PASS |
| `apic_bus_clock_test` | xAPIC/x2APIC timer divisors and configured bus-clock checks, default upstream 5% tolerance, exit 0, S5 | PASS | PASS |
| `xapic_tpr_test` | Actual self-IPIs, IF/TPR masking and delivery, EOI, TPR/PPR/CR8 consistency, both APIC modes, exit 0, S5 | PASS | PASS |

The last five programs are silent on success. Their `assertions=1` marker counts one
completed upstream program, not the number of internal assertions. Skip exits (4),
timeout/signal exits and TAP-shaped substitute output are rejected. These are not the
entire upstream KVM selftest collection, full XSAVE coverage or L2 SMP validation.

Reproduce using static ELFs built from the pinned source (not committed):

```sh
nix develop --accept-flake-config --command env \
  LINUX_SELFTEST_BACKEND=direct-vmx \
  LINUX_SELFTEST_NAME=xcr0_cpuid_test \
  LINUX_SELFTEST_ELF=/absolute/path/to/static/xcr0_cpuid_test \
  scripts/x86_64/run-linux-selftest.sh
```

Use `outer-kvm` for the control. The ELF checker is available without a VM as
`scripts/x86_64/run-linux-selftest.sh --check-elf ELF`, and the transcript gate as
`--check-log BACKEND TEST LOG`. Local static build artifacts and logs are under
`/tmp/thin-hv-kvm-selftests-7.1.5.MUloxc/`; no source patches were applied upstream.
Build with that source's `make ARCH=x86 headers`, then its KVM selftest Makefile,
`ARCH=x86 OUTPUT=<absolute-output> LDFLAGS='-static -no-pie -pthread -L<glibc-static>/lib'`.
The existing flake's `nixpkgs.legacyPackages.x86_64-linux.glibc.static` supplies libc.

The separate kvm-unit-tests VMX suite was investigated at commit
[`714bfd622b00cb5e8f2dad9e620641a0adb74ab7`](https://github.com/kvm-unit-tests/kvm-unit-tests/blob/714bfd622b00cb5e8f2dad9e620641a0adb74ab7/x86/Makefile.x86_64).
Upstream explicitly excludes `vmx` from EFI builds because its assembly is not PIC.
Its Multiboot payload cannot simply be passed to firmware `LoadImage`. It was **not run**;
an audited boot adapter respecting monitor-reserved memory is required. Nor were
KVM selftests requiring an additional nested L3, AMD SVM, confidential-VM hardware,
unsupported features or L2 SMP claimed as executed.

## Reproduced failures, not hidden by controls

1. **Fixed: shadow VMCS acceptance.** On the starting Direct EFI, the expanded native
   fixture reported `stage=vmcs-shadow actual=0x0 expected=0x2`; reference passed.
   Direct accepted a shadow VMCS despite masking the capability. After the fix, all
   four native capability profiles and both one-cycle Linux cases pass. Direct reports
   shadow=0, VMfailValid=26; reference reports shadow=1 and VMfailValid=24/25.
2. **Direct default PCI layout remains FAIL.** With virtio data disk and network,
   firmware assigned BAR4 at `0x380000000000`, `0x380000004000` and `0x380000008000`.
   Direct stopped before Linux with EPT violation reason `0x30`, qualification `0x181`,
   RIP `0x7ece0b8e`, RDX `0x380000009000`. The fixed 8 GiB carrier EPT does not cover
   those BARs. QEMU `info pci` confirms their addresses. The already-halted test was
   stopped explicitly; runner exit 2 is a failure, not a timeout/pass or fallback.
   The bounded-aperture comparison cannot qualify the platform-derived physical map.
3. **Direct S3 remains FAIL.** One sleep/wakeup completes, but KVM then reports VMCS
   capabilities changing from Direct's masked values to the outer CPU's values
   (`VMX_BASIC` high word `0x00981000` → `0x01d81000`, MISC `0x160` → `0x20000165`).
   KVM virtualization enable fails; the post-resume `alloc_loaded_vmcs` faults via
   `kvm_spurious_fault`, ending in `Kernel panic ... Attempted to kill init`.
   This is evidence that Direct interception/state is not preserved across S3,
   not a successful resume or merely a marker failure. No Hyper-V/S3 feature was hidden.
4. **Fixed test-output race, not a monitor fix.** The first reference S3 run reached
   three resumes and clean S5, but PM printk split `variable=DriverFFFF` in the transcript.
   Strict validation correctly rejected it. The atomic-record retest passes without
   disabling kernel logs or weakening the ordered coverage requirements.
   The initial bounded-aperture Direct reboot run also completed both boot epochs,
   workloads and S5 but was rejected because the new gate counted only one EFI
   image entry per boot. Direct has a loader and runtime entry. The gate and host
   fixture now require exactly four entries across two boots; a fresh full rerun passes.
5. **Direct Windows Hyper-V baseline remains FAIL.** A fresh pre-fix release case
   reached its 1200-second deadline with “Please wait”, not an observed BSOD.
   Counters: 73,686,212 L1/L0-only exits; 8,481,566 observed direct entries;
   8,481,565 reflections; 1,019,151 external-interrupt exits; 623,288 window exits;
   6,731 INVEPT; 6,024 INVVPID; zero recorded entry failures. These totals are
   diagnostics, not proof of correct interrupts, forward progress or a watchdog cause.

## Completed validation

| Evidence class | Check | Result |
| --- | --- | --- |
| Host | `cargo xtest -p nested_vmx` | PASS: 8 tests |
| Host | `cargo xtest -p x86_64_hal` | PASS: 40 tests |
| Host | `cargo xtest -p x86_uefi_loader` (all five configured feature rows) | PASS: 45 executions (11+11+5+7+11) |
| Host | `cargo xtest -p x86_guest_uefi_test` (all configured rows) | PASS: 8 executions (4+2+2) |
| Host | `cargo xtest -p xtask` | PASS: 28 tests, including all partial invalidation masks and negative transcript cases |
| Build/static | `cargo fmt --check`, `cargo xbuild x86`, shell syntax, diff check | PASS; no AArch64/Cargo dependency diff |
| QEMU/KVM | Release nested native/readonly A/B plus one Linux long64 cycle per backend | PASS: six cases, suite exit 0 |
| QEMU/KVM | Final atomic-record release suite, 4096 cycles per backend, explicit 600-second limit | PASS: all six cases together, suite exit 0; both guests complete clean S5 |
| QEMU/KVM | Seven pinned upstream selftests per backend | PASS: fourteen runs, each guest exit 0 and clean S5 |
| QEMU/KVM | Reference Linux default-aperture load/reboot | PASS: two boots, 1000 L2 probes and 80 hashes per boot, 128 MiB disk persistence, network, S5 |
| QEMU/KVM | Explicit q35-smoke-1g Linux load/reboot, each backend | PASS: same complete two-boot coverage and final runner exit 0 |
| QEMU/KVM | Reference Linux S3 | PASS: three sleeps/resumes, four two-VM probes, runtime checks, CPU1 re-online, S5 |
| QEMU/KVM | Direct Linux default PCI and S3 | FAIL, separately reproduced as described above |
| QEMU/KVM | Final-release Direct normal Windows | PASS: desktop/COM2/backend/fatal gates and QEMU exit 0; host-driven test termination, not guest S5/daily-use |
| QEMU/KVM | Final-release Direct Windows Hyper-V, 1200 seconds | FAIL: runner exit 1, “Please wait” screenshot, diagnostic counters saved, forced test cleanup, child qcow2 check clean |
| QEMU/KVM | Reference Windows Hyper-V and WSL2 | PASS: guest workload markers and runner exit 0; these modes do not persist numeric QEMU wait status or establish clean S5 |
| QEMU/KVM | Reference Windows S4 | PASS: S4 request/clean shutdown, cold start, exact guest_resume=1 and L2 verification, final S5/QEMU exit 0 |
| QEMU/KVM | Reference Windows 60-minute/two-round-target soak | FAIL overall, runner exit 1: workload completes 331 rounds, required reboot/persistence and zero BugCheck/WHEA/Hyper-V error counts, but no S5 within 120 seconds |
| QEMU TCG/KVM | Existing ordinary smoke, debug and release entrypoints | PASS: nine cases per entrypoint, both runner exits 0 |
| QEMU/KVM | Debug-entrypoint nested suite (native EFI debug; Linux runner release) | PASS: all six cases, one Linux cycle per backend, exit 0 |
| QEMU/KVM | Windows physical-status script SelfTest, snapshot-only evaluation guest | PASS: exact zero-query self-test evidence, runner exit 0 and clean S5; not physical activation-status evidence |
| Physical | Original Windows/Linux, activation status, devices, all CPUs and power | UNVERIFIED |

The host total is **129 passing Rust test executions**. Final-release normal Windows
and the final 1200-second Hyper-V case use the identical staged monitor SHA256
`afc952fde2c9d5aac58b2cea2e1cd2206226152468c2ceb5934fe431a58d6c04`.
An intermediate post-fix Hyper-V run hit the runner's 600-second default: its command
mistakenly used `HYPERV_TIMEOUT` rather than `WINDOWS_HYPERV_TIMEOUT_SECONDS`.
It is retained as a 600-second failure, not mislabeled as the 1200-second comparison.
Likewise, the first 4096-cycle Direct stress run reached cycle 3433 without a recorded
state mismatch but hit the default 300-second limit (QEMU status 124). The matching
reference completed 4096. That interrupted run remains a FAIL; the full suite is
repeated with an explicit bounded 600-second deadline, not a reduced cycle count.
The next Direct run completed all 4096 and S5, but its matching reference transcript
was rejected because the delayed TSC calibration message was spliced into cycle 86.
That failed suite is preserved too. Atomic kernel records address the output race;
the final all-cases-in-one-run result is recorded separately below.

The final atomic-record suite passes **all six cases in one run**, including all
4096 cycles on both backends. Per backend this is 8192 created/destroyed VMs,
589,824 KVM_RUN calls, 57,344 memslot remaps, 65,536 long64 checkpoints,
262,144 paging checks, 65,536 INVLPG and 196,608 CR3 writes. No partial transcript,
aborted workload, reference substitution or forced timeout counts as this PASS.

The full reference Windows soak is **not a clean-shutdown PASS**. Its last saved
screen reads “You're 20% there. Please keep your computer on.” This is consistent
with Windows update/servicing during shutdown, not an observed BSOD; no servicing
event-log diagnosis was collected, so the screen alone is not a proven root cause.
The existing runner's fixed 120-second poweroff bound expires and its cleanup stops
the disposable VM. The child qcow2 check is clean, but that does not certify the
guest filesystem/update state after forced termination. The cutoff is not relaxed,
updates are not disabled and no full-soak success is inferred from shorter tests.

Final Direct Hyper-V counters (same release EFI as the desktop PASS):
72,145,810 L1 exits; 72,145,809 L0-only handled exits; 8,300,904 direct attempts,
observed entries and reflections; 1,009,295 external-interrupt exits; 605,438 window
exits; 6,834 INVEPT; 6,072 INVVPID; zero recorded nested-entry failures. Last reason
23, phase L1-exit. A nonzero instruction/exit count is not usable Hyper-V or WSL2.

## Commands and evidence

```sh
nix develop --accept-flake-config --command env LINUX_KVM_CYCLES=4096 LINUX_KVM_TIMEOUT_SECONDS=600 cargo xrun x86 --nested --release
nix develop --accept-flake-config --command cargo xrun x86
nix develop --accept-flake-config --command cargo xrun x86 --release
nix develop --accept-flake-config --command env LINUX_SOAK_BACKEND=direct-vmx scripts/x86_64/run-linux-soak-test.sh
nix develop --accept-flake-config --command env LINUX_SOAK_BACKEND=direct-vmx LINUX_SOAK_PCI_PROFILE=q35-smoke-1g scripts/x86_64/run-linux-soak-test.sh
nix develop --accept-flake-config --command env LINUX_SUSPEND_BACKEND=direct-vmx scripts/x86_64/run-linux-suspend-test.sh
```

Use `outer-kvm` for each Linux control; no backend is inferred from a successful marker.
For Windows, use fresh child qcow2 images, copied OVMF/TPM state and a unique VNC port:

```sh
nix develop --accept-flake-config --command env \
  WINDOWS_TEST_DIR=/absolute/private/test-directory WINDOWS_VNC=127.0.0.1:31 \
  WINDOWS_MEMORY=4G WINDOWS_HYPERV_TIMEOUT_SECONDS=1200 \
  scripts/x86_64/windows/windows-test.sh monitor-hyperv
```

Other exercised modes: `monitor`, `trusted-kvm-hyperv`, `trusted-kvm-wsl`,
`trusted-kvm-s4`, `trusted-kvm-wsl-soak`. The Windows seed/raw images, original vars,
TPM and installation are not test write targets. Generated EFI, UKI, qcow2, logs,
TPM state, screenshots and dumps are deliberately excluded from Git.
Final read-only checks confirmed the recorded seed qcow2/raw sizes and mtime/ctime,
plus the recorded firmware-variable/ready-file hashes, were unchanged. TPM directories
were copied privately but were not included in the pre-run hash manifest; no full
TPM-state hash comparison is claimed. Completed child-image checks reported no qcow2 errors.

Local evidence (generated, not repository fixtures):

* `/tmp/x86-nested-all-first-20260908.log`: shadow failure and initial long64 passes.
* `/tmp/x86-nested-all-postfix-20260908.log`: all six post-fix nested cases, exit 0.
* `/tmp/x86-selftest-{outer-kvm,direct-vmx}-<program>-20260908.log`: fourteen upstream runs.
* `/tmp/x86-nested-4096-final-20260908.log`: reference complete, Direct 300-second timeout.
* `/tmp/x86-nested-4096-600s-final-20260908.log`: explicit longer bounded rerun.
* `/tmp/x86-nested-4096-atomic-final-20260908.log`: atomic-record host tests and complete suite rerun.
* `/tmp/x86-ordinary-{debug,release}-final-20260908.log`: both ordinary matrices.
* `/tmp/x86-nested-debug-final-20260908.log`: debug-entrypoint nested matrix.
* `/tmp/x86-nested-all-final-host-build-20260908.log`: 129 host tests, formatting and x86 build.
* `/tmp/x86-apic-gate-host-20260908.log`: final APIC allowlist host gate retest.
* `/tmp/x86-power-matrix-20260908.log`: default-layout power matrix, including failures.
* `/tmp/x86-power-soak-direct-vmx-pci-failure-20260908.log`: QEMU PCI BAR evidence.
* `/tmp/x86-power-s3-direct-vmx-20260908.log`: masked/raw capability mismatch and panic.
* `/tmp/x86-power-s3-outer-kvm-atomic-20260908.log`: complete atomic S3 retest.
* `/tmp/x86-windows-matrix.hD1LuX/`: private Windows case commands/statuses, hashes,
  screenshots/counters, seed-integrity checks and qcow2 checks.
* `/tmp/x86-windows-direct-final-1200-20260908.log`: final Direct Hyper-V failure;
  image/counters in `direct-final-hyperv-1200/monitor-hyperv-failure-diagnostics.31UPPR/`.
* `/tmp/x86-windows-physical-status-selftest-20260908.log`: zero-query SelfTest and exit 0.
* `reference/trusted-kvm-wsl-soak.{exit,finished,qcow-check}` under the matrix directory:
  full-soak failure status and image check; `qemu-monitor-trusted-kvm-wsl-soak.in.latest.ppm.png`
  shows the shutdown-time servicing screen.
* `/tmp/x86-nested-final-format-build-20260908.log`: final format check, x86 build and `cargo fmt`.

Physical Windows activation **status**, original hardware/firmware identity, real TPM and
Secure Boot, device/DMA behavior, all-CPU virtualization, S3/S4 and daily-use reliability
remain **UNVERIFIED**. No product key was read, printed, synthesized or persisted;
no activation data, BCD, physical partitions, Secure Boot keys or TPM ownership changed.
