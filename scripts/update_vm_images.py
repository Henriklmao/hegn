#!/usr/bin/env python3
"""Refresh the pinned upstream Cloud Hypervisor VM images in src/images.rs.

hegn does not build its own guest image. This script discovers the newest
official Cloud Hypervisor Linux kernel release and the newest official Cloud
Hypervisor cloud image, downloads them, computes SHA-256, and rewrites the
architecture-specific constants. The scheduled CI workflow then opens a pull
request when anything changed.

Network access and ~5 GiB of temporary disk are required. No file is written
outside Python's temporary directory.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import sys
import urllib.request
from dataclasses import dataclass
from pathlib import Path

ASSETS_URL = (
    "https://raw.githubusercontent.com/cloud-hypervisor/cloud-hypervisor/"
    "main/scripts/test_assets.yaml"
)
LINUX_RELEASES_URL = "https://api.github.com/repos/cloud-hypervisor/linux/releases/latest"
TIMEOUT = 300
MAX_ASSET_BYTES = 6 * 1024 * 1024 * 1024
KERNEL_NAMES = {"x86_64": "vmlinux-x86_64", "aarch64": "Image-arm64"}
ROOTFS_PREFIX = {"x86_64": "jammy-server-cloudimg-amd64-custom-", "aarch64": "jammy-server-cloudimg-arm64-custom-"}
ROOTFS_SUFFIX = {"x86_64": ".qcow2", "aarch64": ".raw"}
ROOTFS_FORMAT = {"x86_64": "Qcow2", "aarch64": "Raw"}


@dataclass(frozen=True)
class Candidate:
    version: str
    artifact: str
    url: str
    sha256: str
    size: int
    format: str | None = None


def log(message: str) -> None:
    print(message, file=sys.stderr)


def fetch_text(url: str) -> str:
    request = urllib.request.Request(url, headers={"User-Agent": "hegn-vm-images"})
    with urllib.request.urlopen(request, timeout=TIMEOUT) as response:
        return response.read().decode("utf-8", "replace")


def parse_assets(text: str) -> list[dict[str, str]]:
    """Minimal parser for the flat asset list in test_assets.yaml."""
    assets: list[dict[str, str]] = []
    current: dict[str, str] | None = None
    for raw in text.splitlines():
        line = raw.strip()
        if line.startswith("- filename:"):
            if current:
                assets.append(current)
            current = {"filename": line.split(":", 1)[1].strip()}
        elif current is not None and not line.startswith("#") and ":" in line:
            key, value = line.split(":", 1)
            if key in {"url", "sha256", "sha1", "arch", "test"}:
                current[key] = value.strip()
    if current:
        assets.append(current)
    return assets


def linux_release() -> tuple[str, list[dict]]:
    request = urllib.request.Request(
        LINUX_RELEASES_URL, headers={"User-Agent": "hegn-vm-images"}
    )
    with urllib.request.urlopen(request, timeout=60) as response:
        release = json.load(response)
    return release["tag_name"], release.get("assets", [])


def kernel_candidate(arch: str, release_tag: str, release_assets: list[dict]) -> Candidate:
    name = KERNEL_NAMES[arch]
    for asset in release_assets:
        if asset.get("name") == name:
            url = asset["browser_download_url"]
            if not url.startswith("https://"):
                raise SystemExit(f"refusing non-HTTPS kernel URL {url}")
            log(f"downloading {url}")
            sha, size = download_and_hash(url)
            return Candidate(release_tag, name, url, sha, size)
    raise SystemExit(f"kernel asset {name} not found in the latest Cloud Hypervisor release")


def rootfs_candidate(arch: str, assets: list[dict[str, str]]) -> Candidate:
    prefix = ROOTFS_PREFIX[arch]
    suffix = ROOTFS_SUFFIX[arch]
    candidates = [
        asset
        for asset in assets
        if asset["filename"].startswith(prefix)
        and asset["filename"].endswith(suffix)
        and arch in asset.get("arch", "")
        and asset.get("url", "").startswith("https://")
    ]
    if not candidates:
        raise SystemExit(f"no upstream {suffix} cloud image found for {arch}")
    asset = sorted(candidates, key=lambda item: item["filename"])[-1]
    url = asset["url"]
    log(f"downloading {url}")
    sha, size = download_and_hash(url)
    version = extract_version(asset["filename"])
    return Candidate(version, asset["filename"], url, sha, size, ROOTFS_FORMAT[arch])


def extract_version(filename: str) -> str:
    match = re.search(r"custom-(\d{8})", filename)
    return match.group(1) if match else "unknown"


def download_and_hash(url: str) -> tuple[str, int]:
    digest = hashlib.sha256()
    size = 0
    request = urllib.request.Request(url, headers={"User-Agent": "hegn-vm-images"})
    with urllib.request.urlopen(request, timeout=TIMEOUT) as response:
        while True:
            chunk = response.read(1024 * 1024)
            if not chunk:
                break
            size += len(chunk)
            if size > MAX_ASSET_BYTES:
                raise SystemExit(
                    f"refusing {url}: download exceeds {MAX_ASSET_BYTES // (1024**3)} GiB"
                )
            digest.update(chunk)
    log(f"  {size / (1024 * 1024):.1f} MiB  sha256 {digest.hexdigest()}")
    return digest.hexdigest(), size


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

    if candidate.format and kind == "rootfs":
        pattern = (
            rf"(pub const {prefix}_FORMAT: crate::config::RootfsFormat = "
            r"crate::config::RootfsFormat::)(\w+);"
        )
        block, count = re.subn(pattern, lambda m: f"{m.group(1)}{candidate.format};", block)
        if count != 1:
            raise SystemExit(f"could not update {prefix}_FORMAT in the {arch} block")

    return text[:block_start] + block + text[block_end:]


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--arch", choices=["x86_64", "aarch64"], required=True)
    parser.add_argument("--file", type=Path, default=Path("src/images.rs"))
    parser.add_argument("--check", action="store_true", help="fail if an update is needed")
    args = parser.parse_args()

    text = args.file.read_text()
    original = text

    release_tag, release_assets = linux_release()
    assets = parse_assets(fetch_text(ASSETS_URL))

    text = update_block(
        text, args.arch, kernel_candidate(args.arch, release_tag, release_assets), "kernel"
    )
    text = update_block(text, args.arch, rootfs_candidate(args.arch, assets), "rootfs")

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
