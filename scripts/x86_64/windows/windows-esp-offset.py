#!/usr/bin/env python3
"""Select one ESP from read-only sfdisk JSON for a disposable QEMU fixture.

Only byte sizes/offsets are emitted. No partition identifiers, files, BCD or
activation data are changed. This is not physical-machine disk tooling.
"""

import json
import sys
import unittest
import uuid


ESP = "c12a7328-f81f-11d2-ba4b-00a0c93ec93b"
LIMIT = (1 << 63) - 1


def integer(value, minimum=0):
    if type(value) is not int or not minimum <= value <= LIMIT:
        raise ValueError("invalid partition integer")
    return value


def select_extent(document, image_bytes):
    table = document["partitiontable"]
    sector = integer(table["sectorsize"], 1)
    image_bytes = integer(image_bytes, 1)
    if table["label"] != "gpt" or table["unit"] != "sectors" or sector not in (512, 4096):
        raise ValueError("unsupported partition layout")
    first, last = integer(table["firstlba"], 2), integer(table["lastlba"], 2)
    if image_bytes % sector or not first <= last < image_bytes // sector - 1:
        raise ValueError("GPT usable extent outside image")
    partitions = table["partitions"]
    if not isinstance(partitions, list) or not 1 <= len(partitions) <= 128:
        raise ValueError("partition count")
    ranges, candidates = [], []
    for partition in partitions:
        start, count = integer(partition["start"], first), integer(partition["size"], 1)
        end = start + count
        if end > last + 1 or end * sector > image_bytes:
            raise ValueError("partition extent outside usable image")
        kind = str(uuid.UUID(partition["type"]))
        ranges.append((start, end))
        if kind == ESP:
            candidates.append((start * sector, count * sector))
    ordered = sorted(ranges)
    if any(left[1] > right[0] for left, right in zip(ordered, ordered[1:])):
        raise ValueError("overlapping partitions")
    if len(candidates) != 1:
        raise ValueError("expected exactly one Windows test ESP")
    return candidates[0]


def select_offset(document, image_bytes):
    return select_extent(document, image_bytes)[0]


def virtual_size(document):
    size = integer(document["virtual-size"], 2 * 1048576)
    if document["format"] != "qcow2" or size % 512:
        raise ValueError("unsupported QEMU image geometry")
    return size


def unique_object(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError("duplicate JSON member")
        result[key] = value
    return result


class OffsetTests(unittest.TestCase):
    def fixture(self):
        return {"partitiontable": {"label": "gpt", "unit": "sectors", "sectorsize": 512,
                "firstlba": 34, "lastlba": 4094, "partitions": [
                    {"start": 2048, "size": 1024, "type": ESP}]}}

    def test_selection_is_unique_bounded_and_not_a_fixed_sector_offset(self):
        for sector in (512, 4096):
            for start in (34, 128, 2048):
                doc = self.fixture()
                doc["partitiontable"]["sectorsize"] = sector
                doc["partitiontable"]["partitions"][0]["start"] = start
                self.assertEqual(select_offset(doc, 4096 * sector), start * sector)
                self.assertEqual(select_extent(doc, 4096 * sector), (start * sector, 1024 * sector))
        for key, bad in (("start", True), ("start", -1), ("start", LIMIT),
                         ("size", 0), ("size", LIMIT), ("type", "not-a-guid")):
            doc = self.fixture()
            doc["partitiontable"]["partitions"][0][key] = bad
            with self.assertRaises(ValueError):
                select_offset(doc, 4096 * 512)
        for key, bad in (("label", "dos"), ("unit", "bytes"), ("sectorsize", 1024),
                         ("firstlba", 4095), ("lastlba", 4096), ("partitions", [])):
            doc = self.fixture()
            doc["partitiontable"][key] = bad
            with self.assertRaises(ValueError):
                select_offset(doc, 4096 * 512)
        for start, kind in ((2048, ESP), (3500, ESP), (2100, "00000000-0000-0000-0000-000000000001")):
            doc = self.fixture()
            doc["partitiontable"]["partitions"].append({"start": start, "size": 1, "type": kind})
            with self.assertRaises(ValueError):
                select_offset(doc, 4096 * 512)
        with self.assertRaises(ValueError):
            select_offset(self.fixture(), 4096 * 512 - 1)
        with self.assertRaises(ValueError):
            json.loads('{"a":1,"a":2}', object_pairs_hook=unique_object)

    def test_virtual_geometry_is_explicit_aligned_and_arithmetic_bounded(self):
        for size in (2 * 1048576, 80 << 30, LIMIT & ~511):
            self.assertEqual(virtual_size({"format": "qcow2", "virtual-size": size}), size)
        for size in (True, -1, 0, 1048576, (80 << 30) + 1, LIMIT + 1):
            with self.assertRaises(ValueError):
                virtual_size({"format": "qcow2", "virtual-size": size})
        with self.assertRaises(ValueError):
            virtual_size({"format": "raw", "virtual-size": 80 << 30})


if __name__ == "__main__":
    if sys.argv[1:] == ["--self-test"]:
        unittest.main(argv=[sys.argv[0]])
    else:
        try:
            args = sys.argv[1:]
            extent = len(args) == 2 and args[0] == "--extent"
            geometry = args == ["--virtual-size"]
            size = args[-1] if args else ""
            if not geometry and not ((len(args) == 1 or extent) and size.isascii() and size.isdecimal()):
                raise ValueError("expected image byte size")
            data = sys.stdin.buffer.read(1048577)
            if len(data) > 1048576:
                raise ValueError("partition JSON too large")
            document = json.loads(data, object_pairs_hook=unique_object)
            if geometry:
                print(virtual_size(document))
            elif extent:
                print(*select_extent(document, int(size)))
            else:
                print(select_offset(document, int(size)))
        except (ValueError, KeyError, TypeError, AttributeError, OSError, RecursionError):
            print("Windows QEMU ESP selection failed: malformed, unsupported or ambiguous layout", file=sys.stderr)
            sys.exit(1)
