#!/usr/bin/env python3
"""Refresh the pinned upstream Firecracker VM images in src/images.rs.

hegn does not build its own guest image. This script discovers the newest
official Firecracker CI kernel and the newest ext4 rootfs, downloads them,
computes SHA-256, and rewrites the architecture-specific constants. The
scheduled CI workflow then opens a pull request when anything changed.

Network access and ~350 MiB of temporary disk are required. No file is written
outside Python's temporary directory.
"""

from __future__ import annotations

import argparse
import hashlib
import re
import sys
import urllib.parse
import urllib.request
import xml.etree.ElementTree as ET
from dataclasses import dataclass
from pathlib import Path

BUCKET = "https://s3.amazonaws.com/spec.ccfc.min"
BASE_PREFIX = "firecracker-ci/"
VERSION_RE = re.compile(r"^firecracker-ci/v(\d+)\.(\d+)/$")
KERNEL_RE = re.compile(r"/vmlinux-(6\.\d+\.\d+)$")
ROOTFS_RE = re.compile(r"/ubuntu-[0-9.]+\.ext4$")
TIMEOUT = 120


@dataclass(frozen=True)
class Candidate:
    version: str
    artifact: str
    url: str
    sha256: str
    size: int


def log(message: str) -> None:
    print(message, file=sys.stderr)


def list_prefix(prefix: str, delimiter: bool = False) -> list[str]:
    tag = "CommonPrefixes" if delimiter else "Contents"
    child = "Prefix" if delimiter else "Key"
    values: list[str] = []
    token: str | None = None
    while True:
        params = {
            "list-type": "2",
            "prefix": prefix,
            "max-keys": "1000",
        }
        if delimiter:
            params["delimiter"] = "/"
        if token:
            params["continuation-token"] = token
        url = f"{BUCKET}?{urllib.parse.urlencode(params)}"
        with urllib.request.urlopen(url, timeout=TIMEOUT) as response:
            tree = ET.parse(response)
        for element in tree.findall(f".//{{*}}{tag}"):
            text = element.findtext(f"{{*}}{child}")
            if text:
                values.append(text)
        if tree.findtext(".//{*}IsTruncated") != "true":
            break
        token = tree.findtext(".//{*}NextContinuationToken")
        if not token:
            break
    return values


def versions() -> list[tuple[int, int, str]]:
    found = []
    for prefix in list_prefix(BASE_PREFIX, delimiter=True):
        match = VERSION_RE.match(prefix)
        if match:
            found.append((int(match.group(1)), int(match.group(2)), prefix))
    return sorted(found, reverse=True)


def newest_kernel(arch: str) -> tuple[str, str]:
    for _, _, prefix in versions():
        keys = list_prefix(f"{prefix}{arch}/")
        candidates = []
        for key in keys:
            if "/debug/" in key:
                continue
            match = KERNEL_RE.search(key)
            if match:
                candidates.append((tuple(int(p) for p in match.group(1).split(".")), key))
        if candidates:
            candidates.sort(reverse=True)
            key = candidates[0][1]
            return key, f"{BUCKET}/{key}"
    raise SystemExit("no upstream vmlinux 6.x artifact found")


def newest_ext4_rootfs(arch: str) -> tuple[str, str]:
    for _, _, prefix in versions():
        keys = list_prefix(f"{prefix}{arch}/")
        candidates = sorted(key for key in keys if ROOTFS_RE.search(key))
        if candidates:
            key = candidates[-1]
            return key, f"{BUCKET}/{key}"
    raise SystemExit("no upstream ext4 rootfs artifact found")


def download_and_hash(url: str) -> tuple[str, int]:
    digest = hashlib.sha256()
    size = 0
    log(f"downloading {url}")
    with urllib.request.urlopen(url, timeout=TIMEOUT) as response:
        while True:
            chunk = response.read(1024 * 1024)
            if not chunk:
                break
            size += len(chunk)
            digest.update(chunk)
    return digest.hexdigest(), size


def candidate(url: str) -> Candidate:
    key = urllib.parse.urlparse(url).path.lstrip("/")
    parts = key.split("/")
    # spec.ccfc.min/firecracker-ci/<version>/<arch>/<artifact>
    version = parts[2] if len(parts) > 2 else "unknown"
    artifact = parts[-1]
    sha, size = download_and_hash(url)
    log(f"  {size / (1024 * 1024):.1f} MiB  sha256 {sha}")
    return Candidate(version, artifact, url, sha, size)


def update_block(text: str, arch: str, candidate: Candidate, kind: str) -> str:
    cfg = f'#[cfg(target_arch = "{arch}")]'
    start = text.index(cfg)
    block_start = text.index("mod pinned {", start)
    block_end = text.index("\n}", block_start)
    block = text[block_start:block_end]

    prefix = kind.upper()
    replacements = {
        f"{prefix}_VERSION": candidate.version,
        f"{prefix}_ARTIFACT": candidate.artifact,
        f"{prefix}_URL": candidate.url,
        f"{prefix}_SHA256": candidate.sha256,
    }
    for name, value in replacements.items():
        pattern = rf'(pub const {name}: &str =\s*)"[^"]*";'
        block, count = re.subn(pattern, lambda m: f'{m.group(1)}"{value}";', block)
        if count != 1:
            raise SystemExit(f"could not update {name} in the {arch} block")
    return text[:block_start] + block + text[block_end:]


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--arch", choices=["x86_64", "aarch64"], required=True)
    parser.add_argument("--file", type=Path, default=Path("src/images.rs"))
    parser.add_argument("--check", action="store_true", help="fail if an update is needed")
    args = parser.parse_args()

    text = args.file.read_text()
    original = text

    for kind, finder in (("kernel", newest_kernel), ("rootfs", newest_ext4_rootfs)):
        key, url = finder(args.arch)
        log(f"selected {kind}: {key}")
        text = update_block(text, args.arch, candidate(url), kind)

    if args.check:
        if text != original:
            log(f"{args.file} is stale; run scripts/update_vm_images.py --arch {args.arch}")
            return 1
        log(f"{args.file} is up to date for {args.arch}")
        return 0

    if text != original:
        args.file.write_text(text)
        log(f"updated {args.file} for {args.arch}")
    else:
        log(f"{args.file} already up to date for {args.arch}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
