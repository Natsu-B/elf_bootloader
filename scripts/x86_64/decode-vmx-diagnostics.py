#!/usr/bin/env python3
"""Validate and decode only the BSP QEMU prototype's fixed VM-exit counters.

The address command accepts one publication inside reserved CPU-owned storage
(or the resident image for explicitly legacy publications),
never a free-form physical address. The decode command reads exactly 176 bytes;
it does not inspect guest, firmware, crash, or licensing data.
"""

import json
import os
import re
import stat
import struct
import sys
import unittest


SIZE = 176
U64_MAX = (1 << 64) - 1
PROTOTYPE_LIMIT = 1 << 47  # current private HOST_CR3 low-canonical address ceiling
MAX_LOG_BYTES = 64 << 20
MAX_LOG_LINE = 4096
BACKEND = "thin-hv: backend=direct-vmx role=project-l0"
IMAGE = re.compile(r"thin-hv: runtime image base=0x([0-9a-f]{16}) end=0x([0-9a-f]{16})")
BLOCK = re.compile(r"thin-hv: monitor block=0x([0-9a-f]{16}) end=0x([0-9a-f]{16})")
PUBLICATION = re.compile(
    r"thin-hv: vmx diagnostics address=0x([0-9a-f]{16}) "
    r"size=176 version=2 scope=bsp-only environment=qemu-prototype( storage=cpu-runtime)?"
)
COUNTERS = (
    "l1_exits", "direct_entry_attempts", "observed_l2_entries",
    "reflected_l2_exits", "l0_only_handled_exits", "external_interrupt_exits",
    "interrupt_window_exits", "invept", "invvpid", "nested_entry_failures",
    "cpuid_exits",
    "vmread_attempts", "vmwrite_attempts", "vmptrld_attempts",
    "reflected_state_writes",
)
PHASES = (
    "initial", "l1-exit", "l0-handled-resume", "direct-entry-prepared",
    "nested-exit", "reflection-complete", "immediate-entry-failure",
    "bounded-guest-returned",
)


class InvalidRecord(ValueError):
    """A bounded validation failure whose text contains no log contents."""


def publication(lines):
    """Return the one validated record address and its owning allocation bounds."""
    backend_count = 0
    image = None
    block = None
    owner = None
    address = None
    for raw in lines:
        line = raw.rstrip("\r\n")
        if line.startswith("thin-hv: backend="):
            # Both the bootstrap and the resident PE enter the same main().
            if line != BACKEND or backend_count >= 2 or image is not None:
                raise InvalidRecord("missing, repeated, or foreign Direct-VMX backend")
            backend_count += 1
        elif line.startswith("thin-hv: runtime image "):
            match = IMAGE.fullmatch(line)
            if match is None or backend_count != 2 or image is not None or address is not None:
                raise InvalidRecord("invalid or repeated resident image bounds")
            image = tuple(int(value, 16) for value in match.groups())
            if not (0 < image[0] < image[1] <= PROTOTYPE_LIMIT):
                raise InvalidRecord("resident image outside the QEMU prototype map")
        elif line.startswith("thin-hv: monitor block="):
            match = BLOCK.fullmatch(line)
            if match is None or image is None or block is not None:
                raise InvalidRecord("invalid or unordered CPU allocation")
            block = tuple(int(value, 16) for value in match.groups())
            if (not (0 < block[0] < block[1] <= PROTOTYPE_LIMIT)
                    or any(value % 4096 for value in block)
                    or block[1] - block[0] > 16 << 20
                    or (block[0] < image[1] and image[0] < block[1])):
                raise InvalidRecord("invalid or overlapping CPU allocation bounds")
        elif line.startswith("thin-hv: vmx diagnostics "):
            match = PUBLICATION.fullmatch(line)
            if match is None or backend_count != 2 or image is None or address is not None:
                raise InvalidRecord("invalid, unordered, or repeated diagnostics publication")
            address = int(match[1], 16)
            owner = block if match[2] is not None else image
            if owner is None or address % 8 or not (owner[0] <= address and address + SIZE <= owner[1]):
                raise InvalidRecord("diagnostics record is not wholly inside its declared owner")
        elif "thin-hv: trusted outer KVM" in line:
            raise InvalidRecord("reference backend cannot publish Direct-VMX diagnostics")
    if backend_count != 2 or image is None or address is None:
        raise InvalidRecord("incomplete Direct-VMX diagnostics publication")
    return address, owner


