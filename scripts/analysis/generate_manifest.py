#!/usr/bin/env python3
"""Backward-compat shim → `python -m nctool manifest generate`.

The manifest generator now lives in the `nctool` package
(`scripts/analysis/nctool/manifest.py`) so it shares one implementation with
`nctool manifest validate` / `roles`. This script is kept because the
`asset-manifest` skill and docs reference it by path; it forwards its historical
CLI (`[ASSET_ROOT] [--nc] [--reuse-hash] [--dry-run]`) to the package.

Prefer the package entry point directly:

    PYTHONPATH=scripts/analysis python3 -m nctool manifest generate [--asset-root DIR] …

Stdlib only; never reads sample pixels (see the package docstring).
"""
from __future__ import annotations

import argparse
import os
import sys

# This file lives in scripts/analysis/, so its own dir is on sys.path[0] when run
# as a script → the sibling `nctool` package imports cleanly with no PYTHONPATH.
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from nctool import manifest as _manifest  # noqa: E402


def main() -> int:
    ap = argparse.ArgumentParser(description="Generate/update nc-assets manifest.json "
                                             "(shim for `nctool manifest generate`)")
    ap.add_argument("asset_root", nargs="?",
                    default=os.environ.get("NC_ASSET_ROOT", "../nc-assets"))
    ap.add_argument("--nc", help="path to the nc binary")
    ap.add_argument("--reuse-hash", action="store_true",
                    help="reuse an existing sha256 when the byte size is unchanged "
                         "(faster, but misses same-size edits; default recomputes all)")
    ap.add_argument("--dry-run", action="store_true",
                    help="print summary but do not write manifest.json")
    ap.add_argument("--allow-exiftool-fallback", action="store_true",
                    help="when nc is not found, build a DEGRADED exiftool-only "
                         "manifest instead of failing loudly (exit 2)")
    args = ap.parse_args()
    return _manifest.cmd_generate(args)


if __name__ == "__main__":
    raise SystemExit(main())
