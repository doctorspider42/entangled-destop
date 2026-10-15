#!/usr/bin/env python3
"""Assemble a static ARM64 Linux init, then put it in a newc initramfs.

Uses clang's integrated assembler only (no cross linker or Linux sysroot).
The object has no relocations; a standard ELF load segment wraps its .text.
Build products go to an explicitly supplied directory outside the checkout.
"""
import argparse
import pathlib
import struct
import subprocess
import tempfile


def text_section(data):
    if data[:6] != b"\x7fELF\x02\x01":
        raise ValueError("assembler did not produce ELF64 little endian")
    offset = struct.unpack_from("<Q", data, 40)[0]
    size, count, names_index = struct.unpack_from("<HHH", data, 58)
    sections = [struct.unpack_from("<IIQQQQIIQQ", data, offset + i * size)
                for i in range(count)]
    names_header = sections[names_index]
    names = data[names_header[4]:names_header[4] + names_header[5]]
    found = None
    for section in sections:
        name_at = section[0]
        name = names[name_at:names.index(b"\0", name_at)]
        if name == b".text":
            found = data[section[4]:section[4] + section[5]]
        if name in (b".rel.text", b".rela.text") and section[5]:
            raise ValueError("unresolved text relocations")
    if not found:
        raise ValueError("no executable text")
    return found


def elf_program(code):
    payload = b"\0" * (4096 - 120) + code
    total = 120 + len(payload)
    ident = b"\x7fELF\x02\x01\x01" + b"\0" * 9
    header = ident + struct.pack("<HHIQQQIHHHHHH", 2, 183, 1, 0x401000,
                                 64, 0, 0, 64, 56, 1, 0, 0, 0)
    segment = struct.pack("<IIQQQQQQ", 1, 5, 0, 0x400000, 0x400000,
                          total, total, 4096)
    return header + segment + payload


def cpio_entry(name, contents, mode, inode, rdevmajor=0, rdevminor=0):
    encoded = name.encode() + b"\0"
    fields = [inode, mode, 0, 0, 1, 0, len(contents), 0, 0,
              rdevmajor, rdevminor, len(encoded), 0]
    header = b"070701" + b"".join(f"{value:08x}".encode() for value in fields)
    beginning = header + encoded
    beginning += b"\0" * (-len(beginning) % 4)
    return beginning + contents + b"\0" * (-len(contents) % 4)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("destination", type=pathlib.Path)
    args = parser.parse_args()
    root = pathlib.Path(__file__).resolve().parent.parent
    destination = args.destination.resolve()
    if destination == root or root in destination.parents:
        raise ValueError("build outputs must be outside the checkout")
    destination.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="entangled-arm64-asm-") as temporary:
        obj = pathlib.Path(temporary) / "init.o"
        subprocess.run(["xcrun", "clang", "--target=aarch64-linux-gnu", "-c",
                        str(root / "guest/arm64-smoke/init.S"), "-o", str(obj)], check=True)
        program = elf_program(text_section(obj.read_bytes()))
    archive = cpio_entry("dev", b"", 0o40755, 1)
    archive += cpio_entry("dev/console", b"", 0o20600, 2, 5, 1)
    archive += cpio_entry("init", program, 0o100755, 3)
    archive += cpio_entry("TRAILER!!!", b"", 0, 4)
    archive += b"\0" * (-len(archive) % 512)
    (destination / "init").write_bytes(program)
    (destination / "smoke.cpio").write_bytes(archive)
    print(f"{destination / 'smoke.cpio'}: {len(archive)} bytes")


if __name__ == "__main__":
    main()
