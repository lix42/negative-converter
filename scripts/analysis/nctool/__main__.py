"""`python -m nctool` entry point.

Minimal seed for the `asset-manifest` task: only the `manifest` command group
(generate / validate / roles) is wired up. The downstream `conversion-metrics`
task adds `metrics` / `thumbs` alongside it.
"""
from __future__ import annotations

import argparse
import sys

from . import manifest as _manifest

ASSET_ROOT_HELP = ("asset root (the folder containing manifest.json); defaults to "
                   "$NC_ASSET_ROOT, else ../nc-assets (the machine-local Drive symlink)")


def _add_root(p: argparse.ArgumentParser) -> None:
    import os
    p.add_argument("--asset-root", dest="asset_root",
                   default=os.environ.get("NC_ASSET_ROOT", "../nc-assets"),
                   help=ASSET_ROOT_HELP)


def build_parser() -> argparse.ArgumentParser:
    ap = argparse.ArgumentParser(prog="python -m nctool",
                                 description="nc conversion-analysis toolkit")
    sub = ap.add_subparsers(dest="group", required=True)

    mani = sub.add_parser("manifest", help="generate / validate / query the asset manifest")
    msub = mani.add_subparsers(dest="cmd", required=True)

    gen = msub.add_parser("generate", help="scan the asset root and write manifest.json")
    _add_root(gen)
    gen.add_argument("--nc", help="path to the nc binary (else auto-discovered)")
    gen.add_argument("--reuse-hash", action="store_true",
                     help="reuse an existing sha256 when the byte size is unchanged "
                          "(faster, but misses same-size edits; default recomputes all)")
    gen.add_argument("--dry-run", action="store_true",
                     help="print the summary but do not write manifest.json")
    gen.add_argument("--allow-exiftool-fallback", action="store_true",
                     help="when the nc binary is not found, build a DEGRADED "
                          "exiftool-only manifest (format/ir_present become "
                          "placeholders) instead of failing loudly (exit 2)")
    gen.set_defaults(func=_manifest.cmd_generate)

    val = msub.add_parser("validate",
                          help="report checksum drift, orphans, and missing files "
                               "(reports only; never deletes)")
    _add_root(val)
    val.set_defaults(func=_manifest.cmd_validate)

    rol = msub.add_parser("roles",
                          help="emit per-roll unexposed|leader|real triples for the "
                               "real-scan harness")
    _add_root(rol)
    rol.set_defaults(func=_manifest.cmd_roles)

    return ap


def main(argv: list[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    return args.func(args)


if __name__ == "__main__":
    raise SystemExit(main())
