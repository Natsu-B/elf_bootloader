# x86_64 validation evidence - 2026-08-30

This manifest ties the compact claims in the architecture document to exact commands, hashes, and
boot markers. Windows disks, dumps, and raw serial logs are intentionally not committed.

- Validated code HEAD: 2e4af630c7566ee070aaa1276ac80ce087f5e12a
- Upstream refactor merged at: 065a40125daed43ff7e19ff0b591c8e4bb09f30c
- Branch: feat/x86-thin-monitor
- Later non-runtime build-input commit: a86492c12221b413b481030ad7b09b060313a273
  (adds curl to the Nix development shell)
- Trusted startup optimization follow-up: 63c5d2b48fe5a377b02b701a1cf479644f9f95f3

The original sections below describe the validated code HEAD. The separately labelled startup
optimization follow-up records its subsequent code, artifacts, and validation.

## Trust boundary

The trusted-outer-kvm feature installs the same profile-specific UEFI Runtime Services overlay and
then calls StartImage. It executes no project VMXON, VMLAUNCH, VMRESUME, or VM-exit reflection
loop. CPU microcode, Linux KVM, QEMU, OVMF, Windows, and Hyper-V are therefore in the trusted
computing base. The project-owned active path is the UEFI loader, runtime handoff, and three
variable hooks.

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

## Release artifacts and active-code proxy

After cargo xbuild x86 --release:

| Artifact | PE bytes | .text bytes | Decoded VMX sites | SHA-256 |
| --- | ---: | ---: | ---: | --- |
| x86-uefi-monitor.efi | 81,920 | 69,801 | 303 | 615d60a7cedb7232c2df6470cac7091a59537aaab36b48606ca2588ba9b3cc94 |
| x86-uefi-kvm-monitor.efi | 24,576 | 18,937 | 0 | db8aa1e709fe7d4abd9cecb5a7e83f68a70a5beb50cc477719486be0423b7ba5 |
| x86-uefi-loader.efi | 81,920 | - | - | 0585343580b84deceb201abe932abb52c165e485ebaeabc3e31e211fbd0d60cb |
| x86-uefi-kvm-loader.efi | 24,576 | - | - | f73e538f8e89e50bd618b2789b60786943b0b975228486eba281cd0edae6c941 |

The trusted runtime PE is 70.0% smaller and its .text is 72.9% smaller. This is an active-code proxy,
not a formal TCB proof. VMX sites were counted from GNU objdump -d output for vmcall, vmclear,
vmlaunch, vmresume, vmptrld, vmptrst, vmread, vmwrite, vmxoff, vmxon, invept, invvpid, and vmfunc.
LLVM objdump does not decode the same byte sequences identically; use GNU objdump from the Nix
shell for this measurement.

## Current-HEAD build and automated checks

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

## Same-baseline Windows Hyper-V A/B

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

## Coherent trusted WSL2 run

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

## Trusted startup optimization follow-up

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

## Protocol-cache and OS-specific follow-up

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

## Nested execution and bounded stability soak

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

This test covers a real WSL2 L2 utility VM but remains a bounded synthetic soak. Its Windows
network load was loopback only; external network traffic, interactive GUI applications, audio,
USB, suspend/resume, dedicated WSL event-channel scanning, and multi-hour operation remain
untested.

## Limits

Six Windows pairs and six Linux pairs still leave broad end-to-end bounds, and the Linux runner's
50 ms polling cannot resolve single-digit-millisecond changes. Windows media, qcow2 images, TPM
state, raw logs, and dumps are not distributable repository fixtures. The 33.576-second Linux and
265.595-second Windows soaks demonstrate bounded stability, not that either OS cannot fail during
indefinite daily use. Physical x86 hardware, VBS/HVCI, Windows Sandbox, SMP direct-VMCS, and a
complete non-interactive cargo xtest remain unproven.
