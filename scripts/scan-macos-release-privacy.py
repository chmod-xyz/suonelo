#!/usr/bin/env python3
"""Fail closed on known local identity/path strings in a bundle or ZIP.
Reports relative paths, rule names and counts, never matched strings.
"""
import argparse
import getpass
import json
from pathlib import Path, PurePosixPath
import re
import socket
import stat
import zipfile


def rules():
    identities = {str(Path.home()), getpass.getuser(), socket.gethostname()}
    result = []
    for index, value in enumerate(sorted(identities)):
        if len(value) < 4:
            continue
        for encoding in ('utf-8', 'utf-16le', 'utf-16be'):
            result.append(('local_identity_' + str(index) + '_' + encoding, re.compile(re.escape(value.encode(encoding)), re.I)))
    for value in ('/Users/', ':\\Users\\', '/private/tmp/', '/var/folders/', '/private/var/folders/'):
        for encoding in ('utf-8', 'utf-16le', 'utf-16be'):
            result.append(('private_path_' + encoding, re.compile(re.escape(value.encode(encoding)), re.I)))
    return result


def matches(data):
    return [{'rule':name, 'count':len(found)} for name, pattern in rules() if (found := pattern.findall(data))]


def scan(path):
    issues = []
    count = 0
    def check(name, data):
        nonlocal count
        count += 1
        for finding in matches(name.encode()) + matches(data):
            issues.append(dict(path=name, **finding))
    if path.is_dir():
        for p in sorted(path.rglob('*')):
            if p.is_symlink():
                raise ValueError('Uninspectable symlink')
            if p.is_file():
                check(p.relative_to(path).as_posix(), p.read_bytes())
    else:
        with zipfile.ZipFile(path) as archive:
            seen = set()
            for entry in archive.infolist():
                name = entry.filename
                parts = PurePosixPath(name).parts
                if name in seen or '..' in parts or name.startswith('/') or stat.S_ISLNK(entry.external_attr >> 16):
                    raise ValueError('Unsafe archive member')
                seen.add(name)
                if not entry.is_dir():
                    if Path(name).suffix in ('.zip', '.dmg', '.gz', '.tar'):
                        raise ValueError('Nested archive requires separate inspection')
                    check(name, archive.read(entry))
            if archive.testzip() is not None:
                raise ValueError('Archive CRC failure')
    if not count:
        raise ValueError('Empty scan')
    return {'filesScanned':count, 'passed':not issues, 'findings':issues}


if __name__ == '__main__':
    parser = argparse.ArgumentParser()
    parser.add_argument('path',type=Path)
    args = parser.parse_args()
    # Exercise identity detection in all supported string encodings.
    assert all(matches(str(Path.home()).encode(e)) for e in ('utf-8','utf-16le','utf-16be'))
    result = scan(args.path)
    print(json.dumps(result,indent=2))
    raise SystemExit(0 if result['passed'] else 1)
