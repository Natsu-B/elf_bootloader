# x86_64 validation evidence - 2026-08-30

This manifest ties the compact claims in the architecture document to exact commands, hashes, and
boot markers. Windows disks, dumps, and raw serial logs are intentionally not committed.

- Historical validation baseline: 2e4af630c7566ee070aaa1276ac80ce087f5e12a
- Upstream refactor merged at: 065a40125daed43ff7e19ff0b591c8e4bb09f30c
- Branch: feat/x86-thin-monitor
- Committed base for the final suspend follow-up: 0b695c875333557728783afd6a7dbffc5e3c0dd1
- Final no-hook direct-chainload code: 1ad50dc5a9ff12c1b132c0fbf55220467a8587f8
- Later non-runtime build-input commit: a86492c12221b413b481030ad7b09b060313a273
  (adds curl to the Nix development shell)
- Trusted startup optimization follow-up: 63c5d2b48fe5a377b02b701a1cf479644f9f95f3
- 2026-09-01/02 trusted-path follow-up: d4d83832297f6df578f27df96496e5899fe91f5f
  through e149051 (`same-ESP`, structural TCB split, location coverage, repeatable soaks, and
  fail-closed shutdown/S4 validation)

The original sections below describe the historical validation baseline. Separately labelled
follow-ups record later code, artifacts, and validation through 2026-09-02.

## Trust boundary

The current trusted-outer-kvm feature directly calls `StartImage` on the selected guest. It installs
no resident project runtime or UEFI Runtime Services hook and executes no project VMXON, VMLAUNCH,
VMRESUME, or VM-exit reflection loop. CPU microcode, Linux KVM, QEMU, OVMF, Windows, and Hyper-V
are therefore in the trusted computing base. The project-owned active path is the UEFI bootstrap
and chainload; each VM receives a separate complete OVMF variable store.

The earlier profile-overlay implementation remains tested and is retained in the direct research
path. Its historical measurements below remain evidence for boot and hook behavior, but corrected
Windows S4 testing excludes it from the daily-use trusted configuration.

The direct-VMCS backend remains available as an explicitly unsafe research path. Its measured
Windows watchdog result is kept separate from the trusted KVM passes below.

## Environment

| Item | Value |
| --- | --- |
| Host | Linux 7.1.5 x86_64 |
| Nix | 2.31.5 |
| Rust | rustc 1.91.0-nightly (de3efa79f 2025-08-08) |
| Cargo | 1.91.0-nightly (840b83a10 2025-07-30) |
| QEMU | 10.0.2 |
| GNU objdump | GNU Binutils 2.44 |
| swtpm | 0.10.1 |
| kvm_intel nested | Y |
| kvm_intel enable_shadow_vmcs | Y |

Commands ran in the flake development shell fixed to the validated commit:

~~~sh
nix develop --accept-flake-config \
  "git+file:///path/to/aarch64_type1_hypervisor?rev=2e4af630c7566ee070aaa1276ac80ce087f5e12a" \
  --command COMMAND
~~~

## Historical baseline release artifacts and active-code proxy

At the historical `2e4af63` baseline, after `cargo xbuild x86 --release`:

| Artifact | PE bytes | .text bytes | Decoded VMX sites | SHA-256 |
| --- | ---: | ---: | ---: | --- |
| x86-uefi-monitor.efi | 81,920 | 69,801 | 303 | 615d60a7cedb7232c2df6470cac7091a59537aaab36b48606ca2588ba9b3cc94 |
| x86-uefi-kvm-monitor.efi | 24,576 | 18,937 | 0 | db8aa1e709fe7d4abd9cecb5a7e83f68a70a5beb50cc477719486be0423b7ba5 |
| x86-uefi-loader.efi | 81,920 | - | - | 0585343580b84deceb201abe932abb52c165e485ebaeabc3e31e211fbd0d60cb |
| x86-uefi-kvm-loader.efi | 24,576 | - | - | f73e538f8e89e50bd618b2789b60786943b0b975228486eba281cd0edae6c941 |

That historical trusted runtime PE was 70.0% smaller and its .text was 72.9% smaller. This is an
historical active-code proxy, not a formal TCB proof. VMX sites were counted from GNU objdump -d output for vmcall, vmclear,
vmlaunch, vmresume, vmptrld, vmptrst, vmread, vmwrite, vmxoff, vmxon, invept, invvpid, and vmfunc.
LLVM objdump does not decode the same byte sequences identically; use GNU objdump from the Nix
shell for this measurement.

## Historical baseline build and automated checks

| Check | Result | Local raw-log SHA-256 |
| --- | --- | --- |
| x86_64_hal, overlay, loader direct/trusted unit tests plus cargo xrun x86 --release | 12 + 4 + 2 + 2 tests and both UEFI smoke paths PASS; 21.155 s | a6c9ed1ade680dece2cfa2e931a5921cd07c8fb4d9f59d671fa351ee08413113 |
| cargo xbuild | AArch64 elf-hypervisor build PASS; 1.235 s | abbaf5e1c7a8b1a36c854a09813ab15b58c0edc084a368ddbde7439f853e18f7 |
| cargo xtest -t std -t unit | 17/17 plan entries PASS | 32abf33721b99db2a570b2948f09beed1bfb02e38c89576b86daa08026ba6deb |
| cargo xtest -t virtio_blk_modern | UEFI/QEMU PASS | 28a8742c78d4d897440426a07a6e3a3efdd9a43bc79c8a53658868d1e0d90250 |
| cargo xtest -t stage1_translation | U-Boot/QEMU PASS | a7b0cbfe75e19c97fc1caaa0f3fce15191856fcc3480a62eb2c63ba1e57f5278 |
| cargo xtest | Not started: existing sudo -v preflight could not authenticate non-interactively | d1b8d9220e57b11f27c615bcb37d4013a5b767692df74d2b3166a92c1be66411 |

The full xtest exit was an environment/preflight failure, not a failed test. The two boot tests above
were selected because they exercise the AArch64 UEFI and U-Boot runners without sudo.

The retained direct-VMCS Linux L2 log has SHA-256
ff72ef1868831c7c788402f240d89ace6644f5905a657a6f80d67d4c933cebdb. It contains Linux 7.1.5,
direct L1 VMLAUNCH/VMRESUME markers, deterministic KVM exits, and
thin-hv: linux L1 L2 KVM PASS.

## Historical same-baseline Windows Hyper-V A/B

Both runs used independent qcow2 children of the same successful Windows baseline and copies of the
same starting UEFI variable store and TPM state. The harness command differed only by mode.

| Mode | Start (JST) | End (JST) | Wall time | Result |
| --- | --- | --- | ---: | --- |
| hyperv (direct QEMU/KVM control) | 17:00:50.266719051 | 17:01:23.606217102 | 33.335 s | Hyper-V PASS |
| trusted-kvm-hyperv | 17:02:48.521982872 | 17:03:22.869858579 | 34.344 s | profile 1, Hyper-V PASS, no direct VMLAUNCH |

