#!/usr/bin/env python3
"""Create deterministic release archives for skillctl."""

from __future__ import annotations

import argparse
import gzip
import io
import json
from pathlib import Path
import tarfile
import zipfile


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Create a deterministic release archive for one skillctl binary."
    )
    parser.add_argument("--repo-root", required=True)
    parser.add_argument("--binary", required=True)
    parser.add_argument("--version", required=True)
    parser.add_argument("--target", required=True)
    parser.add_argument("--output", required=True)
    return parser.parse_args()


def is_windows_target(target: str) -> bool:
    return "windows" in target


def archive_name(version: str, target: str) -> str:
    extension = ".zip" if is_windows_target(target) else ".tar.gz"
    return f"skillctl-{version}-{target}{extension}"


def archive_root(version: str, target: str) -> str:
    return f"skillctl-{version}-{target}"


def binary_name(target: str) -> str:
    return "skillctl.exe" if is_windows_target(target) else "skillctl"


def release_manifest(version: str, target: str, binary: str) -> bytes:
    document = {
        "name": "skillctl",
        "version": version,
        "target": target,
        "binary": binary,
        "files": [binary, "LICENSE", "README.md", "release-manifest.json"],
    }
    return (json.dumps(document, indent=2, sort_keys=True) + "\n").encode("utf-8")


def packaged_entries(
    repo_root: Path, binary_path: Path, version: str, target: str
) -> list[tuple[str, bytes, int]]:
    root = archive_root(version, target)
    binary = binary_name(target)
    return [
        (
            f"{root}/{binary}",
            binary_path.read_bytes(),
            0o755,
        ),
        (
            f"{root}/LICENSE",
            (repo_root / "LICENSE").read_bytes(),
            0o644,
        ),
        (
            f"{root}/README.md",
            (repo_root / "README.md").read_bytes(),
            0o644,
        ),
        (
            f"{root}/release-manifest.json",
            release_manifest(version, target, binary),
            0o644,
        ),
    ]


def write_tar_gz(destination: Path, entries: list[tuple[str, bytes, int]]) -> None:
    with destination.open("wb") as raw_file:
        with gzip.GzipFile(
            filename="",
            mode="wb",
            fileobj=raw_file,
            compresslevel=9,
            mtime=0,
        ) as gzip_file:
            with tarfile.open(fileobj=gzip_file, mode="w", format=tarfile.PAX_FORMAT) as archive:
                for arcname, contents, mode in sorted(entries, key=lambda entry: entry[0]):
                    info = tarfile.TarInfo(name=arcname)
                    info.size = len(contents)
                    info.mode = mode
                    info.mtime = 0
                    info.uid = 0
                    info.gid = 0
                    info.uname = ""
                    info.gname = ""
                    archive.addfile(info, io.BytesIO(contents))


def write_zip(destination: Path, entries: list[tuple[str, bytes, int]]) -> None:
    with zipfile.ZipFile(
        destination,
        mode="w",
        compression=zipfile.ZIP_DEFLATED,
        compresslevel=9,
    ) as archive:
        for arcname, contents, mode in sorted(entries, key=lambda entry: entry[0]):
            info = zipfile.ZipInfo(arcname)
            info.date_time = (1980, 1, 1, 0, 0, 0)
            info.create_system = 3
            info.external_attr = mode << 16
            archive.writestr(info, contents)


def main() -> int:
    args = parse_args()
    repo_root = Path(args.repo_root).resolve()
    binary_path = Path(args.binary).resolve()
    output_dir = Path(args.output).resolve()
    output_dir.mkdir(parents=True, exist_ok=True)

    if not binary_path.is_file():
        raise SystemExit(f"binary does not exist: {binary_path}")

    entries = packaged_entries(repo_root, binary_path, args.version, args.target)
    destination = output_dir / archive_name(args.version, args.target)

    if is_windows_target(args.target):
        write_zip(destination, entries)
    else:
        write_tar_gz(destination, entries)

    print(destination)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
