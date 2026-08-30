#!/usr/bin/env python3
"""Read-only verification of the direct-VMX Windows 0x133 evidence.

The script exports the qcow2 through a temporary qemu-nbd Unix socket with
`-r`, sends only NBD READ requests, and removes the socket/process on exit.
It never mounts the filesystem and never writes an extracted dump to disk.
"""

import datetime
import hashlib
import os
import socket
import struct
import subprocess
import sys
import tempfile
import time


IMAGE = sys.argv[1]
SECTOR_SIZE = 512
CLUSTER_SIZE = 4096

# Values independently obtained from the GPT and NTFS MFT in this image.
WINDOWS_PARTITION_LBA = 567296
PARTITION_BASE = WINDOWS_PARTITION_LBA * SECTOR_SIZE

# C:\\Windows\\MEMORY.DMP, MFT record 127224, 380473258 bytes.
MEMORY_DMP_SIZE = 380473258
MEMORY_DMP_RUNS = [(92889, 3707751)]  # (clusters, LCN)

# C:\\ProgramData\\Microsoft\\Windows\\WER\\Temp\\
# WER.a4a110f2-2ed4-4857-a3d5-b34d44d66c7a.tmp.dmp,
# MFT record 138536, 197272 bytes.
WER_DMP_SIZE = 197272
WER_DMP_RUNS = [
    (1, 2952174),
    (6, 3601196),
    (3, 3607374),
    (6, 4565575),
    (16, 4572153),
    (16, 4590597),
    (1, 4592880),
]


def recv_exact(sock, size):
    result = bytearray()
    while len(result) < size:
        part = sock.recv(size - len(result))
        if not part:
            raise EOFError(f"short NBD reply: {len(result)}/{size}")
        result += part
    return bytes(result)