The trusted path was 1.009 seconds (3.0%) slower in this one boot-to-marker pair. This is a sanity
check, not a steady-state benchmark. Structurally, the trusted path has no project VM-exit loop.

| Local artifact | SHA-256 |
| --- | --- |
| direct harness | 044d39456826c7d6cbcbbf333d2999aaf8dd2cbe941015cfbb7b4cc3002f2c80 |
| direct COM1 | 54478a9dc16aa0c77bf969710e367e6f16800063764edba61fd013b344075c29 |
| direct COM2 | cb9000c74c33d930ef842631e6c5a84d2a3d2e51043c86cc788f0654c1400fb6 |
| direct QEMU | f0d0033868787d250289d73ae3e7702354f0e3fbd2f982a164e5505827480a9b |
| direct qemu-img check | e1e7e923526726578b5043e4d760cb5133bd6d26cfc0ee24d1fba9af0263bbef |
| trusted harness | 5669e639362dc95399dc080caa9a241a28da58c055c4682e95eb57ff7f430988 |
| trusted COM1 | 21b5534914569d3f78d51ba2dd3af146c87c92032e1e4fb60158057fd4404045 |
| trusted COM2 | 98ec29424a5a3e86991b6b3fd1d38b955a11bd57887aaedacb2236f24f66b235 |
| trusted QEMU | f0d0033868787d250289d73ae3e7702354f0e3fbd2f982a164e5505827480a9b |
| trusted qemu-img check | b25641dc271d33b9d0b41e01e71586ae26e1d4beef7060bbf7542d58efc62f11 |

Both post-run qemu-img checks reported no errors.

## Historical coherent trusted WSL2 run

The final WSL run used one work directory for its harness, COM1, COM2, QEMU, and image-check logs.
The wsl-ready marker was deliberately absent, so the harness exercised its setup/reboot path.

- Start: 2026-08-30T17:04:27.478354937+09:00
- End: 2026-08-30T17:08:04.214773132+09:00
- Wall time: 216.732 seconds
- WSL: 2.7.11.0
- Reported package kernel: 6.18.33.2-2
- Guest uname: 6.18.33.2-microsoft-standard-WSL2
- Guest processors: 0 and 1
- Media stamp: 3d0136133c2beda2c71788709efa297c6025aab0b89b1cb319ae10dcc3f0fa52

COM1 contains two boot epochs, two trusted-runtime markers, two profile-1 markers, and zero direct
VMLAUNCH markers. COM2 contains both processor records, thin-hv-wsl2-guest-ok, and the stamped
WSL2 PASS; it contains no WSL2 FAIL. qemu-img check reported no errors.

| Local artifact | SHA-256 |
| --- | --- |
| harness | 355fc0031876c5e96b543d983e31f77e8e3845d04201c456ef4ac1262e92b040 |
| COM1 | c3ab1c7c5196e4df369c1d320aa77c1fbb35b49bcc694d4331f11a08f3a98c34 |
| COM2 | 3c2b37d47d5b7d3f018f0c3107dac33fb4a01c4511e83a1ddaf41426708c3cbc |
| QEMU | 94629e6aa8f5fb1022c7164ce3dc1fcea4009256a6cdcf9fced09551ed51427c |
| qemu-img check | 1e80ed1fc2e09276e40d19a60db538fe93511971c670daa0c57a6aba78078f71 |

## Direct-VMCS 0x133 forensic boundary

The tracked [read-only transcript](direct-vmx-0x133-forensic.txt) and
[exact verifier](../../../scripts/x86_64/windows/direct-vmx-0x133-readonly.py) independently
re-read the current qcow2 chain without a mount, root, a kernel NBD device, or an extracted dump
file. The tracked files have SHA-256 values
4815a4b9a8823d358251bbc120273d0d67bdc7cd808569d4d7493dc900752103 and
1617fdee5efc71d3239380fc992abc957e6cf044af93b505c5bacd2c5d61ae3d respectively.

The top qcow2 SHA-256 was unchanged before and after:
15ce122b0222f43b6d0e9e3709f03b630669a7ce20938460e0bece9b9f08bbb2.
The verifier confirmed:

- MEMORY.DMP: 380,473,258 bytes, SHA-256
  4be11f89920c0b7dcdfd698a56e115279268725a66aee8dad9ba32f87b8aaa6f;
- bugcheck 0x133 with parameters (1, 0x1e00, 0xfffff806899c43b0, 0);
- guest SystemTime 2026-08-30T12:41:53.719276 and uptime 1:06:43.937784;
- WER minidump: LogonUI.exe, exception 0xc0000005 in Windows.UI.Logon.dll+0xbd259,
  SHA-256 5a66631299619ddd5dfd0557278dd993567c425f3c342c0b9a1eab52ce750fd5.

This supports the measured direct-path slowdown/watchdog boundary. It does not identify a specific
Windows driver as the cause.

The verifier is intentionally image-specific: it rechecks GPT and NTFS signatures, then streams the
recorded runlists using NBD READ requests only. Run it only against an immutable copy or read-only
evidence chain with qemu-nbd and Python 3 on PATH:

~~~sh
scripts/x86_64/windows/direct-vmx-0x133-readonly.py /path/to/windows-hyperv.qcow2
~~~

## Nix reproducibility note

Commit a86492c adds pkgs.curl to the development shell. On that commit,
nix flake check --no-build passed and the evaluated devShell derivation references curl 8.14.1,
QEMU 10.0.2, and GNU Binutils 2.44. Building the newly changed shell was blocked on this host by a
stale external Nix sandbox path, /mnt/data. The already-realized validated shell at the code commit
was used for all tests above.

## Historical trusted startup optimization follow-up

Code commit `63c5d2b48fe5a377b02b701a1cf479644f9f95f3` removes two trusted-only CPUID
probes, 116 successful-boot COM1 bytes, and 14 redundant COM1 initialization writes. This removes
at least 246 port-I/O instructions, excluding transmitter poll retries. At 115200 8N1, the removed
bytes represent 10.069 ms of serialized wire time. The trusted build now links firmware error
formatting and an unreachable fallback only; direct VMX formatters are absent.

| Release artifact | PE bytes | `.text` bytes | `.rdata` bytes | Decoded VMX sites | SHA-256 |
| --- | ---: | ---: | ---: | ---: | --- |
| x86-uefi-monitor.efi | 81,920 | 69,689 | 8,757 | 303 | 97982ba69ec153a33d8989ee580adef221d226c19bd5cd694660f3522551ff28 |
| x86-uefi-kvm-monitor.efi | 23,040 | 17,593 | 2,213 | 0 | dc0cf60a072a7ca4c6a9d14db86c45f9c9d8617d045b03ed6cf8140e08aaf499 |
| x86-uefi-loader.efi | 81,920 | - | - | - | e4e9127e55fb24aa127614c4cd88c920d8d7bac078cf31a5b25650d5b586c4c9 |
| x86-uefi-kvm-loader.efi | 23,040 | - | - | - | 6fede470e343823ced64e8de0638e49b9de6dba710e754e546a35011b8b41ab5 |

