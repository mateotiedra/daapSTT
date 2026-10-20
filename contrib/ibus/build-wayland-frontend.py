#!/usr/bin/env python3
"""Build patched upstream IBus in /tmp without installing or starting anything.

Requires gcc, pkg-config, patch, wayland-scanner, the gio-2.0/wayland-client/
xkbcommon development packages and wayland-protocols. Only missing IBus headers
are downloaded/extracted locally. This build targets the installed Ubuntu
amd64 IBus 1.5.32-2build1 ABI; it never replaces libibus or ibus-ui-gtk3.
"""

import argparse
import hashlib
from pathlib import Path
import shutil
import subprocess
import tarfile
import urllib.request


VERSION = "1.5.32"
DEBIAN_VERSION = "1.5.32-2build1"
SOURCE_SHA256 = "b24f41ae38b236b254c09f1a8f53c2354b69b0789e89cea888d0494b09d15d67"
HEADERS_SHA256 = "7d851ba120a2d0f651aa065269aacdf3fe5cf71fb358659b5fe3bcc35534f6df"
SOURCE_URL = f"https://github.com/ibus/ibus/releases/download/{VERSION}/ibus-{VERSION}.tar.gz"


def run(*args, cwd=None):
    subprocess.run(args, cwd=cwd, check=True)


def capture(*args):
    return subprocess.check_output(args, text=True).strip()


def verify(path, expected):
    actual = hashlib.sha256(path.read_bytes()).hexdigest()
    if actual != expected:
        raise SystemExit(f"SHA256 mismatch for {path}: {actual}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--work-dir", type=Path,
                        default=Path(f"/tmp/daapstt-ibus-wayland-{VERSION}"))
    args = parser.parse_args()
    work = args.work_dir.resolve()
    patch = Path(__file__).resolve().with_name(f"ibus-{VERSION}-wayland.patch")
    for tool in ("gcc", "pkg-config", "patch", "wayland-scanner", "dpkg-query",
                 "dpkg-deb", "apt"):
        if not shutil.which(tool):
            raise SystemExit(f"Missing build dependency: {tool}")
    installed = capture("dpkg-query", "-W", "-f=${Version}", "libibus-1.0-5:amd64")
    if installed != DEBIAN_VERSION:
        raise SystemExit(f"Expected libibus {DEBIAN_VERSION}, found {installed}; review ABI first")
    packages = ("gio-2.0", "wayland-client", "xkbcommon")
    # Fail before downloading if non-IBus development dependencies are missing.
    import shlex
    flags = shlex.split(capture("pkg-config", "--cflags", "--libs", *packages))
    protocol_dir = Path(capture("pkg-config", "--variable=pkgdatadir", "wayland-protocols"))
    v3_xml = protocol_dir / "unstable/text-input/text-input-unstable-v3.xml"
    if not v3_xml.is_file():
        raise SystemExit(f"Missing wayland-protocols XML: {v3_xml}")
    work.mkdir(parents=True, exist_ok=True)
    archive = work / f"ibus-{VERSION}.tar.gz"
    if not archive.exists():
        urllib.request.urlretrieve(SOURCE_URL, archive)
    verify(archive, SOURCE_SHA256)
    headers = work / "headers"
    deb = work / f"libibus-1.0-dev_{DEBIAN_VERSION}_amd64.deb"
    if not deb.exists():
        run("apt", "download", f"libibus-1.0-dev:amd64={DEBIAN_VERSION}", cwd=work)
    verify(deb, HEADERS_SHA256)
    run("dpkg-deb", "-x", str(deb), str(headers))
    # A fresh source tree makes rerunning the patch deterministic, without
    # deleting any existing directory or relying on a previously patched tree.
    import tempfile
    source_root = Path(tempfile.mkdtemp(prefix="source-", dir=work))
    with tarfile.open(archive) as source:
        source.extractall(source_root, filter="data")
    source = source_root / f"ibus-{VERSION}"
    run("patch", "--batch", "--fuzz=0", "-p1", "-i", str(patch), cwd=source)
    build = source_root / "build"
    build.mkdir()
    (build / "config.h").write_text("/* Standalone frontend: no optional surrounding-text support. */\n")
    # ibusinternal.h only supplies private compile-time helpers. The public
    # headers come from the matching binary package, not an unconfigured tree.
    shutil.copyfile(source / "src/ibusinternal.h", build / "ibusinternal.h")
    run("wayland-scanner", "client-header", str(v3_xml),
        str(build / "text-input-unstable-v3-client-protocol.h"))
    frontend = source / "client/wayland"
    output = work / "ibus-wayland"
    sources = [frontend / name for name in (
        "main.c", "ibuswaylandim.c", "input-method-unstable-v1-protocol.c",
        "input-method-unstable-v2-protocol.c", "virtual-keyboard-unstable-v1-protocol.c")]
    run("gcc", "-std=gnu11", "-O2", "-g", "-Wall", "-Wextra",
        "-DIBUS_COMPILATION", f"-I{build}",
        f"-I{headers / 'usr/include/ibus-1.0'}", f"-I{frontend}",
        *(str(path) for path in sources), "-o", str(output),
        *flags, "-Wl,-z,defs", "-l:libibus-1.0.so.5")
    print(f"Built: {output}")
    print(f"Patched upstream sources and build files: {source_root}")
    print("No installation or launch performed. With the existing bus environment:")
    print(f"  {output}")
    print("The upstream binary has no command-line options; it connects to the current")
    print("IBus bus and WAYLAND_DISPLAY. Stop any temporary frontend first; never run")
    print("two input-method owners on the same seat. No --exec-daemon or --replace.")


if __name__ == "__main__":
    main()