def regular_file(path):
    """Open a regular, non-symlink input without blocking on a special file."""
    descriptor = os.open(path, os.O_RDONLY | os.O_NONBLOCK | os.O_NOFOLLOW)
    try:
        if not stat.S_ISREG(os.fstat(descriptor).st_mode):
            raise InvalidRecord("diagnostics input must be a regular file")
        return os.fdopen(descriptor, "rb")
    except BaseException:
        os.close(descriptor)
        raise


def published_address(path):
    """Scan a bounded serial log without retaining or printing its contents."""
    with regular_file(path) as source:
        if os.fstat(source.fileno()).st_size > MAX_LOG_BYTES:
            raise InvalidRecord("serial log exceeds diagnostic scan bound")

        def lines():
            total = 0
            while True:
                line = source.readline(MAX_LOG_LINE + 1)
                if not line:
                    return
                total += len(line)
                if len(line) > MAX_LOG_LINE or total > MAX_LOG_BYTES:
                    raise InvalidRecord("serial log exceeds diagnostic scan bound")
                yield line.decode("utf-8", errors="replace")

        return publication(lines())


def decode_record(data, address):
    """Decode the immutable snapshot; never accept a torn or exhausted sequence."""
    if len(data) != SIZE or data[:8] != b"THVSTAT1":
        raise InvalidRecord("incorrect diagnostics size or magic")
    words = struct.unpack("<22Q", data)
    if words[1] != 2 or words[2] != SIZE or words[4] != 1:
        raise InvalidRecord("unsupported diagnostics version, size, or scope")
    if words[3] & 1 or words[3] == U64_MAX:
        raise InvalidRecord("torn or exhausted diagnostics sequence")
    if words[20] >= len(PHASES) or (words[21] > 0xFFFFFFFF and words[21] != U64_MAX):
        raise InvalidRecord("invalid diagnostics phase or VM-exit reason")
    return {
        "schema": "thin-hv.vmx-diagnostics.v2",
        "backend": "direct-vmx",
        "role": "project-l0",
        "environment": "QEMU/KVM",
        "scope": "bsp-only-qemu-prototype",
        "address": f"0x{address:016x}",
        "size": SIZE,
        "sequence": words[3],
        "counters": dict(zip(COUNTERS, words[5:20])),
        "last_phase": {"value": words[20], "name": PHASES[words[20]]},
        "last_reason": None if words[21] == U64_MAX else words[21],
    }


