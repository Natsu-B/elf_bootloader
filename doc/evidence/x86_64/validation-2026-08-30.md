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

## Limits

The three valid Windows A/B pairs are still too few for a narrow performance bound and retain an
order-bias caveat. Windows media, qcow2 images, TPM state, raw logs, and dumps are not distributable
repository fixtures. Physical x86 hardware, VBS/HVCI, Windows Sandbox, SMP direct-VMCS, and a
complete non-interactive cargo xtest remain unproven.
