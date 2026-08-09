#!/usr/bin/env python3
"""Re-measure the v1 → v2 default-render comparison table.

Backs [`docs/reports/render-defaults-v2.md`](../../docs/reports/render-defaults-v2.md).
The report's headline is a claim about real scans, so it has to be re-runnable by
someone who did not write it — that is the whole reason this file is committed
rather than the measurement being a shell session someone describes afterwards.

It reads **derived numbers only** from `nc`'s JSON report (`loss.*`,
`output_stats.mean`); no sample pixels are read, printed, or stored, per CLAUDE.md.

Why an explicit argument **list** and not a shell helper: the first pass at this
table was produced by a zsh function that interpolated an unquoted `$extra`
variable. zsh does not word-split unquoted parameters, so `--d-max 2.0` arrived as
one argument, `nc` rejected nothing visible, and the "v1" column was measured with
a v2 anchor. Every published number was wrong. `subprocess` with a real list cannot
reproduce that class of mistake.

Usage (from anywhere):

    cargo build --release
    python3 scripts/render-defaults-v2/measure.py

Prerequisites: a release `nc` at `target/release/nc`, and the real scans at
`../nc-assets` (the machine-local symlink described in CLAUDE.md).
"""

from __future__ import annotations

import json
import subprocess
import sys
import tempfile
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
NC = REPO / "target" / "release" / "nc"
ASSETS = REPO.parent / "nc-assets"

# (label, frame path relative to the assets root, the roll's frozen film base).
#
# The film base is stated per frame because it has no default (`nc` exits 2 without
# one) and because a roll's base is a calibration, not a per-frame measurement: the
# same values must be used for both versions or the comparison measures the base
# rather than the curve.
FRAMES = [
    (
        "Ektar 20260713-nikon-963",
        "rolls/Ektar/20260713-nikon-963.tif",
        "0.51679254,0.2768597,0.18973067",
    ),
    (
        "Ektar 20260713-nikon-971",
        "rolls/Ektar/20260713-nikon-971.tif",
        "0.51679254,0.2768597,0.18973067",
    ),
    (
        "Portra160 20260720-nikon-1058",
        "rolls/Portra160/20260720-nikon-1058.tif",
        "0.5340505,0.26347753,0.15655756",
    ),
    (
        "Portra160 20260720-nikon-1065",
        "rolls/Portra160/20260720-nikon-1065.tif",
        "0.5340505,0.26347753,0.15655756",
    ),
]

# v2 is the shipped default render, so it takes no conversion flags at all — if it
# needed any, it would not be the default. v1 is spelled out in full: all three
# defaults that moved on 2026-08-08 have to be restored together, because restoring
# two of the three measures a render that never shipped.
VERSIONS = {
    "v1": ["--density-curve", "exponential", "--density-gamma", "1.0", "--d-max", "2.0"],
    "v2": [],
}


def measure(frame: Path, film_base: str, extra: list[str], out: Path) -> dict:
    argv = [
        str(NC),
        "convert",
        str(frame),
        "-o",
        str(out),
        "--film-base",
        film_base,
        *extra,
    ]
    proc = subprocess.run(argv, capture_output=True, text=True, check=False)
    if proc.returncode != 0:
        sys.exit(f"nc exited {proc.returncode} for {argv}\n{proc.stderr}")
    report = json.loads(proc.stdout)
    loss = report["loss"]
    total = loss["total_samples"]
    return {
        # The fraction the report's "clipped" column carries: samples the u16 encode
        # had to pull back to white, as a percentage of all samples.
        "clipped_pct": 100.0 * loss["clipped_high"] / total if total else 0.0,
        # Green only. Read it as a change-detector, never as a quality score — a
        # frame mean conflates scene content with rendering.
        "mean_g": report["output_stats"]["mean"][1],
        "resolved_dmax": report.get("dmax"),
    }


def main() -> None:
    if not NC.is_file():
        sys.exit(f"no release binary at {NC} — run `cargo build --release` first")
    if not ASSETS.is_dir():
        sys.exit(f"no assets at {ASSETS} — see CLAUDE.md for the nc-assets symlink")

    rows = []
    with tempfile.TemporaryDirectory(prefix="nc-render-defaults-v2-") as tmp:
        for label, relative, film_base in FRAMES:
            frame = ASSETS / relative
            if not frame.is_file():
                sys.exit(f"missing frame {frame}")
            row = {"frame": label}
            for version, extra in VERSIONS.items():
                out = Path(tmp) / f"{version}-{Path(relative).name}"
                row[version] = measure(frame, film_base, extra, out)
            rows.append(row)

    header = f"| {'frame':30} | {'v1 clipped':>10} | {'v1 mean_g':>9} | {'v2 clipped':>10} | {'v2 mean_g':>9} |"
    print(header)
    print("|" + "|".join("-" * len(part) for part in header.split("|")[1:-1]) + "|")
    for row in rows:
        print(
            f"| {row['frame']:30} | {row['v1']['clipped_pct']:9.2f}% | "
            f"{row['v1']['mean_g']:9.4f} | {row['v2']['clipped_pct']:9.2f}% | "
            f"{row['v2']['mean_g']:9.4f} |"
        )
    print()
    for row in rows:
        print(
            f"{row['frame']}: resolved dmax v1={row['v1']['resolved_dmax']} "
            f"v2={row['v2']['resolved_dmax']}"
        )


if __name__ == "__main__":
    main()
