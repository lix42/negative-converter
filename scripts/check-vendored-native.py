#!/usr/bin/env python3
"""Verify deterministic hashes of nc's two vendored native source snapshots."""

from __future__ import annotations

import argparse
import hashlib
import json
import subprocess
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
ULTRAHDR = ROOT / "vendor/ultrahdr-sys/libultrahdr"
TURBOJPEG = ULTRAHDR / "third_party/turbojpeg"
MANIFEST = ROOT / "vendor/ultrahdr-sys/VENDORED_SNAPSHOT.json"


def snapshot_paths(root: Path, excluded_root: Path | None = None) -> list[Path]:
    paths = []
    for path in root.rglob("*"):
        if not path.is_file() and not path.is_symlink():
            continue
        if ".git" in path.parts:
            continue
        if excluded_root is not None and path.is_relative_to(excluded_root):
            continue
        paths.append(path)
    return sorted(paths, key=lambda item: item.as_posix())


def tracked_paths(root: Path, excluded_root: Path | None = None) -> set[Path]:
    """Return index entries for a snapshot tree.

    libultrahdr ships a nested .gitignore whose broad build* and tests/data
    rules also hide legitimate files after the source is copied under vendor/.
    The local snapshot therefore has to be force-added. Checking the index here
    prevents a local hash from blessing files that a fresh CI checkout cannot
    contain. This workaround is removed by
    output/ultrahdr-dependency-externalization together with the local snapshot.
    """
    relative_root = root.relative_to(ROOT).as_posix()
    result = subprocess.run(
        ["git", "-C", str(ROOT), "ls-files", "-z", "--", relative_root],
        check=True,
        capture_output=True,
    )
    paths = {
        ROOT / entry
        for entry in result.stdout.decode().split("\0")
        if entry
    }
    if excluded_root is not None:
        paths = {path for path in paths if not path.is_relative_to(excluded_root)}
    return paths


def verify_snapshot_is_tracked(root: Path, excluded_root: Path | None = None) -> None:
    on_disk = set(snapshot_paths(root, excluded_root))
    in_index = tracked_paths(root, excluded_root)
    if on_disk == in_index:
        return

    untracked = sorted(on_disk - in_index, key=lambda item: item.as_posix())
    missing = sorted(in_index - on_disk, key=lambda item: item.as_posix())
    details = []
    if untracked:
        details.append(
            "not tracked (use git add -f for upstream-ignored snapshot files): "
            + ", ".join(path.relative_to(ROOT).as_posix() for path in untracked[:10])
        )
    if missing:
        details.append(
            "tracked but absent: "
            + ", ".join(path.relative_to(ROOT).as_posix() for path in missing[:10])
        )
    raise RuntimeError("vendored snapshot/index mismatch; " + "; ".join(details))


def tree_hash(root: Path, excluded_root: Path | None = None) -> tuple[str, int]:
    digest = hashlib.sha256()
    count = 0
    for path in snapshot_paths(root, excluded_root):
        relative = path.relative_to(root).as_posix().encode()
        if path.is_symlink():
            payload = b"symlink\0" + path.readlink().as_posix().encode()
        else:
            payload = path.read_bytes()
        digest.update(len(relative).to_bytes(8, "big"))
        digest.update(relative)
        digest.update(len(payload).to_bytes(8, "big"))
        digest.update(payload)
        count += 1
    return digest.hexdigest(), count


def snapshot() -> dict[str, object]:
    verify_snapshot_is_tracked(ULTRAHDR, TURBOJPEG)
    verify_snapshot_is_tracked(TURBOJPEG)
    ultrahdr_hash, ultrahdr_files = tree_hash(ULTRAHDR, TURBOJPEG)
    turbojpeg_hash, turbojpeg_files = tree_hash(TURBOJPEG)
    return {
        "schema_version": 1,
        "algorithm": "sha256(path-length || path || content-length || content), sorted paths",
        "snapshots": {
            "libultrahdr_without_bundled_libjpeg_turbo": {
                "revision": "11ac0c325bbf56ecf8be8704ff0f79fc9e1aac77",
                "files": ultrahdr_files,
                "sha256": ultrahdr_hash,
            },
            "libjpeg_turbo": {
                "revision": "20ade4dea9589515a69793e447a6c6220b464535",
                "files": turbojpeg_files,
                "sha256": turbojpeg_hash,
            },
        },
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--write",
        action="store_true",
        help="replace the checked-in manifest after intentional source review",
    )
    args = parser.parse_args()
    try:
        actual = snapshot()
    except (OSError, RuntimeError, subprocess.CalledProcessError) as error:
        print(f"unable to verify vendored native snapshots: {error}")
        return 1
    rendered = json.dumps(actual, indent=2, sort_keys=True) + "\n"
    if args.write:
        MANIFEST.write_text(rendered)
        print(f"wrote {MANIFEST.relative_to(ROOT)}")
        return 0
    expected = json.loads(MANIFEST.read_text())
    if actual != expected:
        print(
            "vendored native snapshot mismatch; review the source diff, update "
            "PINNED_REVISION, then run scripts/check-vendored-native.py --write"
        )
        return 1
    print(
        "vendored native snapshots match: "
        f"{actual['snapshots']['libultrahdr_without_bundled_libjpeg_turbo']['files']} "
        "libultrahdr files, "
        f"{actual['snapshots']['libjpeg_turbo']['files']} libjpeg-turbo files"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
