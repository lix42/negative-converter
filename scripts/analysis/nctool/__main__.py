"""`python -m nctool` entry point.

Four command groups: `manifest` (generate / validate / roles), `compare`
(build-version run / diff), `roll` (manifest-driven calibrate / convert /
deterministic analysis artifacts), and `metrics` (pixel-derived measurement of a
converted image, whatever produced it).

`metrics` is the one group that needs third-party packages; see
`scripts/analysis/requirements.txt`. Importing the module is still free — it
pulls `numpy`/`tifffile` inside the functions that touch pixels — so a checkout
without the venv builds the same parser and gets a real error only if it runs the
command.
"""
from __future__ import annotations

import argparse
import sys

from . import compare as _compare
from . import manifest as _manifest
from . import metrics as _metrics
from . import roll as _roll

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

    # --- compare: the conversion-versioning comparison harness ---------------
    cmp_ = sub.add_parser("compare",
                          help="convert a fixed benchmark set under a build and diff "
                               "two builds' results")
    csub = cmp_.add_subparsers(dest="cmd", required=True)

    crun = csub.add_parser("run",
                           help="convert a benchmark set with one nc build and write "
                                "its run record")
    _add_root(crun)
    crun.add_argument("--nc", required=True,
                     help="path to the nc binary to benchmark (required and explicit: "
                          "a comparison of two builds must never auto-discover one)")
    crun.add_argument("--set", dest="set_name", default="fixtures",
                      help="benchmark set from benchmark.json (default: fixtures — the "
                           "committed fixtures, runnable without the Drive assets)")
    crun.add_argument("--out", help="write the run record here (default: stdout)")
    crun.add_argument("--benchmark", default=_compare.BENCHMARK,
                      help="path to the benchmark manifest (default: "
                           "scripts/analysis/benchmark.json)")
    crun.add_argument("--skip-checksums", action="store_true",
                      help="skip hashing the input bytes (faster on 50-160 MB scans; "
                           "only safe when you have just run `manifest validate`). "
                           "Recorded per frame as checksums:skipped and surfaced by "
                           "`compare diff`, so an unverified comparison never looks "
                           "verified")
    crun.set_defaults(func=_compare.cmd_run)

    cdiff = csub.add_parser("diff",
                            help="diff two run records into a version-keyed comparison "
                                 "report (mean dRGB, clip-fraction delta, timings)")
    cdiff.add_argument("before", help="run record from the baseline build")
    cdiff.add_argument("after", help="run record from the candidate build")
    cdiff.set_defaults(func=_compare.cmd_diff)

    # --- roll: one manifest roll, calibrated once and rendered by configuration
    roll = sub.add_parser("roll", help="calibrate, convert, and analyze manifest rolls")
    rsub = roll.add_subparsers(dest="cmd", required=True)

    rconvert = rsub.add_parser(
        "convert", help="measure Dmin/Dmax, freeze a recipe, and convert one roll")
    rconvert.add_argument("roll", help="source roll name from manifest.json")
    _add_root(rconvert)
    rconvert.add_argument("--nc", required=True, help="path to the nc binary to run")
    rconvert.add_argument("--config", help="configuration ID (default: hash of frozen recipe)")
    rconvert.add_argument("--out-dir", help="output directory (default: converted/nc/CONFIG/ROLL)")
    rconvert.add_argument("--recipe", help="partial recipe or conversion sidecar to extend")
    rconvert.add_argument("--dmin-region", help="unexposed-frame X,Y,W,H (default: center 80%%)")
    rconvert.add_argument("--dmax-region", help="leader-frame X,Y,W,H (default: center 80%%)")
    rconvert.add_argument("--d-max", type=float,
                          help="explicit Dmax; skips leader estimation (for a known or "
                               "deliberately chosen fallback value)")
    rconvert.add_argument("--dmin-mode", choices=("grid", "region"), default="grid",
                          help="measure Dmin with a five-cell grid or one region "
                               "aggregate (default: grid)")
    rconvert.add_argument("--film-type", choices=("unknown", "silver", "chromogenic"),
                          help="record film chemistry in the estimate reports and "
                               "the frozen recipe (provenance; it gates nothing)")
    rconvert.add_argument("--output-preset", help="override output.preset in the recipe")
    rconvert.add_argument("--print-exposure", type=float,
                          help="override print.print_exposure in the recipe")
    rconvert.add_argument("--max-memory", default="6GiB",
                          help="memory budget passed to estimate and roll (default: 6GiB)")
    rconvert.add_argument("--strict-estimate", action="store_true",
                          help="promote calibration warnings to errors")
    rconvert.add_argument("--strict-roll", action="store_true",
                          help="promote conversion warnings to a failing roll exit")
    rconvert.set_defaults(func=_roll.cmd_convert)

    ranalyze = rsub.add_parser(
        "analyze", help="normalize one converted roll into a deterministic JSON artifact")
    ranalyze.add_argument("roll", help="source roll name")
    ranalyze.add_argument("run", help="configuration ID or path to tags.json")
    _add_root(ranalyze)
    ranalyze.add_argument(
        "--out", help="write analysis JSON here (default: analysis.json beside tags.json)")
    ranalyze.set_defaults(func=_roll.cmd_analyze)

    # --- metrics: pixel-derived measurement of one output image --------------
    met = sub.add_parser(
        "metrics",
        help="measure a converted image's own pixels (any producer: nc, NLP, "
             "SmartConvert, a hand-edited export)")
    esub = met.add_subparsers(dest="cmd", required=True)

    mimage = esub.add_parser(
        "image", help="measure one image and emit its metric record as JSON")
    mimage.add_argument("image", help="path to a single-page RGB TIFF")
    mimage.add_argument(
        "--space", required=True,
        # Listed from the table itself: a hand-written list here went stale the
        # moment Adobe RGB was added, and `--help` is what a user reads.
        help="the image's colour space, declared (" + ", ".join(sorted(_metrics.SPACES))
             + "). Never inferred: a TIFF's samples do not say whether they are "
               "transfer-encoded, and guessing wrong yields a plausible wrong answer")
    mimage.add_argument(
        "--region",
        help="measure this rectangle, as FRACTIONS of the frame: x,y,w,h. "
             "Fractions, not pixels, because compared images differ in size")
    mimage.add_argument(
        "--inset", type=float,
        help="measure all but this fraction of each edge (0.05 trims 5%% per side). "
             "Use it to keep the film holder and rebate out of the statistics — but "
             "check it actually clears them: a holder can occupy 10-15%% of an edge")
    mimage.add_argument(
        "--jpeg-image", choices=("sdr", "hdr"), default="sdr",
        help="which rendition of a gain-map JPEG to measure (default: sdr, the "
             "base image — which is also what a plain JPEG has). `hdr` needs the "
             "gain map applied and is not implemented; it says so rather than "
             "measuring the base and calling it HDR")
    mimage.add_argument("--out", help="write the record here (default: stdout)")
    mimage.add_argument(
        "--no-checksum", action="store_true",
        help="skip hashing the image bytes (the record then cannot identify which "
             "file it described)")
    mimage.set_defaults(func=_metrics.cmd_image)

    mroll = esub.add_parser(
        "roll", help="measure every converted frame of one roll and roll the "
                     "scalars up into one artifact")
    mroll.add_argument("roll", help="source roll name from manifest.json")
    mroll.add_argument("run", help="configuration ID or path to tags.json")
    _add_root(mroll)
    mroll.add_argument(
        "--space",
        help="override the colour space; by default it is resolved from the run's "
             "frozen recipe (recorded provenance, not a guess at the pixels) and "
             "an under-determined one is refused")
    mroll.add_argument("--region", help="as for `metrics image`: x,y,w,h fractions")
    mroll.add_argument("--inset", type=float,
                       help="as for `metrics image`: trim this fraction per edge")
    mroll.add_argument(
        "--jpeg-image", choices=("sdr", "hdr"), default="sdr",
        help="which rendition of a gain-map JPEG to measure (default: sdr, the "
             "base image — which is also what a plain JPEG has). `hdr` needs the "
             "gain map applied and is not implemented; it says so rather than "
             "measuring the base and calling it HDR")
    mroll.add_argument("--out", help="write the record here "
                                     "(default: metrics.json beside tags.json)")
    mroll.add_argument("--markdown", help="also write the Markdown table here")
    mroll.set_defaults(func=_metrics.cmd_roll)

    mtable = esub.add_parser(
        "table", help="render a stored roll metrics record as Markdown")
    mtable.add_argument("record", help="path to a metrics.json written by `roll`")
    mtable.add_argument("--out", help="write here (default: stdout)")
    mtable.set_defaults(func=_metrics.cmd_table)

    return ap


def main(argv: list[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    return args.func(args)


if __name__ == "__main__":
    raise SystemExit(main())