Relative to the preceding trusted artifact, the PE shrank by 1,536 bytes (6.25%), `.text` by
1,344 bytes (7.10%), and `.rdata` by 728 bytes (24.75%). GNU objdump still decodes zero VMX
sites and zero CPUID instructions in the trusted runtime. The removed CPUID/backend markers and
direct VMX error strings are absent.

The pre-optimization tracked [A/B measurements](trusted-kvm-overhead-2026-08-30.tsv), SHA-256
`cda0fc8f3fb2e7bcb5436135dcb0e5f58e5910b81d5f38a5a14904604ff86ca3`, used independent
children of one immutable Windows baseline. Pair 2 was excluded because an unrelated formatting
command overlapped its first run. Valid pairs 1, 3, and 4 produced:

| Mode | n | Mean | Sample SD |
| --- | ---: | ---: | ---: |
| direct QEMU/KVM control | 3 | 24.675 s | 2.388 s |
| trusted outer KVM | 3 | 24.154 s | 1.461 s |

The paired trusted-minus-direct mean was -0.520 seconds with sample SD 1.418 seconds and a 95% t
interval of [-4.044, +3.003] seconds. The earlier +1.009-second single-pair result was therefore
boot variance, not evidence of trusted-path overhead. The valid pairs all ran direct first, so
cache/order bias is not excluded.

One fresh post-optimization trusted child reached the Hyper-V PASS marker in 22.898 seconds. It
reported runtime/profile 1, contained no direct VMLAUNCH marker, passed `qemu-img check`, and left
the baseline disk, UEFI variables, TPM state, and artifact hashes unchanged. Its serial log SHA-256
is `c29314a893c6d292c17ee130d1f5891cca7dc5fee138347a350979172de1dd8d`.
Direct/trusted loader unit tests, overlay tests, both UEFI smoke paths, the AArch64 build, and all
17 filtered std/unit plan entries passed.

## Historical protocol-cache and OS-specific follow-up

Code commit `72df83d6c9baa7871693e71574e6fd5c45ff5422` reuses the parent LoadedImage
metadata and the firmware's shared DevicePathUtilities protocol while loading the guest and runtime
image. A successful Linux-profile
boot avoids two parent `HandleProtocol(LoadedImage)` calls and one
`LocateProtocol(DevicePathUtilities)` call. The measured Windows topology avoids three and two
respectively. The trusted bootstrap no longer initializes COM1 or emits successful-path messages;
the runtime entry initializes it once and combines the runtime/profile report. This removes one
seven-OUT initialization and 82 serial bytes, at least 171 port-I/O instructions when every
transmitter poll succeeds. The bytes occupy 7.118 ms on a physical 115200 8N1 link; QEMU's emulated
UART does not serialize them at physical wire rate.

| Release artifact | PE bytes | `.text` bytes | `.rdata` bytes | Decoded VMX sites | SHA-256 |
| --- | ---: | ---: | ---: | ---: | --- |
| x86-uefi-monitor.efi | 81,408 | 69,273 | 8,757 | 303 | 592dcfc752e8a8cd21d24a310b95e5e88e6e2620efb3b82f32717a22dadfd505 |
| x86-uefi-kvm-monitor.efi | 22,016 | 16,825 | 2,149 | 0 | 5abbe2a5f89472d067bc1fd8870f596776233fd6659ce61b3cb80afe8a78dbde |
| x86-uefi-loader.efi | 81,408 | 69,273 | 8,757 | 303 | a33c672b4a79702244f3895b2479c7f16e698b5408d94d1f1e58930cc6f784c7 |
| x86-uefi-kvm-loader.efi | 22,016 | 16,825 | 2,149 | 0 | 1f4add14176ec1629a8c843150411a71391992c03be49f91bfadf03ff0abec40 |

Relative to `b86f4fc`, each trusted PE is 1,024 bytes smaller (4.44%), `.text` is 768 bytes smaller
(4.37%), and `.rdata` is 64 bytes smaller (2.89%). The direct PE also shrank by 512 bytes while its
decoded VMX-site count stayed at 303. The trusted PE still decodes zero VMX and zero CPUID sites.

Windows and Linux were measured separately with six AB/BA-counterbalanced pairs per phase. Every
Windows sample used a fresh child of one immutable Hyper-V baseline, fresh variables and TPM state,
and passed the marker, status, and `qemu-img check` gates. Every Linux sample booted the same
15,682,560-byte UKI (SHA-256
`5d2ab8dff5ff2a82873eb494882c81c1e140f3278b1eaaaa83cd93f12eea2d94`) with fresh ESP and
variables, and proved VMX, `kvm_intel`, `/dev/kvm`, profile 2, and the final PASS marker.

| OS / phase | Direct mean | Trusted mean | Paired trusted-direct mean | 95% paired t interval |
| --- | ---: | ---: | ---: | ---: |
| Windows before | 23.601 s | 22.933 s | -0.668 s | [-1.726, +0.391] s |
| Windows after | 23.430 s | 23.370 s | -0.061 s | [-0.803, +0.681] s |
| Linux before | 1.145571 s | 1.120474 s | -0.025097 s | [-0.053715, +0.003521] s |
| Linux after | 1.120421 s | 1.137510 s | +0.017090 s | [-0.024685, +0.058864] s |

All four intervals include zero. Linux marker detection used 50 ms polling, visible as quantization
in the tracked [OS paired data](trusted-kvm-os-speed-2026-08-30.tsv), so its nominal before/after
shift is not evidence of a regression. The Windows raw TSV hashes are
`930686e8583fe00e02c6355a0eceadba438edd81eca733e6d2f8bb547c1cbd6c` before and
`67497b021c663bffce2b07c754953226208fdde5690e944a60b70cd37b330b7b` after. The corresponding
Linux paired TSV hashes are `7d24925b3fd5bdf1d5ffa7dfe83f5f617c401df32f7322c5da99decd23456229`
and `c7c32b03fd7c47949f996d7e71afcd353bbc6e3e768928ba6982053ae2a9febb`.

A separate no-polling benchmark compared the exact `b86f4fc` trusted artifacts with the new ones.
A blocking serial reader timestamped the common runtime-active line for 20 AB/BA-balanced pairs;
all 42 runs including warmups passed overlay and final guest checks. Spawn-to-runtime averaged
657.296 ms before and 658.538 ms after. The paired new-minus-old mean was +1.242 ms with a 95%
interval of [-5.238, +7.721] ms, so the smaller deterministic startup path produced no detectable
QEMU wall-time change or regression. The tracked
[paired samples](trusted-kvm-uefi-startup-2026-08-30.tsv) come from raw tree SHA-256
`6a6c73506f05843cef294c48ed903b7972ba6e4407faef1171b55ead4466c3db`.

## Direct nested-entry physical-width cache

Commit `260b09d03ee84c3c648c1dfe1c17814965cc172d` retains the validated 12-to-52-bit
physical-address width in the direct runtime. Before it, every nested VMLAUNCH or VMRESUME address
validation executed CPUID leaves `0x80000000` and `0x80000008`. KVM v7.1.5 stores `maxphyaddr` in
host and vCPU state instead of rediscovering it on each nested entry. The project cache therefore
removes two outer exits from the repeated direct-entry path without changing L1's exposed CPUID.
The one-vCPU runtime uses an `AtomicU8` zero sentinel; an SMP design with heterogeneous CPUID
policy would need per-vCPU state.

