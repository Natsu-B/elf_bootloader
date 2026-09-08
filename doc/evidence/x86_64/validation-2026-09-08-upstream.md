# Extended real-L2 QEMU matrix (completed 2026-09-09)

Branch: `feat/x86-thin-monitor`, starting commit `24f0243`; initial test-tooling
commit `f75a815`, prerequisite/gate follow-up `f4211fb`, bounded stress profile
`3345224`. This continues the
[earlier nested/power validation](validation-2026-09-08-nested.md), whose production
VMX implementation is unchanged. The pre-existing user edit to `AGENTS.md` is not
part of these commits. No branch switch, Claude call, new subagent, new crate,
AArch64 production edit, firmware identity change, or physical-machine test.

**Physical daily-use verdict remains NO-GO.** This report distinguishes actual L2
execution from firmware/unit fixtures and from the outer-KVM reference backend.
Direct Hyper-V boot, Direct L1 S3, general high PCI MMIO and physical SMP/platform
ownership remain unresolved; reference Windows success does not resolve them.

All selected QEMU checks and follow-up diagnostics are complete. This is **not an
all-PASS result**: reproducible failures, intermittent failures and unavailable
capabilities are recorded separately below. No QEMU test remains running.

## Implementation changes

The existing UKI/UEFI runner and `xtask` framework are reused; no parallel test
framework, dependency or production hypervisor architecture is introduced.

* `scripts/x86_64/build-linux-uki.sh`: package the explicitly selected L2 QEMU's
  loader, libraries and ROMs plus the two new disposable guest workloads.
* `run-linux-l2-os-test.sh`, `linux-l1-l2-os-init`, `linux-l2-os-init`: actual L2
  OS boot, memory, disk, network, persistence and clean shutdown gates.
* `run-linux-kunit-test.sh`, `linux-l1-kunit-init`, `linux-l2-kunit-cases.txt`,
  `check-kunit-case.awk`: pinned upstream case profiles, isolated prerequisites,
  bounded deadlines and strict per-case/whole-matrix results.
* `run-linux-selftest.sh`, `linux-l1-selftest-init`: expanded 70-program allowlist,
  bounded resources, disposable-L1 prerequisites and complete workload checks.
* `scripts/x86_64/windows/windows-test.sh`: bounded final poweroff budget; a
  forced termination cannot pass the Windows soak shutdown gate.
* `xtask/src/main.rs`: extend existing transcript fixtures and add
  `linux_kunit_gates_require_real_results_and_complete_ordered_matrix`,
  `linux_l2_os_gate_requires_six_complete_kvm_boots` and
  `windows_poweroff_timeout_is_bounded_and_decimal` host tests.

The unqualified script names above are under `scripts/x86_64/`. Changes to the
physical-L0 guide and previous evidence page link this report without rewriting
their historical test outcomes. Generated artifacts and diagnostic upstream
source copies are not committed. AArch64 implementation paths are unchanged.

## Environment and scope

* Development host: Intel Core Ultra 9 185H, Linux 7.1.5; host KVM PMU parameter
  `enable_pmu=N`. Host module parameters are not changed for these tests.
* VMX execution uses QEMU/KVM, never TCG fallback. TCG cannot establish Intel VMX
  nesting; its firmware-path regression evidence remains in the earlier report.
* Direct and reference L1 Linux normally have one CPU. Reference-only rseq uses
  two L1 CPUs for actual scheduler migration. Direct does not claim L1 SMP.
  L2 programs can create multiple vCPUs on the one L1 CPU; this is deliberately
  oversubscribed and is not proof that physical Windows-visible APs are virtualized.
* Generic UEFI/Linux runners are serialized: their ESP, variable store and serial
  paths are shared. A private Windows VM may run beside a Linux L1 and its single
  QEMU L2 child. Total concurrently running QEMU processes never exceeds three.
* Linux L1/L2 kernel SHA256:
  `e7d8f6e0478f9a2d161a8cf77cae9e090739cca63916ada8541efb699207f278`.
* Packaged headless L2 QEMU 10.1.5 SHA256:
  `41d7c59b2b00eb414e9377d2d6c653f868fb67853b69e4692f5e9240a98382d4`.
  Its absolute resolved ELF interpreter/DT_NEEDED library paths and required ROMs
  are packaged into the disposable initramfs. This explicitly selected headless
  binary avoids desktop-plugin runtime dependencies; it is not a backend fallback.
* Release project monitor SHA256 remains
  `afc952fde2c9d5aac58b2cea2e1cd2206226152468c2ceb5934fe431a58d6c04`,
  identical to the earlier Direct Windows/nested regression build. This work
  changes test tooling and budgets, not production VMX behavior.
* All test images, disks, swap images, vars, TPM state, serial logs and screenshots
  are generated evidence outside Git. No Windows product key is read or saved.

