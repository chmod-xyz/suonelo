"""Verify the Windows CI inputs and executable without logging matched text."""

import argparse
import hashlib
import json
import os
import re
import stat
import struct
from pathlib import Path


PRIVATE_PATTERNS = (
    rb"[a-z]:[\\/]+(?:users|documents and settings)[\\/]+[^\\/\x00\r\n]+",
    rb"-----begin (?:[a-z0-9]+ )*private key-----",
    rb"github_pat_[a-z0-9_]{22,}|gh[pousr]_[a-z0-9]{30,}",
)
SOURCE_INPUTS = (
    "app", "components", "lib", "public", "src-tauri/src", "src-tauri/tests", "src-tauri/icons",
    "src-tauri/capabilities", "src-tauri/tauri.conf.json", "src-tauri/build.rs",
    "src-tauri/Cargo.toml", "src-tauri/Cargo.lock", "package.json",
    "package-lock.json", "next.config.ts", "postcss.config.mjs", "tsconfig.json",
)


def private_match(data, extra=()):
    # Removing NUL padding also detects the ASCII subset of UTF-16 LE and BE.
    folded = data.lower()
    unpadded = folded.replace(b"\0", b"")
    if any(re.search(pattern, unpadded) for pattern in PRIVATE_PATTERNS):
        return True
    return any(
        variant.lower().encode(encoding) in folded
        for value in extra
        for variant in {value, value.replace("\\", "/"), value.replace("/", "\\")}
        for encoding in ("utf-8", "utf-16le", "utf-16be")
    )


def safe_payload(path, markers):
    attributes = getattr(path.lstat(), "st_file_attributes", 0)
    if path.is_symlink() or attributes & stat.FILE_ATTRIBUTE_REPARSE_POINT:
        raise ValueError("A build input is a filesystem link.")
    data = path.read_bytes()
    if private_match(os.fsencode(path.name), markers) or private_match(data, markers):
        raise ValueError("A build input or executable contains a private marker.")
    return data


def payload_files(path):
    attributes = getattr(path.lstat(), "st_file_attributes", 0)
    if path.is_symlink() or attributes & stat.FILE_ATTRIBUTE_REPARSE_POINT:
        raise ValueError("A build input is a filesystem link.")
    if path.is_dir():
        for child in sorted(path.iterdir()):
            yield from payload_files(child)
    elif path.is_file():
        yield path
    else:
        raise ValueError("A build input is not a regular file.")


def verify_pe(data, arch):
    if data[:2] != b"MZ":
        raise ValueError("Expected a Windows PE executable.")
    pe, = struct.unpack_from("<I", data, 0x3C)
    if data[pe:pe + 4] != b"PE\0\0":
        raise ValueError("Invalid PE signature.")
    machine, sections, _, _, _, optional_size, characteristics = struct.unpack_from(
        "<HHIIIHH", data, pe + 4
    )
    optional = pe + 24
    magic, = struct.unpack_from("<H", data, optional)
    expected_machine, expected_magic, directories = {
        "x86": (0x014C, 0x010B, 96),
        "x64": (0x8664, 0x020B, 112),
    }[arch]
    if (machine, magic) != (expected_machine, expected_magic) or not characteristics & 2 or characteristics & 0x2000:
        raise ValueError(f"Expected a Windows {arch} PE executable.")
    directory_count, = struct.unpack_from("<I", data, optional + directories - 4)
    if directory_count < 7 or optional_size < directories + 7 * 8:
        raise ValueError("Incomplete PE metadata.")
    certificate = struct.unpack_from("<II", data, optional + directories + 4 * 8)
    if certificate != (0, 0):
        raise ValueError("Unexpected Authenticode certificate table.")
    debug_rva, debug_size = struct.unpack_from("<II", data, optional + directories + 6 * 8)
    if (debug_rva, debug_size) == (0, 0):
        return
    if not debug_rva or not debug_size or debug_size % 28:
        raise ValueError("Malformed PE debug directory.")
    for index in range(sections):
        section = optional + optional_size + index * 40
        _, address, length, start = struct.unpack_from("<IIII", data, section + 8)
        if address <= debug_rva and debug_rva + debug_size <= address + length:
            offset = start + debug_rva - address
            if offset + debug_size > len(data):
                break
            for entry in range(offset, offset + debug_size, 28):
                kind, = struct.unpack_from("<I", data, entry + 12)
                if kind == 2:
                    raise ValueError("Executable has a CodeView/PDB record.")
            return
    raise ValueError("Unreadable PE debug directory.")


def runner_markers():
    keys = ("GITHUB_WORKSPACE", "RUNNER_TEMP", "USERPROFILE", "CARGO_HOME", "RUSTUP_HOME")
    # Restrict automatic runner checks to the hosted job. A local owner review
    # supplies its own private identifiers from a file outside the checkout.
    if os.environ.get("GITHUB_ACTIONS") != "true":
        return []
    return [os.environ[key] for key in keys if os.environ.get(key)]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--private-identifiers", type=Path, help="Local UTF-8 file, one value per line")
    sub = parser.add_subparsers(dest="mode", required=True)
    sub.add_parser("source")
    frontend = sub.add_parser("frontend")
    frontend.add_argument("manifest", type=Path)
    frontend.add_argument("--verify", action="store_true")
    binary = sub.add_parser("binary")
    binary.add_argument("executable", type=Path)
    binary.add_argument("--arch", choices=("x86", "x64"), default="x86")
    args = parser.parse_args()
    try:
        markers = runner_markers()
        if args.private_identifiers:
            markers.extend(
                value.strip() for value in args.private_identifiers.read_text(encoding="utf-8-sig").splitlines()
                if value.strip()
            )
        if args.mode == "source":
            for root_file in Path(".").iterdir():
                if root_file.name.lower().startswith(".env") and root_file.name != ".env.example":
                    raise ValueError("Unexpected dotenv build input.")
            for source in SOURCE_INPUTS:
                for path in payload_files(Path(source)):
                    safe_payload(path, markers)
        elif args.mode == "frontend":
            if not Path("out/index.html").is_file():
                raise ValueError("The frontend export is missing its entry point.")
            files = {}
            for path in payload_files(Path("out")):
                if path.suffix.lower() == ".map":
                    raise ValueError("The frontend export contains a source map.")
                payload = safe_payload(path, markers)
                files[path.as_posix()] = hashlib.sha256(payload).hexdigest()
            manifest = json.dumps(files, sort_keys=True, indent=2) + "\n"
            if args.verify:
                if args.manifest.read_text(encoding="utf-8") != manifest:
                    raise ValueError("The frontend changed during native compilation.")
            else:
                args.manifest.write_text(manifest, encoding="utf-8")
        else:
            verify_pe(safe_payload(args.executable, markers), args.arch)
    except (OSError, UnicodeError, struct.error):
        parser.exit(1, "CI artifact check failed: an input could not be read or parsed.\n")
    except ValueError as error:
        parser.exit(1, f"CI artifact check failed: {error}\n")
    print(f"CI artifact check passed: {args.mode}")


if __name__ == "__main__":
    main()