QEMU 10.0.2 ran four AB/BA-counterbalanced pairs with an identical staged loader, guest UKI,
OVMF code and variables template. Each sample created one KVM VM/vCPU and completed 65,536 real
L2 `KVM_RUN`/`KVM_EXIT_IO` cycles. A guest marker immediately before the loop removed firmware and
Linux boot time from the primary measurement. All eight samples passed their VMX, `/dev/kvm`,
`kvm_intel`, start, final, exit-reason, port, size, count, and `L2OK` data gates.

| Direct runtime | n | Mean START-to-PASS | L2 entries/s |
| --- | ---: | ---: | ---: |
| `63533d5` before cache | 4 | 8.917910 s | 7,349 |
| `260b09d` after cache | 4 | 8.043292 s | 8,148 |

The paired new-minus-old mean was -0.874619 seconds, or 9.81% less time and 10.87% more entry
throughput. Every pair improved; deltas ranged from -0.897899 to -0.832568 seconds, and the 95%
paired t interval was [-0.920617, -0.828621] seconds. The staged inputs were unchanged after all
runs. Old and new monitor SHA-256 values were
`592dcfc752e8a8cd21d24a310b95e5e88e6e2620efb3b82f32717a22dadfd505` and
`6478006bccda5812d8fdcee985767eb6079d02ec78884091186e3632933b581d`; the benchmark UKI hash was
`ec2deb0a6d90be886e0780d882e5e33e05597f14233bcf807de0798ae5175f84`.
The tracked [eight samples](direct-cpuid-cache-2026-08-30.tsv) have SHA-256
`1ec5b3304935b763f3d8a97322cbea6a6a002182f3fdf761e52e72e7467bc8e1`.
The flake-pinned `cargo fmt --all -- --check`, `cargo xtest -p x86_uefi_loader` (three tests),
and `cargo xbuild x86 --release` passed. Separate direct and trusted UEFI/KVM smoke boots also
passed guest CPUID, variable-overlay, and final direct-VMX or trusted-guest markers.

## Historical nested execution and bounded stability soak

The soak host was an Intel Core Ultra 9 185H running Linux 7.1.5. The Linux soak used QEMU 10.1.5;
the flake-pinned Windows runner and direct A/B used QEMU 10.0.2. Its
`kvm_intel` parameters had nested VMX, EPT, EPT A/D, VPID, APICv, shadow VMCS, and unrestricted
guest enabled; KVM's TDP MMU was also enabled. The v7.1.5 KVM audit covered
`arch/x86/kvm/vmx/nested.c`, `vmx.c`, and `vmcs_shadow_fields.h`. In particular, the upstream
shadow-field list already includes common exit state and guest RIP, so the daily-use path keeps
using host KVM rather than duplicating KVM's VMCS12/VMCS02 machinery in the project TCB.

### Linux trusted outer-KVM bounded soak

One isolated trusted-profile QEMU booted the same Linux L1 twice, with one guest reboot between
passes. Each L2 probe opened `/dev/kvm`, issued `KVM_CREATE_VM`, registered guest memory, issued
`KVM_CREATE_VCPU` and `KVM_RUN`, then checked both `KVM_EXIT_IO` and the real-mode guest's
`L2OK` output. Both boots completed 1,000 probes, for 2,000 actual L2 creations and runs.

| Checkpoint | Boot 1 | Boot 2 |
| --- | ---: | ---: |
| trusted runtime active | 0.771 s | 18.306 s |
| nested ready | 1.314 s | 18.772 s |
| five usernet pings | 5.346 s | 22.799 s |
| 128 MiB tmpfs, two workers, 80 SHA-256 rounds | 8.716 s | 26.002 s |
| 1,000 L2 runs complete | 16.041 s | 33.436 s |
| 128 MiB virtio-blk hash / reboot persistence | 17.834 s | 33.551 s |

QEMU exited zero by normal S5 poweroff after 33.576 seconds, with no timeout or failure marker.
The disk payload hash was
`254bcc3fc4f27172636df4bf32de9f107f620d559b20d760197e452b97453917`. Immutable input hashes
matched before and after, and no QEMU or swtpm process remained. The temporary result bundle's
ordered aggregate SHA-256 is
`f6631bcefd193e8e4ecfe90c70f513cb19a2921ace396522186f934a2fd2f8c0`; its serial and timestamped
event hashes are `d8ba90916b218cadda70505bad05f4459314741115430de45e728e3e95b105fa` and
`59fff4078de31dfb23761cfe95fcb1f56b7434acaee2898250e21737b0d17b65` respectively.
The loader, trusted monitor, and soak UKI hashes are
`9e6de84ac550188f832ddf46dc5005825a16354c014a44a0acfb5f015b04d496`,
`4cdbddb8d103f9ad446c99a5a5c095330afdf81752ed9197f2fd5c54c8486f98`, and
`7a1b2aec81595b52518e456ccd71f7b955155047fe27d52d9e4ae858b9bbea85`.

This is a trusted outer-KVM test, not a direct-VMCS result. The CPU, memory, disk, and network
loads ran in Linux L1; L2 was a 4 KiB real-mode correctness probe. External networking, a full
Linux L2 OS, filesystems inside L2, and a long thermal soak remain untested.

### Windows trusted outer-KVM bounded soak

One fresh qcow2 child of the immutable Hyper-V baseline, with independent UEFI variables and TPM
state, first passed a WSL2 gate in 44.771 seconds. The soak then booted it with two vCPUs and
`host,+vmx,-hypervisor,kvm=off`. Each of two workload phases checked the enabled Hyper-V feature,
`HypervisorPresent`, running VMMS, a randomized 256 MiB memory SHA-256, a flushed and reread 128
MiB disk file, 64 MiB of hash-verified TCP loopback traffic, and four WSL commands. The WSL2
utility VM returned `uname`, hashed `/proc/cpuinfo`, hashed 64 MiB of generated data, and shut down
successfully in each phase.

| Check | Phase 1 | Phase 2 after Windows reboot |
| --- | ---: | ---: |
| workload duration | 9.941 s | 8.808 s |
| Windows memory | 256 MiB, SHA-256 PASS | 256 MiB, SHA-256 PASS |
| Windows disk | 128 MiB, SHA-256 PASS | 128 MiB, SHA-256 PASS |
| TCP loopback | 64 MiB, SHA-256 PASS | 64 MiB, SHA-256 PASS |
| WSL2 generated-data hash | 64 MiB PASS | 64 MiB PASS |

The phase-1 disk hash
`1335c5b16fae304222797d621e3fbab9f09743ab00f38db51741589922dd7901` matched after reboot.
The bounded event scan found zero bugcheck/WHEA events and zero critical/error events in the
Hyper-V Hypervisor and VMMS admin logs. Three trusted-runtime profile-1 epochs covered the setup
and explicit soak reboots; none contained a direct VMLAUNCH marker. The final Hyper-V, WSL2, daily
soak, disk-persistence, and event markers all passed.