## Actual Linux OS as L2

`run-linux-l2-os-test.sh`, `linux-l1-l2-os-init` and `linux-l2-os-init` run six
independent real Linux boots per backend, alternating one and two L2 vCPUs three
times. Every child explicitly uses `-accel kvm`, boots a real Linux kernel and
must power off with QEMU exit 0. Each L1 must then reach S5 with QEMU exit 0 too.

Both backends **PASS all six boots** with:

* 32 MiB RAM SHA256 verification and 32 MiB copy/compare;
* two workers, 16 SHA256 iterations each (32 hashes per boot), including scheduling;
* a private 64 MiB virtio block disk, nonzero 32 MiB payload, write/fsync/read hash;
* a boot-number sector plus full payload checked across all five child restarts;
* virtio-net restricted user networking, three successful ICMP exchanges per boot.

Expected SHA256 values are computed independently on the host: 32 MiB zero bytes
`83ee47245398adee79bd9c0a8bc57b821e92aba10f5f9ade8a5d1fae4d8c4302`;
32 MiB `Z` bytes `371036ccdfc733fa30542a26a4f276536147c906d1570f0becc1d6f8b868c311`.
The host gate requires the exact ordered six-boot sequence, numeric CPU/work/disk
coverage, five persistence checks, 18 received packets and all child statuses.

Evidence: `/tmp/x86-l2-os-io-final-{outer-kvm,direct-vmx}-20260908.log`.
Timing overlapped the Windows soak and is not a controlled performance benchmark.
Initial packaging failures (PIE detection, RUNPATH libraries, desktop dlopen
plugins and unnecessary PXE ROM loading) are preserved as setup failures; only
the final headless, direct-kernel-boot runs establish the above PASS.
The frozen-runner regression also passes all six complete boots per backend,
including every disk/network/persistence assertion and all seven QEMU exits:
`/tmp/x86-l2-os-frozen-{outer-kvm,direct-vmx}-20260908.log`.

## Pinned upstream Linux KVM selftests

Source is unchanged Linux 7.1.5 commit
`155b42bec9cbb6b8cdc47dd9bd09503a81fbe493`, as pinned in the earlier report.
Each static non-PIE program runs through Linux L1 `/dev/kvm`; its KVM guests are
L2. `kvm_intel nested=0` blocks optional L3 sections, not this L1-to-L2 execution.
This distinction applies especially to `state_test`, APERF/MPERF and bus-lock tests.

The existing `run-linux-selftest.sh` and `xtask` entries are extended, not replaced
with a separate test framework. Each program needs exit 0 and L1 S5. TAP programs
also require exact ordered names, plan, zero skipped/failed assertions and totals.
Exit 4 means an unavailable prerequisite, never PASS. For non-TAP programs,
`assertions=1` means one completed upstream program, not one internal assertion.
The memslot performance program additionally requires all six named subtests and
positive iteration counts: upstream can otherwise silently omit the map subtest.

Disposable-L1-only prerequisite setup is explicit:

* Wait for Linux's natural TSC clocksource calibration; never force unstable TSC.
* Enable KVM's test emulation prefix only for its tests; smaller-MAXPHYADDR is a
  `kvm_intel`, not `kvm`, parameter.