class DecoderTests(unittest.TestCase):
    """Pure host tests; no firmware, guest, QEMU, or licensing access."""

    def setUp(self):
        self.lines = [
            BACKEND,
            BACKEND,
            "thin-hv: runtime image base=0x0000000000100000 end=0x0000000000101000",
            "thin-hv: vmx diagnostics address=0x0000000000100040 "
            "size=176 version=2 scope=bsp-only environment=qemu-prototype",
        ]
        self.words = [int.from_bytes(b"THVSTAT1", "little"), 2, SIZE, 2, 1]
        self.words += list(range(15)) + [4, 48]

    def record(self, words=None):
        return struct.pack("<22Q", *(self.words if words is None else words))

    def test_valid_publication_and_crlf(self):
        expected = (0x100040, (0x100000, 0x101000))
        self.assertEqual(publication(self.lines), expected)
        self.assertEqual(publication(line + "\r\n" for line in self.lines), expected)

    def test_missing_duplicate_and_unordered_publication(self):
        for index in range(4):
            with self.assertRaises(InvalidRecord):
                publication(self.lines[:index] + self.lines[index + 1:])
            with self.assertRaises(InvalidRecord):
                publication(self.lines[:index] + [self.lines[index]] + self.lines[index:])
        with self.assertRaises(InvalidRecord):
            publication(self.lines[:2] + [self.lines[3], self.lines[2]])

    def test_foreign_backend_and_reference_contamination(self):
        for backend in ("outer-kvm role=reference", "physical-chainload project_vmx=0"):
            with self.assertRaises(InvalidRecord):
                publication(["thin-hv: backend=" + backend] + self.lines[1:])
        with self.assertRaises(InvalidRecord):
            publication(self.lines + ["thin-hv: trusted outer KVM guest PASS"])

    def test_address_alignment_and_image_boundaries(self):
        for address in (0, 0xFFFF8, 0x100041, 0x100F58, U64_MAX):
            invalid = self.lines[:3] + [self.lines[3].replace("0000000000100040", f"{address:016x}")]
            with self.assertRaises(InvalidRecord):
                publication(invalid)
        valid = self.lines[:3] + [self.lines[3].replace("0000000000100040", "0000000000100f50")]
        self.assertEqual(publication(valid)[0], 0x100F50)

    def test_prototype_bounds_and_publication_metadata(self):
        for line in (
            self.lines[2].replace("0000000000101000", "0000800000000001"),
            self.lines[2].replace("0000000000101000", "0000000000100000"),
        ):
            with self.assertRaises(InvalidRecord):
                publication(self.lines[:2] + [line, self.lines[3]])
        for field, replacement in (("size=176", "size=144"), ("version=2", "version=1"),
                                   ("scope=bsp-only", "scope=smp")):
            with self.assertRaises(InvalidRecord):
                publication(self.lines[:3] + [self.lines[3].replace(field, replacement)])

    def test_cpu_runtime_publication_owner_and_high_memory(self):
        block = "thin-hv: monitor block=0x0000000200000000 end=0x0000000200002000"
        record = self.lines[3].replace("0000000000100040", "0000000200001f50") + " storage=cpu-runtime"
        valid = self.lines[:3] + [block, record]
        self.assertEqual(publication(valid), (0x200001F50, (0x200000000, 0x200002000)))
        for invalid in (
            self.lines[:3] + [record],
            self.lines[:3] + [record, block],
            self.lines[:3] + [block, block, record],
            self.lines[:3] + [block, self.lines[3] + " storage=cpu-runtime"],
            self.lines[:3] + [block, record.removesuffix(" storage=cpu-runtime")],
            self.lines[:3] + [block, record.replace("0000000200001f50", "0000000200001f58")],
            self.lines[:3] + [block.replace("0000000200000000", "0000000200000001"), record],
            self.lines[:3] + [block.replace("0000000200002000", "0000800000001000"), record],
            self.lines[:3] + [block.replace("0000000200000000", "0000000000100000"), record],
        ):
            with self.assertRaises(InvalidRecord):
                publication(invalid)

    def test_exact_size_magic_and_abi_fields(self):
        for data in (self.record()[:-1], self.record() + b"\0", b"NOTSTAT1" + self.record()[8:]):
            with self.assertRaises(InvalidRecord):
                decode_record(data, 0x100040)
        for index, value in ((1, 1), (2, 144), (4, 2)):
            words = self.words.copy()
            words[index] = value
            with self.assertRaises(InvalidRecord):
                decode_record(self.record(words), 0x100040)

    def test_odd_exhausted_sequence_phase_and_reason(self):
        for index, value in ((3, 1), (3, U64_MAX), (20, 8), (21, 1 << 32)):
            words = self.words.copy()
            words[index] = value
            with self.assertRaises(InvalidRecord):
                decode_record(self.record(words), 0x100040)

    def test_bounded_json_counter_mapping_and_saturation(self):
        self.words[5] = U64_MAX
        self.words[21] = U64_MAX
        result = decode_record(self.record(), 0x100040)
        self.assertEqual(result["counters"]["l1_exits"], U64_MAX)
        self.assertEqual(result["counters"]["cpuid_exits"], 10)
        self.assertEqual(result["counters"]["vmread_attempts"], 11)
        self.assertEqual(result["counters"]["vmwrite_attempts"], 12)
        self.assertEqual(result["counters"]["vmptrld_attempts"], 13)
        self.assertEqual(result["counters"]["reflected_state_writes"], 14)
        self.assertEqual(result["last_phase"]["name"], "nested-exit")
        self.assertIsNone(result["last_reason"])
        self.assertLess(len(json.dumps(result)), 2048)


def main(arguments):
    """Expose only provenance-derived addressing and fixed-record decoding."""
    if arguments == ["--self-test"]:
        unittest.main(argv=[sys.argv[0]])
        return 0
    if len(arguments) not in (2, 3) or arguments[0] not in ("address", "decode"):
        raise InvalidRecord("usage: address LOG | decode LOG RECORD | --self-test")
    if (arguments[0] == "address") != (len(arguments) == 2):
        raise InvalidRecord("incorrect diagnostics command arguments")
    address, _ = published_address(arguments[1])
    if arguments[0] == "address":
        print(f"0x{address:016x}")
    else:
        with regular_file(arguments[2]) as source:
            data = source.read(SIZE + 1)
        print(json.dumps(decode_record(data, address), sort_keys=True, separators=(",", ":")))
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main(sys.argv[1:]))
    except InvalidRecord as error:
        print(f"VMX diagnostics unavailable: {error}", file=sys.stderr)
        sys.exit(1)
    except (OSError, UnicodeError):
        print("VMX diagnostics unavailable: input could not be read safely", file=sys.stderr)
        sys.exit(1)