The QEMU harness exited zero after 265.595 seconds. `qemu-img check` found no errors in either the
work image or immutable baseline. Baseline disk, variable, and TPM hashes and stat metadata were
unchanged, the release loader/monitor hashes still matched, and no QEMU or swtpm process remained.
The ordered evidence-list SHA-256 is
`1ea4816d44df68bdd1b29ab1d9741964b954f3da674cccc53fa40b192a5cc1cc`; COM1 and COM2 log hashes
are `4581b7d04f6154622c296cfbcc0b759f9d9ff77462d30e9588530f1edb02e195` and
`4bc1616999bdfa80fb5c8eb2841db18481c7b9bd6de69d06a157d49a7cba93c4`.

This historical test covers a real WSL2 L2 utility VM but remains a bounded synthetic soak. Its
Windows network load was loopback only; external network traffic, interactive GUI applications,
audio, USB, dedicated WSL event-channel scanning, and multi-hour operation remain untested.

## Final no-hook suspend and artifact follow-up

### Current trusted artifact boundary

The final trusted build emits only `x86-uefi-kvm-loader.efi`. It directly chainloads the selected
guest, prints `resident_runtime=0`, does not stage `MONITORX64.EFI`, and uses the VM's existing OVMF
variable store. The previous trusted runtime-driver artifact in the historical table above is not a
current output or execution dependency.

| Current release artifact | PE bytes | `.text` | `.rdata` | VMX sites | CPUID sites | SHA-256 |
| --- | ---: | ---: | ---: | ---: | ---: | --- |
| trusted `x86-uefi-kvm-loader.efi` | 10,752 | 6,945 | 1,073 | 0 | 0 | `530daf8cd72720d6140d518967536c3a295ff153e37b5d4e9b0605e00ff45b6f` |
| direct `x86-uefi-loader.efi` | 81,920 | 69,977 | 8,757 | 303 | 22 | `7590aaf3abbdc0f3d8b8bc99772e4138a5b45210a6a5a7cc076814c50ef1d162` |

VMX and CPUID columns count instructions decoded by GNU objdump 2.44. The trusted binary is 86.9%
smaller overall and 90.1% smaller in `.text`. Its normal-dependency graph contains only
`x86_uefi_loader` and `r-efi`; `x86_64_hal` and the direct VMX policy crates are absent. A string
audit found the direct-chainload marker in the trusted binary and no runtime-monitor,
variable-overlay, install/restore-hook, or VMX markers. The direct binary retained all of those
markers. This is an active-code and dependency measurement, not a formal proof of the complete CPU,
host KVM/QEMU/OVMF, firmware, or guest TCB.

Commit `60003ec` makes this an xbuild invariant: `xtask` disassembles the trusted PE before copying
it to `bin/` and fails the build if GNU objdump decodes any VMX mnemonic, including its
`vmreadq`/`vmwriteq` spellings. The same build removes the two known obsolete output names instead
of leaving stale EFI files beside current artifacts. Commit `976b2e9` then placed the trusted
chainloader in its own feature-gated module, and `3fbaa9f` removed `x86_64_hal` from its normal
dependency graph by keeping the two serial port instructions local. The final-PE disassembly gate
remains the runnable regression check; the source split and graph reduce what can reach it.

### Linux trusted direct-chainload S3

`scripts/x86_64/run-linux-suspend-test.sh` booted Linux 7.1.5 with two CPUs through the trusted
loader and no resident project runtime. It completed three deep-S3 cycles. CPU1 went offline and
returned on every cycle; the real `/dev/kvm` probe passed before suspend and after each resume.
Every cycle wrote and read a disposable valid `DriverFFFF` EFI load option through native firmware
Runtime Services, remounted `efivarfs`, verified it after resume, deleted it, and confirmed absence.
The run ended with `thin-hv: linux S3 nested KVM PASS cycles=3` and exit status zero.

The post-review rerun of `60003ec` added an exact `/sys/devices/system/cpu/online == 0-1` check
before suspend and after every resume. The outer harness also required exactly three
`CPU 1 is now offline` and three `CPU1 is up` records. All three cycles passed those gates, the
trusted-loader positive/negative marker checks, nested KVM, and native EFI-variable checks.

### Corrected Windows S4 and root-cause isolation

The S4 verifier now requires an in-memory nonce created before `SetSuspendState` to survive in the
same PowerShell process. The earlier apparent trusted PASS was a false positive: a scheduled startup
script saw the persisted phase file after a normal fallback boot. That result is withdrawn.

The corrected direct QEMU/KVM control restored the original process in 17.123 seconds. Its disk
SHA-256 was `1b1744a3b313072047b1a929d5b441ea8514cc9ae729fb9dd1d8e8842f6a2e6e`;
`SecureBoot=00`, WSL2, zero bugcheck/WHEA and Hyper-V errors, and
`process_continuation=PASS` all passed.

The old trusted runtime with the three variable hooks powered off through S4 with QEMU status zero,
but Windows failed restoration. Kernel-Boot event 16 recorded `FailureStatus=0xC0000001`; the
following bugcheck was `0x7E` with exception `0xC0000005`. The 1,073,900-byte minidump has SHA-256
`e92a56f3b8da6a2197e98c4855124ade5b0ea7e5fbab620c04bf206bb5e8baa4`. A hook-active build without
the MAT edit boot-looped and did not reach the WSL marker within 120 seconds, so it produced no S4
result. Keeping the MAT edit but omitting the hooks restored the original process in 15.92 seconds,
and a no-overlay runtime control did so in 17.668 seconds. The measured S4 differentiator is
therefore installation of the variable hooks; the evidence does not identify a specific hook or
Windows restoration check as the internal cause.

The final trusted direct-chainload run is retained locally at
`/tmp/thin-hv-windows-s4-direct-chainload.Fyv1VP`. QEMU powered off cleanly, cold-started with the
same qcow2, OVMF variable store, and TPM state, and exited zero after restoration. The original
process resumed in 18.631 seconds with:

```text
thin-hv: windows hibernate disk_persist=1 sha256=55fb6791b8ff592a00291ec24d865f8029a088bfe67b52d8980a8bf45801aa9a
thin-hv: windows hibernate firmware_variable=1 SecureBoot=00
thin-hv: windows hibernate wsl2_after=PASS
thin-hv: windows hibernate events bugcheck_whea=0 hyperv_errors=0
thin-hv: windows hibernate process_continuation=PASS
thin-hv: windows hibernate PASS state=S4 guest_resume=1
```

`qemu-img check` reported no errors. This makes trusted Linux S3 and no-hook Windows S4
conditional-GO results for the tested QEMU/KVM configuration. The old Windows hook configuration,
the direct-VMCS backend, physical hardware, and bare-metal suspend remain NO-GO for daily use.