* Mount `/dev/shm` for the upstream named semaphore used by hardware-disable stress.
* Mount memory cgroup/debugfs for access tracking; do not weaken its assertions.
* Bound paging/dirty-log memory to 64 MiB per vCPU; MMU stress uses 1 GiB/two vCPUs.
* Use 4096 memslots for all six perf subtests (map's maximum is 8209), retaining
  each upstream five-second workload. The unsupported default-size attempt is
  separate evidence, not relabeled as a complete six-subtest PASS.
* Hugepage/NX parameters and MSR-device access apply only to a disposable L1.
* Hardware-disable retains all 512 fork/kill rounds, four vCPUs and 64 helper
  threads per child. Both backends pass after prerequisite setup. Direct needs
  the finite 600-second guest budget; the earlier 120-second timeout remains FAIL.

The primary, unmodified-program matrix contains **70 programs per backend**:

| Backend | Program PASS | Unavailable prerequisite (exit 4, runner FAIL) | Other FAIL |
| --- | ---: | ---: | ---: |
| outer-KVM / reference | 60 | 10 | 0 |
| Direct-VMX / project L0 | 54 | 12 | 4 |

Common program PASS (54, with the optional-branch qualifications below):

```text
access_tracking_perf_test apic_bus_clock_test coalesced_io_test cpuid_test cr4_cpuid_sync_test
debug_regs demand_paging_test dirty_log_perf_test dirty_log_test exit_on_emulation_failure_test
fastops_test feature_msrs_test fix_hypercall_test guest_memfd_test guest_print_test
hardware_disable_test hwcr_msr_test hyperv_clock hyperv_cpuid hyperv_extended_hypercalls
hyperv_features hyperv_ipi hyperv_tlb_flush irqfd_test kvm_binary_stats_test kvm_clock_test
kvm_create_max_vcpus kvm_page_table_test kvm_pv_test max_vcpuid_cap_test
memslot_modification_stress_test mmu_stress_test msrs_test platform_info_test pre_fault_memory_test
recalc_apic_map_test set_boot_cpu_id set_memory_region_test set_sregs_test
smaller_maxphyaddr_emulation_test smm_test state_test steal_time sync_regs_test
system_counter_offset_test tsc_msrs_test ucna_injection_test userspace_io_test
userspace_msr_exit_test xapic_ipi_test xapic_state_test xapic_tpr_test xcr0_cpuid_test xss_msr_test
```

| Remaining program | Reference | Direct | Observed reason/scope |
| --- | --- | --- | --- |
| `rseq_test` | PASS, 2 L1 CPUs | Unavailable | Requires two L1 CPUs; Direct AP ownership is not implemented |
| `tsc_scaling_sync` | PASS | Unavailable | Direct's conservative mask does not offer the required TSC scaling |
| `memslot_perf_test` | PASS, all six, 4096 slots | FAIL, exit 142 | RW handshake exceeds upstream `alarm(10)` |
| `nx_huge_pages_test` | PASS | FAIL, exit 254 | First 2 MiB-page count expects 1, observes 0 |
| `dirty_log_page_splitting_test` | PASS | FAIL, exit 254 | Hugepage-backed count expects 0x8000 base pages, observes 0 |
| `vmx_exception_with_invalid_guest_state` | PASS | FAIL, exit 137 at 120 and 600 s | Test-only `unrestricted_guest=0`; pending-exception/error-return stress does not finish within either bound |

The two hugepage assertions are explained by the current policy:
`nested_vmx::TRUSTED_EPT_VPID_CAPABILITIES` excludes both EPT large-page size bits.
Linux 7.1.5's `ept_caps_to_lpage_level()` therefore selects `PG_LEVEL_4K`, passed
by `vmx.c` to `kvm_configure_mmu()`. These failures do not demonstrate corrupted
large-page mappings: this Direct policy does not offer those mappings at all.
No capability bits are added merely to satisfy the tests. The lack of large
pages is also relevant to performance, but is not a complete diagnosis of every
Direct slowdown or timeout.

The invalid-state program first checks repeated emulation-error returns, then
uses a 200-microsecond periodic signal to catch KVM with an exception pending.
Its second phase is timing-sensitive rather than a fixed iteration count.
A timeout alone does not locate the stalled phase or prove state corruption.
The unchanged reference ELF passes this final profile in about 0.35 seconds
from L1 kernel boot, including S5. Direct still times out at 600 seconds with
process exit 137, then reaches clean L1 S5. Evidence:
`/tmp/x86-invalid-state-600-{outer-kvm,direct-vmx}-20260908.log`.
The bounded diagnostics below complete the handler's error-return checks on
both backends; a one-shot timer control also lets Direct finish the main phase.
This points to periodic-signal overhead starving main-thread progress, not a
demonstrated stuck handler ioctl. It does not replace the original periodic
program's FAIL or establish acceptable timer/exit-path performance.

Unavailable on both (10): `amx_test` (required XTILE state),
`monitor_mwait_test` (L2 MWAIT CPUID, even with the explicit `+monitor` L1 profile),
`pmu_counters_test` and `pmu_event_filter_test` (PMU), `aperfmperf_test`
(disable-exits APERF/MPERF capability), `kvm_buslock_test` (bus-lock exit capability),
`xen_vmcall_test` and `xen_shinfo_test` (Xen HVM capabilities),
`private_mem_kvm_exits_test` and `private_mem_conversions_test` (software-protected
VM type). These are observed capability/prerequisite results in this pinned
environment, not blanket claims about native CPU instruction support.

The other pinned x86 source programs require L3 VMX/SVM or AMD SVM/SEV. Neither is
offered to these L2s; the non-x86 `arch_timer`/`get-reg-list` sources are not x86
build targets. They are outside this finite x86_64 L2 matrix, not passing tests.

Primary evidence groups: `/tmp/x86-selftest-*`, `/tmp/x86-expanded-*`,
`/tmp/x86-feature-*`, `/tmp/x86-generic-*`, `/tmp/x86-last-*` (all dated 20260908),
plus `/tmp/x86-rseq-outer-2L1-20260908.log`,
`/tmp/x86-dirty-perf-direct-final-20260908.log`,
`/tmp/x86-mmu-direct-final-20260908.log`,
`/tmp/x86-memslot-4096-{outer-kvm,direct-vmx}-20260908.log` and the 600-second
hardware-disable records. Aggregate completion records are in
`/tmp/x86-qemu-remaining-matrix-20260908.log` and
`/tmp/x86-qemu-last-matrix-20260908.log`.

Two intermediate Direct runs finished the actual guest workload and S5 but the
host shell failed after its runner file was edited during execution. They remain
harness-invalid, not overall PASS. The frozen-runner dirty-log and MMU reruns
above both finish with host exit 0. Runners are not edited during final execution.

Program exit 0 is not evidence for optional unexecuted branches. Successful runs
explicitly report these limitations: `state_test` omits L3 state checks;
`smm_test` omits VMX-inside-SMM; `hyperv_cpuid` omits enlightened VMCS;
`set_memory_region_test` omits guest-memfd regions; `pre_fault_memory_test` omits
VM type 1. Those optional branches are not included in the claimed L2 coverage.

## Pinned kvm-unit-tests as L2

Source: `714bfd622b00cb5e8f2dad9e620641a0adb74ab7`. All 61 selected cases and their
exact CPU/memory/argument/time limits live in `linux-l2-kunit-cases.txt`.
`run-linux-kunit-test.sh` uses the existing UKI/UEFI runner. Its one-CPU, 4 GiB L1
uses the explicit `q35-smoke-1g` PCI fixture; this is not platform-derived EPT.

Coverage includes APIC/NMI and I/O APIC, L2 SMP, 12 VM-exit microbenchmarks,
ordinary/forced/reduced-MAXPHYADDR paging, emulator/instruction tests, XSAVE,
debug state, MSRs, PCID, protection keys, UMIP/LA57/LAM, L2 S3, Hyper-V KVM ABIs and
emulated Intel IOMMU. Hyper-V ABI tests and L2 S3 are not Windows Hyper-V boot or
Direct L1/physical sleep validation. VMX/SVM tests needing L3 and i386-only build
targets are not counted as this x86_64 L2 suite.

`isa-debug-exit` success is QEMU exit 1, failure exit 3 and whole-test skip exit 77.
The shared AWK gate distinguishes PASS, FAIL, SKIP and PARTIAL (some skipped or
expected-failure assertions). It rejects malformed/duplicate summaries, embedded
failure diagnostics, bad status, incomplete/reordered matrices and late failures.
Legacy cases also require their specific completion records, not just exit 1.
The full runner is nonzero if any FAIL, SKIP or PARTIAL remains.

The APIC tests retain upstream 30,000/100,000 NMI repetitions. Initial 30/60-second
timeouts occurred on both backends; the final bounded profile allows 1200 seconds
per APIC case. Reduced-width paging receives 600 seconds. Forced-emulation paging
initially receives 600 seconds; a separate final profile allows 1800 seconds after
Direct reaches 340 progress dots but times out at 600 seconds. These are test
budgets, not changes to production VMX semantics or reductions of test assertions.
Async-PF uses a dedicated disposable 2 GiB swap disk and a child-only memory cgroup
(384 MiB pressure threshold, 1536 MiB hard ceiling) so real asynchronous faults
can occur. It never changes host swap. A too-small hard ceiling initially caused
guest-cgroup OOM during swap writeback; that attempt is a setup failure, not PASS.
The corrected reference setup separately passes with 32,517 actual asynchronous
PF events, zero `oom`/`oom_kill` counters, debug-exit success and clean L1 S5:
`/tmp/x86-kunit-asyncpf-pressure-high-outer-20260908.log`.

The scratch device is selected by the existing runner's `THINHVDATA` serial,
rejecting duplicates and requiring the exact 2 GiB size before formatting. An
initial enumeration-based `/dev/vda` assumption failed the size guard because
that device was the test ESP; no swap formatting happened in that failed attempt.

Initial missing Multiboot ROMs and child-QEMU consumption of the case-list stdin
were runner errors. Required ROMs are now packaged and child stdin is `/dev/null`.
Aborted/setup-invalid runs do not count as completed test coverage.

Final-budget reference matrix: **50 PASS, 0 FAIL, 5 SKIP, 6 PARTIAL**, all 61
executed and clean L1 S5. x2APIC completes 56 assertions including nmi-after-sti,
multiple-NMI and pending-NMI. xAPIC completes 45 assertions with its intentionally
disabled x2APIC subtest skipped. Async-PF handles 32,455 actual events. Whole-matrix
runner status remains nonzero because SKIP/PARTIAL is not all-PASS.
Evidence: `/tmp/x86-kunit-all-final-budget-outer-kvm-20260908.log`.

The matching Direct matrix executes all 61 cases: **49 PASS, 1 FAIL, 5 SKIP,
6 PARTIAL**, followed by clean L1 S5. Its x2APIC test completes all 56 assertions;
Async-PF handles 32,444 actual events with zero OOM counters. The one failure is
the 600-second forced-emulation paging timeout (exit 137). No completed assertion
failure is printed before that timeout; this is not a completed paging PASS.
Evidence: `/tmp/x86-kunit-all-final-budget-direct-vmx-20260908.log`.

The isolated 1800-second Direct FEP run also times out (exit 137), reaching 436
progress dots, with no printed assertion failure. The FEP bit is the highest
permutation bit in upstream `access.c`: approximately the first 292 dots are
ordinary accesses, then the forced-emulation half runs. Thus the 340-to-436-dot
progress between the 600/1800-second attempts must not be extrapolated as a
uniform rate over the whole test. It suggests roughly an hour for all 38,338,566
cases in this setup. A separate bounded 5400-second profile verifies
the remaining combinations, without removing any assertions or CPU features.
Evidence: `/tmp/x86-kunit-access_fep-final-direct-vmx-20260908.log`.

**Full FEP completion: PASS on both backends, 38,338,566 tests and zero failures
each**, followed by clean L1 S5 and host runner exit 0. L1 kernel uptime at final
poweroff is 136.061 seconds for reference and 3936.139 seconds for Direct
(65.6 minutes, 28.93x). This completes the previously unexecuted permutations;
the 600/1800-second timeouts remain historical FAILs. It does not erase the
separate intermittent reduced-MAXPHYADDR assertion failures below.
Evidence: `/tmp/x86-kunit-access-fep-5400-{outer-kvm,direct-vmx}-20260909.log`.

Both backends' whole-case skips are `pks`, `pmu_lbr`, `pmu_pebs`, `tsx-ctrl`,
and `cet`. Partial cases are `xapic`, `emulator`, `memory`, `pmu`,
`vmware_backdoors`, and `la57`; unavailable x2APIC/MOVBE/pcommit/PMU features and
upstream known-erratum/unsupported subcases are not claimed as passing coverage.

The reduced-MAXPHYADDR access case failed with a real accessed-bit assertion in
an earlier reference run: PTE `0x2000061`, expected `0x2000041`, one failure among
2,899,975 tests (debug-exit 3). It passes in this final matrix. That historical failure
is not erased by the later pass. Three isolated repeats finish **3 PASS on
reference; 1 PASS and 2 FAIL on Direct**. Direct also passes it in the final full
matrix, so a single green run does not establish repeatability.
The first isolated Direct repeat fails with PDE `0x20000e7`, expected `0x2000087`
(A/D bits), again one failure among 2,899,975 tests; the second repeat passes.
The third repeat finds PDE `0x20000e3`, expected `0x20000a3` (dirty bit), one failure.
All retain clean L1 S5 and the exact debug-exit status. This demonstrates
intermittency in both backend profiles, not an exclusively project-L0 failure.
These guests explicitly enable KVM's test-only `allow_smaller_maxphyaddr=1`;
the real host setting remains `N` and is not changed to manufacture a control.
Repeat evidence: `/tmp/x86-maxphyaddr-repeat-{outer-kvm,direct-vmx}-{1,2,3}-20260908.log`.

## QEMU/KVM performance observations

The final `vmexit.flat` A/B runs report `(rdtsc_end - rdtsc_start) / iterations`
after the upstream adaptive loop reaches its duration goal. These are TSC ticks
per benchmark iteration, **not physical-L0 overhead measurements**. The Windows
soak had already ended. There is no CPU isolation/frequency-controlled benchmark
environment, and multi-vCPU L2 cases share one L1 CPU.

For the separate frozen six-Linux-L2-boot workload, L1 kernel uptime at final
poweroff is 22.804 seconds on reference and 180.555 seconds on Direct (7.92x).
This includes that workload's L2 boots/I/O and L1 setup, excludes preceding UEFI
time, and is an observation in QEMU rather than a physical-machine benchmark.

| Benchmark | Reference | Direct | Direct/reference |
| --- | ---: | ---: | ---: |
| CPUID | 12,985 | 314,669 | 24.23 |
| VMCALL | 38,983 | 982,954 | 25.21 |
| CR8 read | 8 | 9 | 1.12 |
| CR8 write | 17 | 11 | 0.65 |
| PM timer IN | 19,595 | 389,389 | 19.87 |
| IPI | 66,311 | 2,428,565 | 36.62 |
| IPI + halt | 59,686 | 2,413,979 | 40.44 |
| TSC deadline | 12,504 | 781,350 | 62.49 |
| Immediate TSC deadline | 25,731 | 1,081,526 | 42.03 |
| CR0.WP toggle | 54,910 | 2,008,217 | 36.57 |
| CR4.PGE toggle | 1,505 | 1,939 | 1.29 |

The exit/timer-heavy paths are substantially slower in this QEMU setup; fast
unintercepted register paths are controls, not proof of additional VM exits.
The initial one-vCPU pause round-robin reports eight ticks on both backends, but
with one CPU it does not wait for another CPU and cannot establish that behavior.
The corrected two-vCPU profile **passes on both backends**, reporting 4,793,354
reference and 5,319,799 Direct ticks/iteration (1.11x). This exercises actual
cross-vCPU PAUSE round-robin, not proof of exposed VMX PLE capability semantics.
Evidence: `/tmp/x86-kunit-vmexit_ple_round_robin-final-{outer-kvm,direct-vmx}-20260908.log`.

An additional **diagnostic-only** memslot binary changes just upstream
`alarm(10)` to `alarm(300)` in `host_perform_sync()`. Assertions, all six subtests,
five-second per-subtest target and 4096-slot cap remain. Its results must stay
separate from the primary unmodified-program FAIL above. Original source and
binary are preserved; the diagnostic copy is outside Git.

* Original source SHA256: `57a3ba1620292906742b9e3ab301d5ad5605a815dd866abef68a99bef87a0c66`.
* Diagnostic source SHA256: `911d4bf8f5e098dff00685face8615db26c1d507cff08dfac58a57c385afdc3f`.
* Original ELF SHA256: `cbaf2c34cc3af73081b89b5b52e02c3a9d554f3d92aa9850a37e4816ea56dc2b`.
* Diagnostic ELF SHA256: `e943b9af8ffaddd402ed0cbb61f274de55b1f3c0e9edf7067e2d70e598263884`.

Build evidence: `/tmp/x86-memslot-wait-diagnostic-build-20260908.log`;
both diagnostic runs **PASS all six subtests and clean L1 S5**:
`/tmp/x86-memslot-wait-diagnostic-{outer-kvm,direct-vmx}-20260908.log`.
Direct's RW workload completes one iteration in **24.657132347 seconds**;
reference completes 228 iterations, averaging 0.021986432 seconds. The upstream
RW workload touches each page in a 512 MiB region. This shows forward completion
with a longer handshake deadline, not a primary unmodified-program PASS or a
controlled steady-state slowdown ratio (the iteration counts differ drastically).

An additional invalid-state **diagnostic copy** preserves all original checks,
the 200-microsecond timer, and KVM calls. Bounded `write()` markers identify the
initial invalid-state pair, the timed phase, first SIGALRM and first observed
pending exception. Markers can affect timing; diagnostic outcomes cannot replace
the primary unmodified-program result. Neither source nor ELF is committed.

* Original source SHA256: `3d6ec38d40b71affd1128d46a72a953374b59cbb25c989f4789ff5bf4f1a1504`.
* Diagnostic source SHA256: `3db3b299922ca273eecaaf52699c81babebbfc478f17678c101ea4eb62e9d69f`.
* Original ELF SHA256: `f939d1556f28ea13fe28761ecdbb090c2b0e2be0155add4ad1131e568298e052`.
* Diagnostic ELF SHA256: `6303ea0682047858d0699e762254f4e97b47e0bdcf1a414d8653d7c43fdcb306`.

Build evidence: `/tmp/x86-invalid-state-diagnostic-build-20260909.log`;
reference completes all six phase markers and passes. Direct completes the
initial invalid-state pair, starts the timed phase, receives SIGALRM **and
observes a pending exception**, then times out without the timed-phase completion
marker. Thus this run is not merely failing to catch the pending-exception window.
It localizes the remaining noncompletion to processing after that observation;
this first diagnostic alone does not distinguish the following ioctls.
Both L1s reach clean S5. Evidence:
`/tmp/x86-invalid-state-phase-diagnostic-{outer-kvm,direct-vmx}-20260909.log`.

A second bounded diagnostic adds markers around pending-state `SET_SREGS` and
each of the two following `KVM_RUN` calls. A trace flag is true only in the first
pending handler, keeping output bounded; all original KVM calls/checks and the
200-microsecond periodic timer remain unchanged. **Both backends complete
`SET_SREGS` and both handler `KVM_RUN` calls, including their original checks.**
Reference then completes the main timed phase and passes. Direct does not emit
the main timed-phase completion marker and times out at 600 seconds (exit 137),
then reaches clean S5. Thus the observed noncompletion is not a blocked first
pending handler's state-setting or KVM-run ioctl.
Source SHA256 `943bb60091c295ac12398baaede4afded4603ee705bd77acb5d153b557ffcc4f`;
ELF SHA256 `5dca89ab48608284b037d45eac2258c06ca10d3baf0f2635ced7816f864340ad`.
Build evidence: `/tmp/x86-invalid-state-call-diagnostic-build-20260909.log`.
Run evidence: `/tmp/x86-invalid-state-call-diagnostic-{outer-kvm,direct-vmx}-20260909.log`.

A final **one-shot diagnostic control** starts again from the original source,
not either trace copy. It changes only timer scheduling and bounded measurement:
the initial 200-microsecond expiration and original explicit rearm when no
exception is pending remain, but automatic periodic rearming is disabled.
All original KVM calls and assertions remain. `CLOCK_MONOTONIC` measurements
around the pending handler use the existing timespec helpers; constant-length
`write()` markers report only whether its duration reaches 200 microseconds.
No formatted signal-handler output is introduced.

**Both backends PASS every original error-return check and finish the main
phase**, with program exit 0 and clean L1 S5. Reference reports handler duration
below 200 microseconds; Direct reports at least 200 microseconds. Final L1 S5
uptimes are 0.345237 and 0.366127 seconds respectively, not isolated test timings.
This is evidence **consistent with periodic SIGALRM starving main-thread progress
under Direct's overhead**, an inference rather than proof of all interrupt/state
semantics. The original periodic program still FAILs; neither production VMX nor
the primary upstream source/binary is changed to hide that result.

* One-shot source SHA256: `4f4e9661807de180c3c83fbfb22611d1ca79b80f56523b0c92f93d981bb60a79`.
* One-shot ELF SHA256: `a5a36b661c5331101c8fff9106eecfd1b4d64dad4a6f37ba9785188a0a3663fe`.

Build evidence: `/tmp/x86-invalid-state-oneshot-diagnostic-build-20260909.log`.
Run evidence: `/tmp/x86-invalid-state-oneshot-diagnostic-{outer-kvm,direct-vmx}-20260909.log`.
The diagnostic source/ELF remain outside Git alongside the unchanged originals.

## Windows reference control

**PASS:** the complete 60-minute WSL2 daily soak, 376 workload rounds, reboot,
persistent data, post-reboot L2 verification, zero BugCheck/WHEA and Hyper-V errors,
then natural final S5 and runner exit 0. Finished 2026-09-08 23:29:08 JST. The child
qcow2 check reports no errors. Evidence:
`/tmp/x86-windows-matrix.hD1LuX/reference-final-poweroff/` and
`/tmp/x86-windows-soak-poweroff-20260908.log`.

This supersedes only the earlier reference-soak final-shutdown failure. The runner
now has a strictly bounded `WINDOWS_POWEROFF_TIMEOUT_SECONDS` (default 120, allowed
1..1800); this rerun explicitly used 1200 seconds. All 60 minutes were repeated
from a fresh child of the golden seed. Updates were not disabled. A marker or
forced QEMU termination cannot satisfy this final-shutdown gate.

Plain QEMU/KVM `hyperv`, `wsl` and `wsl-s4` controls also pass in fresh children:
`/tmp/x86-windows-matrix.hD1LuX/plain-control/`. Direct normal desktop PASS and
Direct Hyper-V's 1200-second boot failure remain the earlier report's results.
No outer-KVM or plain-control success is attributed to project Direct-VMX.

Post-run seed checks pass: both golden qcow2 files and the base raw image retain
their recorded size/mtime/ctime; all six recorded firmware-variable/ready-file
SHA256 checks match. Evidence: `/tmp/x86-final-seed-integrity-20260909.log`.
The earlier manifest did not hash the complete TPM directories, so no full TPM
hash-comparison claim is made; test TPM directories were private copies.

## Build and regression validation

The frozen final-regression batch completes all 21 scheduled commands: 17 exit 0,
four nonzero (Direct FEP at 1800 seconds, Direct reduced-MAXPHYADDR repeats 1/3,
and Direct invalid-state at 600 seconds). It does not stop after the first FAIL.
Aggregate: `/tmp/x86-qemu-final-regress-20260908.log`.

| Evidence class | Check | Result |
| --- | --- | --- |
| Build/static | `cargo xbuild x86`, `cargo fmt --check`, shell syntax and diff check | PASS |
| QEMU TCG/KVM | `cargo xrun x86`, `cargo xrun x86 --release` | PASS: nine ordinary cases each |
| QEMU/KVM | `LINUX_KVM_CYCLES=4096 LINUX_KVM_TIMEOUT_SECONDS=600 cargo xrun x86 --nested --release` | PASS: all six suite cases, including 4096 lifecycle cycles on each backend and clean S5 |
| Physical hardware | Original Windows/Linux, activation status, devices, AP ownership and power | UNVERIFIED; daily-use NO-GO |

Evidence: `/tmp/x86-final-upstream-xbuild-20260908.log`,
`/tmp/x86-frozen-ordinary-{debug,release}-20260908.log`,
`/tmp/x86-frozen-nested-4096-20260908.log`, `/tmp/x86-frozen-fmt-check-20260908.log`.
Each backend's lifecycle run creates/destroys 8192 VMs and performs 589,824
`KVM_RUN` calls, 57,344 memslot remaps, 65,536 long64 checkpoints, 262,144 paging
checks, 65,536 INVLPG and 196,608 CR3 writes. These are the same strict ordered
coverage gates as the earlier regression, not inferred from one success marker.

The final FEP/first-phase-diagnostic batch completes ten commands: nine exit 0;
only the Direct first-phase diagnostic causes its A/B wrapper to exit nonzero.
Aggregate: `/tmp/x86-qemu-completion-probes-20260909.log`. Subsequently the finer
call-level A/B diagnostic completes (reference PASS, Direct timeout FAIL), and
the one-shot A/B diagnostic completes with both backends PASS as detailed above.
No unfinished test or diagnostic is counted as a result.

Already completed: `cargo fmt --check` and `cargo xtest -p` for `xtask`,
`x86_64_hal`, `nested_vmx`, `x86_uefi_loader`, `x86_guest_uefi_test`:
**132 passing host tests**, zero failures across 13 feature/package executions.
Evidence: `/tmp/x86-final-upstream-host-packages-20260908.log`.

After adding the long FEP profile, `cargo xtest -p xtask` again passes all 31 tests,
including individual 1..5400-second limits and the aggregate 14,280/14,281-second
boundary. The whole-QEMU budget is 14,400 seconds, reserving 120 seconds for
setup/shutdown. The current 61-case manifest totals 14,070 seconds, so even child
deadlines do not intrinsically exceed that parent deadline. This changes only
test limits; Windows' separate 1800-second maximum is unchanged.
Evidence: `/tmp/x86-kunit-5400-budget-host-20260909.log`.

The post-profile five-package rerun again passes **132 host test executions,
zero failures, across all 13 configured feature/package executions**:
`/tmp/x86-completion-host-{xtask,x86_64_hal,nested_vmx,x86_uefi_loader,x86_guest_uefi_test}-20260909.log`.
`cargo xbuild x86` and `cargo fmt --check` pass again:
`/tmp/x86-completion-xbuild-20260909.log`, `/tmp/x86-completion-fmt-check-20260909.log`.

The unfiltered workspace `cargo xtest` plan is not invoked: its unrelated legacy
AArch64 `file/scripts/run_fat32_virtio_test.sh` uses `sudo fdisk`, formatting and
loop mounts. This x86 validation does not modify that runner or add privileged
host operations. The affected x86 package rows and x86 QEMU entrypoints are
validated separately; no full-workspace/AArch64 test PASS is claimed.

## Reproduction entrypoints

Run from this checked-out repository inside `nix develop --accept-flake-config`.
Use the pinned, built upstream artifacts above. For each backend (`outer-kvm`
and `direct-vmx`), the commands used are the existing script entrypoints:

```sh
LINUX_SELFTEST_BACKEND=direct-vmx LINUX_SELFTEST_NAME=cpuid_test \
  LINUX_SELFTEST_ELF=/tmp/thin-hv-kvm-selftests-7.1.5.MUloxc/out/x86/cpuid_test \
  bash scripts/x86_64/run-linux-selftest.sh

LINUX_KUNIT_BACKEND=direct-vmx LINUX_KUNIT_CASE=all \
  LINUX_L2_KUNIT_DIR=/tmp/thin-hv-kvm-unit-tests-20260908/x86 \
  LINUX_L2_QEMU=/nix/store/dz3ivvcn2916ac16l95vzgshikxrbicr-qemu-host-cpu-only-for-vm-tests-10.1.5/bin/qemu-system-x86_64 \
  bash scripts/x86_64/run-linux-kunit-test.sh

LINUX_L2_OS_BACKEND=direct-vmx \
  LINUX_L2_QEMU=/nix/store/dz3ivvcn2916ac16l95vzgshikxrbicr-qemu-host-cpu-only-for-vm-tests-10.1.5/bin/qemu-system-x86_64 \
  bash scripts/x86_64/run-linux-l2-os-test.sh

cargo xrun x86
cargo xrun x86 --release
LINUX_KVM_CYCLES=4096 LINUX_KVM_TIMEOUT_SECONDS=600 cargo xrun x86 --nested --release
```

The absolute paths identify this machine's disposable build cache, not committed
dependencies. Replace them with equivalently pinned local builds when reproducing.
The 70-program allowlist is in `run-linux-selftest.sh`; generic upstream binaries
are directly under `out/`, architecture-specific ones under `out/x86/`.
`LINUX_KUNIT_CASE` also accepts one manifest name for isolated repeats. Historical
full-matrix logs used the `f4211fb` manifest; the later two-vCPU PAUSE and longer
FEP profiles are separately identified above, not retroactively imposed on logs.
The scripts reuse the project UKI/UEFI runner and fail closed on a wrong backend;
their pure transcript checks run through the existing `cargo xtest -p xtask` row.