with tempfile.TemporaryDirectory(prefix="thin-hv-nbd-ro-") as temp_dir:
    socket_path = os.path.join(temp_dir, "disk.sock")
    process = subprocess.Popen(
        ["qemu-nbd", "-r", "-f", "qcow2", "-k", socket_path, IMAGE],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
    )
    client = None
    try:
        for _ in range(200):
            if os.path.exists(socket_path):
                break
            if process.poll() is not None:
                raise RuntimeError(process.stderr.read().decode(errors="replace"))
            time.sleep(0.02)
        else:
            raise TimeoutError("qemu-nbd socket did not appear")

        client = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        client.connect(socket_path)

        magic, options_magic = struct.unpack(">QQ", recv_exact(client, 16))
        server_flags = struct.unpack(">H", recv_exact(client, 2))[0]
        assert magic == 0x4E42444D41474943
        assert options_magic == 0x49484156454F5054

        # Fixed newstyle + no-zeroes, then NBD_OPT_EXPORT_NAME for the default export.
        client.sendall(struct.pack(">I", 3))
        client.sendall(struct.pack(">QII", 0x49484156454F5054, 1, 0))
        export_size, transmission_flags = struct.unpack(">QH", recv_exact(client, 10))
        print(
            f"NBD export_size={export_size} server_flags=0x{server_flags:x} "
            f"transmission_flags=0x{transmission_flags:x}"
        )

        request_handle = 0

        def read_at(offset, size):
            global request_handle
            output = bytearray()
            while size:
                amount = min(size, 8 * 1024 * 1024)
                request_handle += 1
                client.sendall(
                    struct.pack(
                        ">IHHQQI",
                        0x25609513,  # NBD_REQUEST_MAGIC
                        0,           # request flags
                        0,           # NBD_CMD_READ
                        request_handle,
                        offset,
                        amount,
                    )
                )
                reply_magic, error, reply_handle = struct.unpack(
                    ">IIQ", recv_exact(client, 16)
                )
                assert reply_magic == 0x67446698
                assert error == 0
                assert reply_handle == request_handle
                output += recv_exact(client, amount)
                offset += amount
                size -= amount
            return bytes(output)

        # Recheck the partition and NTFS signatures before trusting the recorded runlists.
        gpt = read_at(SECTOR_SIZE, SECTOR_SIZE)
        ntfs = read_at(PARTITION_BASE, SECTOR_SIZE)
        assert gpt[:8] == b"EFI PART"
        assert ntfs[3:11] == b"NTFS    "
        print(
            f"GPT={gpt[:8]!r} windows_partition_lba={WINDOWS_PARTITION_LBA} "
            f"partition_offset={PARTITION_BASE} NTFS={ntfs[3:11]!r}"
        )

        def stream_runs(runs, real_size):
            remaining = real_size
            for cluster_count, lcn in runs:
                amount = min(remaining, cluster_count * CLUSTER_SIZE)
                offset = PARTITION_BASE + lcn * CLUSTER_SIZE
                while amount:
                    chunk_size = min(amount, 8 * 1024 * 1024)
                    yield read_at(offset, chunk_size)
                    offset += chunk_size
                    amount -= chunk_size
                    remaining -= chunk_size
            assert remaining == 0

        memory_hash = hashlib.sha256()
        memory_header = bytearray()
        for chunk in stream_runs(MEMORY_DMP_RUNS, MEMORY_DMP_SIZE):
            memory_hash.update(chunk)
            if len(memory_header) < 8192:
                memory_header += chunk[: 8192 - len(memory_header)]

        assert memory_header[:8] == b"PAGEDU64"
        major_version = struct.unpack_from("<I", memory_header, 8)[0]
        minor_version = struct.unpack_from("<I", memory_header, 12)[0]
        machine = struct.unpack_from("<I", memory_header, 0x30)[0]
        processors = struct.unpack_from("<I", memory_header, 0x34)[0]
        bugcheck = struct.unpack_from("<I", memory_header, 0x38)[0]
        parameters = [
            struct.unpack_from("<Q", memory_header, offset)[0]
            for offset in (0x40, 0x48, 0x50, 0x58)
        ]
        dump_type = struct.unpack_from("<I", memory_header, 3992)[0]
        system_time_raw = struct.unpack_from("<Q", memory_header, 4008)[0]
        uptime_raw = struct.unpack_from("<Q", memory_header, 4144)[0]
        filetime_epoch = datetime.datetime(1601, 1, 1, tzinfo=datetime.timezone.utc)
        system_time = filetime_epoch + datetime.timedelta(
            microseconds=system_time_raw / 10
        )
        uptime = datetime.timedelta(microseconds=uptime_raw / 10)
        print(
            f"MEMORY.DMP size={MEMORY_DMP_SIZE} sha256={memory_hash.hexdigest()} "
            f"signature={memory_header[:8].decode()} "
            f"version={major_version}.{minor_version} machine=0x{machine:x} "
            f"processors={processors} dump_type={dump_type} "
            f"bugcheck=0x{bugcheck:x} params={tuple(hex(x) for x in parameters)} "
            f"system_time_utc={system_time.isoformat(timespec='microseconds')} "
            f"uptime={uptime}"
        )

        wer_dump = b"".join(stream_runs(WER_DMP_RUNS, WER_DMP_SIZE))
        wer_hash = hashlib.sha256(wer_dump).hexdigest()
        signature, version, stream_count, directory_rva = struct.unpack_from(
            "<4sIII", wer_dump, 0
        )
        timestamp = struct.unpack_from("<I", wer_dump, 20)[0]
        assert signature == b"MDMP"
        directories = {}
        for index in range(stream_count):
            stream_type, size, rva = struct.unpack_from(
                "<III", wer_dump, directory_rva + index * 12
            )
            directories[stream_type] = (rva, size)

        def minidump_string(rva):
            length = struct.unpack_from("<I", wer_dump, rva)[0]
            return wer_dump[rva + 4 : rva + 4 + length].decode(
                "utf-16le", errors="replace"
            )

        modules = []
        module_rva, _ = directories[4]
        module_count = struct.unpack_from("<I", wer_dump, module_rva)[0]
        for index in range(module_count):
            offset = module_rva + 4 + index * 108
            base, image_size, _, _, name_rva = struct.unpack_from(
                "<QIIII", wer_dump, offset
            )
            modules.append((base, image_size, minidump_string(name_rva)))

        exception_rva, _ = directories[6]
        exception_code = struct.unpack_from("<I", wer_dump, exception_rva + 8)[0]
        exception_address = struct.unpack_from("<Q", wer_dump, exception_rva + 24)[0]
        parameter_count = struct.unpack_from("<I", wer_dump, exception_rva + 32)[0]
        exception_parameters = struct.unpack_from(
            "<" + "Q" * min(parameter_count, 15), wer_dump, exception_rva + 40
        )
        fault_modules = [
            (name, exception_address - base)
            for base, image_size, name in modules
            if base <= exception_address < base + image_size
        ]
        print(
            f"WER_DMP size={WER_DMP_SIZE} sha256={wer_hash} signature={signature.decode()} "
            f"timestamp_utc={datetime.datetime.fromtimestamp(timestamp, datetime.timezone.utc).isoformat()} "
            f"first_module={modules[0][2]} exception=0x{exception_code:x} "
            f"address=0x{exception_address:x} params={tuple(hex(x) for x in exception_parameters)} "
            f"fault_modules={[(name, hex(offset)) for name, offset in fault_modules]}"
        )
    finally:
        if client is not None:
            client.close()
        if process.poll() is None:
            process.terminate()
            try:
                process.wait(timeout=3)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=3)
        stderr = process.stderr.read().decode(errors="replace").strip()
        if stderr:
            print(f"qemu-nbd stderr: {stderr}", file=sys.stderr)

print("cleanup=complete")