After the trusted `StartImage` lifetime fix in `60003ec`, a fresh child qcow2 and copied OVMF/TPM
state reran the complete test at `/tmp/thin-hv-windows-s4-lifetime.uwD5rQ`. QEMU again powered off
for S4 and cold-started. The original PowerShell process resumed in 18.496 seconds; disk persistence
SHA-256 was `4a8d4e761367309b78d6a1c7bf1369053026430f6ac683f9d29e894a47422366`,
`SecureBoot=00`, WSL2, zero bugcheck/WHEA and Hyper-V errors, and
`process_continuation=PASS` all passed. The harness and a separate `qemu-img check` both exited
zero.

## 2026-09-01/02 guest-location and repeatable-soak follow-up

### Same-ESP guest selection

Commit `d4d8383` changed the candidate order to parent-device `GUESTX64.EFI`, parent-device
`bootmgfw.efi`, then `bootmgfw.efi` on other filesystems. The first non-missing load error is still
fatal. This adds the same-ESP Windows path without weakening error handling or adding a parser.

Commit `d2ee9c3` made `cargo xrun x86 --release` run three live trusted QEMU/KVM smokes after the
direct smoke. All passed:

| `X86_UEFI_GUEST_LOCATION` | Staged paths | Required profile | Result |
| --- | --- | ---: | --- |
| `guest` | `EFI/BOOT/GUESTX64.EFI` | 2 | PASS |
| `windows` | `EFI/Microsoft/Boot/bootmgfw.efi` on the loader ESP | 1 | PASS |
| `both` | both parent-device paths | 2 | PASS; `GUESTX64.EFI` precedence |

Each trusted run required `resident_runtime=0`, native-variable and guest PASS markers, and rejected
runtime-monitor, variable-overlay, and direct `VMLAUNCH` markers. The `windows` fixture uses the
small UEFI payload at the installed-Windows path; a complete installed Windows same-ESP boot is not
claimed. The normal Windows harness continues to cover the other-filesystem fallback.

### Repeatable Linux runner

Commit `e40eb29` added an executable repository-owned runner:

```sh
nix develop --accept-flake-config --command scripts/x86_64/run-linux-soak-test.sh
```

It builds a dedicated UKI, creates a fresh 192 MiB data disk, and boots the trusted two-vCPU Linux
L1 twice around a guest reboot. Every boot runs two CPU/memory SHA-256 workers, restricted usernet,
and repeated real `/dev/kvm` VM/vCPU creation and `KVM_RUN`; the data disk must retain a stamped
128 MiB payload into phase 2 before clean S5 poweroff. The non-zero stamp prevents a dropped write
from matching a fresh sparse all-zero disk. Defaults are 40 hash rounds per worker, 1,000 L2 probes
per boot, and a 900-second timeout. Longer runs set `LINUX_SOAK_HASH_ROUNDS`,
`LINUX_SOAK_L2_PROBES`, and `LINUX_SOAK_TIMEOUT_SECONDS`.

The 2026-09-01/02 long run used the following exact load:

```sh
nix develop --accept-flake-config --command env \
  LINUX_SOAK_HASH_ROUNDS=12000 \
  LINUX_SOAK_L2_PROBES=120000 \
  LINUX_SOAK_TIMEOUT_SECONDS=4800 \
  scripts/x86_64/run-linux-soak-test.sh
```

One QEMU process covered two trusted profile-2 boot epochs. Each boot completed two workers and
24,000 hashes, 120,000 real KVM VM/vCPU/run probes, restricted-usernet ping, and the expected phase
marker. Phase 1 wrote the stamped 128 MiB disk payload and requested reboot; phase 2 reread SHA-256
`de23f42cf7b86dd9833320e382de3855fb09dfa2be4c1430ee24caab9393f875`. The two L2 loops ended at
guest uptimes 2,057.06 and 1,912.29 seconds. The final marker, S5 request, kernel `Power down`, and
host harness all exited successfully; no soak FAIL, kernel panic, Oops, or BUG marker appeared.
The preserved serial and QEMU logs have SHA-256
`32d194ad126793010b7688101d72f2906918bc7ca6065194f13814b137b5177a` and
`ca11776ce2b9fe273db81177eb2451d1d2d6efec1b47f6082568d0dbd99d3a27`.

A fail-closed review then found that the generic smoke runner could send HMP `quit` after the final
PASS instead of requiring QEMU's natural poweroff, and did not reject an explicit guest FAIL before
that PASS. Commit `e965f0f` makes both checks opt-in and enables them for this soak. A negative run
that treated the early `nested KVM ready` marker as failure exited nonzero in 2.747 seconds. A
post-fix two-boot run with one hash round and one L2 probe per boot reached S5 and exited zero
naturally; its serial and QEMU log SHA-256 values are
`3b28f2669304db60372e68d13196130af0ba1bd266eebc096a3f9ad127d9eb9e` and
`e4125b7c8ea5517dc1d173cd52072ca4770487acbb99231ed0f651795269daf5`.

### Repeatable Windows runner and 60-minute result

Commit `2d3cc3a` added `trusted-kvm-wsl-soak` and its static self-check:

```sh
nix develop --accept-flake-config --command \
  scripts/x86_64/windows/windows-test.sh check-wsl-soak

nix develop --accept-flake-config --command env \
  WINDOWS_TEST_DIR=/tmp/thin-hv-windows-final-soak.DN5iGm \
  WINDOWS_CPU=host,+vmx,-hypervisor,kvm=off \
  WINDOWS_MEMORY=4G \
  WINDOWS_VNC=127.0.0.1:9 \
  WINDOWS_DAILY_SOAK_MINUTES=60 \
  WINDOWS_DAILY_SOAK_ROUNDS=2 \
  WINDOWS_DAILY_SOAK_TIMEOUT_SECONDS=7200 \
  WINDOWS_DAILY_SOAK_EXTERNAL_URL=https://raw.githubusercontent.com/torvalds/linux/v7.1/README \
  scripts/x86_64/windows/windows-test.sh trusted-kvm-wsl-soak
```

`WINDOWS_TEST_DIR` was an ignored private qcow2 child with its own complete OVMF variable store and
TPM state, prepared from the immutable Hyper-V/WSL2 baseline. The path records this run exactly;
another machine must point it at an equivalently prepared private directory.

The normal soak defaults are at least 60 minutes and at least two rounds. Each round checks the
Hyper-V feature, `HypervisorPresent`, VMMS, randomized 256 MiB Windows memory, a flushed and reread
128 MiB disk file, 64 MiB TCP loopback, WSL2 CPU data, and a 64 MiB WSL2 memory hash. An optional
`WINDOWS_DAILY_SOAK_EXTERNAL_URL` adds a public HTTPS object fetched independently by Windows and
WSL2; the verifier rejects credentials, query, and fragment and requires equal SHA-256 values.

The completed run above used a 60-minute target, at least two rounds, and the versioned Linux v7.1
README as its optional external probe. Phase 1 took 3,887 ms; after the forced Windows reboot,
phase 2 took 8,351 ms. Work continued until both gates held, completing 459 rounds in 3,606,549 ms.
Both external fetches matched SHA-256
`2844b0b2cafe22741724c4fdda79b1259de48743f12ad40ce408adb1ef00ceda`. The final state reported
`reboot=1`, `disk_persist=1`, `bugcheck_whea=0`, and `hyperv_errors=0`.

