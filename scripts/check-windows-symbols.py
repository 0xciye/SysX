"""Verify that an MSVC PDB matches its exact PE image's CodeView GUID and age."""
import json
from pathlib import Path
import struct
import sys
import uuid


def u32(data, offset):
    return struct.unpack_from("<I", data, offset)[0]


def pe_identity(path):
    data = path.read_bytes()
    assert data[:2] == b"MZ", "Missing DOS header"
    pe = u32(data, 0x3C)
    assert data[pe:pe + 4] == b"PE\0\0", "Missing PE signature"
    assert struct.unpack_from("<H", data, pe + 4)[0] == 0x8664, "Expected x64 PE"
    optional = pe + 24
    assert struct.unpack_from("<H", data, optional)[0] == 0x20B, "Expected PE32+"
    sections = pe + 24 + struct.unpack_from("<H", data, pe + 20)[0]
    count = struct.unpack_from("<H", data, pe + 6)[0]
    debug_rva, debug_size = struct.unpack_from("<II", data, optional + 112 + 6 * 8)
    debug_offset = None
    for index in range(count):
        virtual_size, rva, raw_size, raw = struct.unpack_from("<IIII", data, sections + index * 40 + 8)
        if rva <= debug_rva < rva + max(virtual_size, raw_size):
            debug_offset = raw + debug_rva - rva
            break
    assert debug_offset is not None, "Missing debug directory"
    for offset in range(debug_offset, debug_offset + debug_size, 28):
        if u32(data, offset + 12) == 2:  # IMAGE_DEBUG_TYPE_CODEVIEW
            payload = u32(data, offset + 24)
            if data[payload:payload + 4] == b"RSDS":
                return data[payload + 4:payload + 20], u32(data, payload + 20)
    raise AssertionError("Missing CodeView RSDS identity")


def pdb_identity(path):
    data = path.read_bytes()
    assert data[:32] == b"Microsoft C/C++ MSF 7.00\r\n\x1aDS\0\0\0", "Expected MSF 7 PDB"
    block, _, _, directory_size, _, block_map = struct.unpack_from("<6I", data, 32)
    directory_count = (directory_size + block - 1) // block
    pages = struct.unpack_from(f"<{directory_count}I", data, block_map * block)
    directory = b"".join(data[p * block:(p + 1) * block] for p in pages)[:directory_size]
    stream_count = u32(directory, 0)
    assert stream_count > 1, "Missing PDB info stream"
    sizes = struct.unpack_from(f"<{stream_count}I", directory, 4)
    cursor = 4 + 4 * stream_count
    for index, size in enumerate(sizes):
        page_count = 0 if size == 0xFFFFFFFF else (size + block - 1) // block
        pages = struct.unpack_from(f"<{page_count}I", directory, cursor)
        cursor += page_count * 4
        if index == 1:
            info = b"".join(data[p * block:(p + 1) * block] for p in pages)[:size]
            return info[12:28], u32(info, 8)
    raise AssertionError("Missing PDB info stream")


if __name__ == "__main__":
    image, pdb = map(Path, sys.argv[1:3])
    expected = pe_identity(image)
    actual = pdb_identity(pdb)
    assert actual == expected, "PDB GUID/age does not match the executable"
    print(json.dumps({"image": image.name, "pdb": pdb.name, "status": "PASS",
                      "guid": str(uuid.UUID(bytes_le=expected[0])), "age": expected[1],
                      "pdb_bytes": pdb.stat().st_size}))
