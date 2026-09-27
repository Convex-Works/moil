"""Builds the packages the tests' package index serves, all at version 1.0.

usage: packages.py OUT_DIR BUILT_DIR NAME=KIND ...

KIND is what the index offers for NAME:

  sdist                only an sdist
  sdist+foreign-wheel  an sdist, and a wheel for a platform no test runs on

Building the sdist, by uv or anything else, leaves a file named NAME in
BUILT_DIR, and makes a wheel whose module has VALUE = "built". The foreign
wheel's module has VALUE = "wheel". Only the standard library is needed.
"""

import io
import os
import sys
import tarfile
import zipfile

BACKEND = '''\
"""A PEP 517 build backend that leaves a mark when it builds."""

import base64, hashlib, os, zipfile

NAME = {name!r}
BUILT = {built!r}


def get_requires_for_build_wheel(config_settings=None):
    return []


def build_wheel(wheel_directory, config_settings=None, metadata_directory=None):
    os.makedirs(BUILT, exist_ok=True)
    open(os.path.join(BUILT, NAME), "w").close()
    info = NAME + "-1.0.dist-info"
    files = [
        (NAME + "/__init__.py", b'VALUE = "built"\\n'),
        (info + "/METADATA", ("Metadata-Version: 2.1\\nName: %s\\nVersion: 1.0\\n" % NAME).encode()),
        (info + "/WHEEL", b"Wheel-Version: 1.0\\nGenerator: moil-tests\\nRoot-Is-Purelib: true\\nTag: py3-none-any\\n"),
    ]
    record = ""
    for path, data in files:
        digest = base64.urlsafe_b64encode(hashlib.sha256(data).digest()).rstrip(b"=").decode()
        record += "%s,sha256=%s,%d\\n" % (path, digest, len(data))
    record += info + "/RECORD,,\\n"
    wheel = NAME + "-1.0-py3-none-any.whl"
    with zipfile.ZipFile(os.path.join(wheel_directory, wheel), "w") as archive:
        for path, data in files:
            archive.writestr(path, data)
        archive.writestr(info + "/RECORD", record)
    return wheel
'''

PYPROJECT = """\
[build-system]
requires = []
build-backend = "backend"
backend-path = ["."]

[project]
name = "{name}"
version = "1.0"
"""

# Static metadata, so that locking never needs to build the sdist.
PKG_INFO = "Metadata-Version: 2.2\nName: {name}\nVersion: 1.0\n"

# Fixed timestamps, so that the archives' bytes depend only on the inputs.
MTIME = 1_700_000_000
DATE = (2024, 1, 1, 0, 0, 0)


def sdist(out, built, name):
    files = {
        "PKG-INFO": PKG_INFO.format(name=name),
        "pyproject.toml": PYPROJECT.format(name=name),
        "backend.py": BACKEND.format(name=name, built=built),
    }
    buffer = io.BytesIO()
    with tarfile.open(fileobj=buffer, mode="w:gz", format=tarfile.PAX_FORMAT) as archive:
        for path, text in files.items():
            data = text.encode()
            info = tarfile.TarInfo(f"{name}-1.0/{path}")
            info.size, info.mtime, info.mode = len(data), MTIME, 0o644
            archive.addfile(info, io.BytesIO(data))
    write(out, f"{name}-1.0.tar.gz", buffer.getvalue())


def foreign_wheel(out, name):
    info = f"{name}-1.0.dist-info"
    files = [
        (f"{name}/__init__.py", 'VALUE = "wheel"\n'),
        (f"{info}/METADATA", f"Metadata-Version: 2.1\nName: {name}\nVersion: 1.0\n"),
        (f"{info}/WHEEL", "Wheel-Version: 1.0\nGenerator: moil-tests\nRoot-Is-Purelib: false\nTag: py3-none-win_amd64\n"),
        (f"{info}/RECORD", ""),
    ]
    buffer = io.BytesIO()
    with zipfile.ZipFile(buffer, "w") as archive:
        for path, text in files:
            archive.writestr(zipfile.ZipInfo(path, date_time=DATE), text)
    write(out, f"{name}-1.0-py3-none-win_amd64.whl", buffer.getvalue())


def write(out, file_name, data):
    with open(os.path.join(out, file_name), "wb") as f:
        f.write(data)


out, built, *packages = sys.argv[1:]
for package in packages:
    name, kind = package.split("=")
    sdist(out, built, name)
    if kind == "sdist+foreign-wheel":
        foreign_wheel(out, name)
    elif kind != "sdist":
        sys.exit(f"unknown kind {kind!r}")