The host-generated run ID `7f422ef6-50b8-422e-90ac-a185f8ad3831` and media stamp
`fb89e4d428c24602b1fb89e56686e38ca63fbd04ed681cfbf1a63275a3785168` were present in the staged
media, persisted phase state, and exact final PASS marker, so a stale marker from another invocation
could not satisfy the run. COM1 recorded exactly two trusted profile-1 boot epochs. Windows shut
down within the 120-second gate, QEMU and the host harness returned zero, and independent
`qemu-img check` runs found no errors in either the work image or its immutable baseline. The
baseline image, OVMF variables, and TPM state retained their pre-run SHA-256 values. No Windows
QEMU or `swtpm` process remained afterward.

The preserved desktop, firmware-serial, and QEMU logs have SHA-256
`2314ddad3c89115c70923f33de7f62c0a6ff5c8598c88807b6fa8e9016e1c023`,
`edfac1b1788c1eefe172ed1c9aa2abaf523cbf00cf2763b5ae854ea4c7d686c6`, and
`5ee4dc4923860d8e76e05e8e88f2a046e4df68a6539accf1f50168bb7912b6fb`. This is a repeatable
one-hour synthetic two-boot soak, not multi-hour, interactive GUI, audio, USB/GPU, passthrough, or
general network stability evidence.

### Hardened Windows S4 rerun

Commit `f186a8a` made S4 event queries fail closed, included WHEA warnings, made the WSL probe stop
on its first failing command, delayed the Run-dialog launch, and required clean post-resume
shutdown, QEMU status zero, and `qemu-img check`. The first live rerun exposed that procfs reports
`/proc/cpuinfo` size zero even when populated; `b6b16ea` replaced the invalid size test with a
content grep followed by SHA-256. The next rerun proved the new shutdown gate: an ACPI power button
can re-hibernate an S4-enabled guest instead of shutting it down. Commit `e149051` therefore has the
verified guest request S5 after emitting its final markers while the host waits for natural exit.

After revalidating WSL readiness for the staged media, the final command was:

```sh
nix develop --accept-flake-config --command env \
  WINDOWS_TEST_DIR=/tmp/thin-hv-windows-final-soak.DN5iGm \
  WINDOWS_CPU=host,+vmx,-hypervisor,kvm=off \
  WINDOWS_MEMORY=4G \
  WINDOWS_VNC=127.0.0.1:9 \
  WINDOWS_HYPERV_TIMEOUT_SECONDS=1200 \
  scripts/x86_64/windows/windows-test.sh trusted-kvm-s4
```

The request-side QEMU powered off through S4 with status zero, and the cold-started QEMU restored
the original guest PowerShell process and its in-memory nonce in 17.371 seconds. The 16 MiB file
retained SHA-256 `c99fec6347e6eb302466c991afd4e9f79b3ee14451a662e15cad05e881d00014`;
`SecureBoot=00`, WSL2 before and after resume, zero bugcheck/WHEA and Hyper-V errors, and
`process_continuation=PASS` all passed. COM1 contained exactly two trusted profile-1 epochs. The
verifier then requested S5, the resume-side QEMU and host harness exited zero, an independent
`qemu-img check` found no errors, and no QEMU or `swtpm` remained.

| S4 log | Request SHA-256 | Resume SHA-256 |
| --- | --- | --- |
| desktop serial | `f8144a5a3cacc243f68c5096702e019eb31dd7539977f2db40ecc603804e33d7` | `92a1306a91136dce9befeb5373f8dc0127cadd9578e8a262b0355280354006cf` |
| firmware serial | `27f839a8aba41e97ba8623beb9724e2f476ce6d9545e1e53ca5a725585b30864` | `27f839a8aba41e97ba8623beb9724e2f476ce6d9545e1e53ca5a725585b30864` |
| QEMU | `d20010d8d296255afd3154674288ed7fd833ffaa873b8f00656fb7535cc64e8a` | `e4125b7c8ea5517dc1d173cd52072ca4770487acbb99231ed0f651795269daf5` |

The request and resume sides are separate QEMU processes/invocations because S4 powers the VM off.
“Same process” here means the original in-guest PowerShell process resumed; a cold guest boot or
startup task cannot reconstruct its nonce.

## Claude Opus daily-use review

Claude Code returned three authenticated `claude-opus-5` reviews at maximum effort. The third
completed review used only read/search tools after the user explicitly authorized sending the
private source and documentation. The account is a USD 20 Claude Pro plan, not an API dollar
budget:

| Review | Turns | CLI list-price estimate (USD) | Prompt SHA-256 | Result SHA-256 |
| --- | ---: | ---: | --- | --- |
| initial | 46 | 5.2573025 | `efae1a8ec70c4c8d912a6758ad638a14fa2a90fb0e1ca9d194ff88d70b3b4821` | `647839a2ccf31daa6163b4008f2d5a264f74e6b2c58472aca65de2f6cf71ad8d` |
| re-review | 39 | 4.312761 | `a0bae01d4add094e0ada30d55cc284e7593924bd0277ab210e48053885bb4aa1` | `b8e5bf718fdd5af69cb13c11a2263f7f8ad69de01cfb6c99e0d000d8266de767` |
| final daily-driver review | 37 | 4.528085 | `ec71aba9e582b2999089011354f4091283e7447fc6853cab2104f9ff5baaf3b3` | `a4fb71e5c61e0f5a55623be53acc8c7a588948e16576206d9728d002b935da50` |
| quota-aborted fourth pass | 39 | 4.610336 | `089efcea280132b960c005e93dc515f562a7fec5a4b5c3a1f1627e98d2cea411` | `11ab80f36e166ab6f2d7884bc26a7af2ec59fcb1782f8bed7e8b0af417f3c6d3` |

The four CLI attempts total a USD 18.7084845 list-price estimate; that is not charged plan spend or
remaining capacity. The fourth pass produced 50,015 output tokens but hit the current-session limit
before returning a review: `terminal_reason=api_error`, result `You've hit your session limit`, and
JSON SHA-256 `715e73b01affdcf26a27d0b843ec375706017714c17bc34132598a2ff753d948`.
It therefore supplies neither a new finding nor a GO verdict. Refreshed Claude `/usage` reported the
current session at 100% until 2026-09-02 01:59 Asia/Tokyo and the all-model week at 12% until
2026-09-03 18:59; usage credits were off. The second review correctly required proof that S4 resumed
the original process; implementing that check exposed the false positive above. At review time its
verdict was conditional-GO for trusted Linux and Windows, unmeasured for host suspend, and NO-GO
for physical/bare-metal and direct-VMCS daily use. The old-hook Windows verdict was superseded by
the corrected failure. The final review examined the no-hook direct-chainload configuration and
gave the same conditional-GO/NO-GO boundary.

Accepted and implemented findings:

* `VAR-001`: `BootCurrent` now passes through firmware (`93d556e`); synthesizing a chainload value
  was rejected because it would misreport the firmware-selected entry.
* `NEW-RESUME-001`: EFI Runtime Services are exercised after every Linux S3 resume (`e4886e8` and
  the final native-variable follow-up).
* `NEW-S4-002` and `NEW-S4-003`: the harness states its S3 policy explicitly and requires the
  same-process continuation nonce (`0b695c8`).
* `TEST-001`: smoke completion and timeout handling were tightened, and the dedicated S3 runner was
  added. Stale documentation findings `NEW-DOC-005/006` are corrected here.

Partially valid findings retained as explicit limits:

* `BOOT-001`: source-based guest selection is hardcoded, but separate VM/ESP configurations already
  select the daily OS. A full `EFI_LOAD_OPTION` parser would enlarge the TCB and is not yet needed.
* `MAT-001`: the direct runtime allocation remains RWX. VBS/HVCI and bare-metal use remain NO-GO
  until code and data permissions are split; the final trusted path has no resident runtime.
* `SECBOOT-001`: the artifacts are unsigned and Secure Boot was disabled. The stronger claim that
  this always forces BitLocker recovery was not supported.
* `NEW-CI-007`: the suspend tests are dedicated scripts rather than `cargo xtest` plan entries.
* `NEW-SMOKE-004`: HMP reply pairing has a latent race if unrelated monitor traffic is introduced;
  no such failure was observed in these single-client runs.

Findings judged invalid or not applicable to the trusted path:

* `SUSPEND-001`, `HANG-001`, `EPT-001`, and `RELOC-001` assume project-owned VMX/EPT/resident state.
  The trusted path has none; these are direct-VMCS research concerns.
* `SUSPEND-002` broadly predicted unstable runtime placement. QEMU measurements were stable; only
  physical firmware placement remains unknown.
* `TIME-001` proposed an incomplete Hyper-V enlightenment set. Testing the dependency-complete
  candidate made hibernation unavailable, so it was not adopted.
* `TEST-002` claimed the UEFI test did not exercise live hooks; the historical payload did call the
  installed Runtime Services table directly.
* Requiring project ACPI suspend emulation is not applicable: the trusted path delegates guest
  power states to QEMU, OVMF, and KVM.
* Synthesizing `BootCurrent`, and the unconditional BitLocker-recovery claim, were rejected for the
  reasons above.

### Final-review finding disposition

Accepted and implemented in `60003ec`:

* `LIFETIME-001`: a returning UEFI application/OS loader is already unloaded by firmware. The
  trusted path no longer calls `UnloadImage` on that invalid handle and now frees non-null
  `ExitData`, as required by [UEFI 2.11](https://uefi.org/specs/UEFI/2.11/04_EFI_System_Table.html).
* `HARNESS-001/002`: every trusted smoke now requires the direct-chainload marker and rejects
  direct/runtime/overlay markers. Linux additionally checks the live CPU-online mask after every
  resume, while the harness counts the kernel's CPU1 offline/up events.
* The valid part of `TRUST-001`: the final trusted PE already contained zero decoded VMX
  instructions, but that fact had no automatic regression gate. `xbuild` now enforces it.
* The valid part of `CI-001`: `x86_64_hal`, `nested_vmx`, `uefi_variable_overlay`, and the trusted
  loader feature are now host-test plan entries. The five selected x86 entries passed 30 tests in
  total, and `xtask` passed 17 tests.
* `DOC-001/002` and the valid part of `ARTIFACT-001`: monitor default staging and current test counts
  are corrected, and builds prune the two known obsolete EFI output names.
* `BOOT-002` was initially retained as a boundary, then superseded by `d4d8383` and `d2ee9c3`:
  parent-device Windows selection and live `guest`/`windows`/`both` path smokes now pass. Only a
  complete installed-Windows same-ESP boot remains unmeasured.

Partially valid items retained as explicit boundaries:

* `WIN-S3-001`: Windows S3 is explicitly disabled and remains untested; the measured Windows power
  state is S4 only.
* `SECBOOT-002`: artifacts are unsigned, Secure Boot and BitLocker remain untested, and recovery
  risk depends on the active PCR profile. Microsoft documents how to inspect Secure Boot integrity
  use and when to suspend/reseal protection in the
  [BitLocker FAQ](https://learn.microsoft.com/en-us/windows/security/operating-system-security/data-protection/bitlocker/faq)
  and [configuration reference](https://learn.microsoft.com/en-us/windows/security/operating-system-security/data-protection/bitlocker/configure).
* `TEST-003`: the trusted feature is now compiled by the plan, but tests of path string constants
  were not added; the live trusted UEFI smoke exercises the actual load/start/error contract.

Final-review claims judged invalid or not applicable:

* `PAYLOAD-001` was rejected. The temporary physical profile-2 key is a negative regression probe:
  if an overlay hook is accidentally reintroduced, the logical `DriverFFFF` lookup exposes it.
  The fixture uses fresh VARS and deletes every scratch key.
* The High-severity/linker-only part of `TRUST-001` was rejected. Normal optimized trusted IR has
  already eliminated the direct functions before linking; only some direct statics existed before
  LLVM optimization. The final-PE build gate addresses the real regression risk without a
  large mechanical source split. The later `976b2e9` follow-up nevertheless made the separation
  structural with a small module split and retained the final-PE gate.
* `HARNESS-001` overstated the normal fixture gap: it already required the trusted return and native
  variable markers. Only custom long-running guests lacked the loader-side positive/negative gate.
* Claude's proposed x86 `uefi` plan row is incompatible with the current `xtask` grammar, whose
  `uefi` category always builds the AArch64 UEFI integration target. Adding a knowingly invalid row
  was rejected.
* Reapplying direct-VMX/EPT/MAT/runtime, per-pCPU direct-state, project ACPI ownership, or
  monitor-enforced variable-isolation findings to the trusted path was rejected: that path has no
  project VMX layer or resident monitor. The incomplete Hyper-V `TIME-001` proposal, unconditional
  BitLocker recovery, and direct-path crash/overhead conclusions are likewise not evidence against
  the trusted configuration.

## Limits

Six Windows pairs and six Linux pairs still leave broad end-to-end bounds, and the Linux runner's
50 ms polling cannot resolve single-digit-millisecond changes. Windows media, qcow2 images, TPM
state, raw logs, and dumps are not distributable repository fixtures. The historical 33.576-second
Linux and 265.595-second Windows soaks and the repeatable two-boot runners demonstrate bounded
stability, not that either OS cannot fail during indefinite daily use. Physical x86 hardware,
VBS/HVCI, Windows Sandbox, SMP direct-VMCS, and a complete non-interactive cargo xtest remain
unproven. Physical-host suspend, bare-metal resume, interactive GUI use, audio, USB, general or
sustained external networking, modern standby, and multi-hour or long repeated S3/S4 use are also
unproven. The 2026-09-01/02 Windows run proves one bounded HTTPS fetch from both Windows and WSL2;
Linux exercised only restricted QEMU usernet.
