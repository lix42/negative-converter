#!/usr/bin/env python3
"""Digitize the characteristic curves in `docs/datasheets/` into `curves.json`.

The Kodak still-film sheets are **vector art with no raster layer**, so their published
characteristic curves can be read exactly rather than approximately: the plot frame gives
the density calibration (frame bottom is `D = 0.0`) and the axis ticks the exposure
calibration. Two independent extraction paths — the raw PDF content stream, and
`pdftocairo -svg` — agree to ±0.002 density where both run.

Output is `src/film_stock/curves.json`, the intermediate that
`film_stock::curves`'s pinned Rust literals are audited against. Splitting it in two
is deliberate, and mirrors `pipeline/colorimetry/`: extraction needs poppler and is run by
hand, while *"the literals match the extraction"* is a plain `cargo test` that needs
neither poppler nor network and therefore runs in CI.

    python3 scripts/analysis/digitize_datasheets.py            # rewrite curves.json
    python3 scripts/analysis/digitize_datasheets.py --check    # verify, change nothing

Requires poppler (`pdftotext`, `pdftocairo`, `pdfinfo`): `brew install poppler`.

Traps worth knowing before editing this file:

- **Keep the bézier subdivision.** Some curves are drawn as a handful of long cubics;
  taking only their endpoints leaves 0.7-decade gaps that linear interpolation then cuts
  corners across, which moved Ektar's measured gamma by 3%.
- **Calibrate y off the plot frame, not the axis labels.** A label's *baseline* sits ~2.4 pt
  below its tick, which biases every density by ~0.05.
- **Enforce monotonicity before taking `D-min`.** A digitized curve can dip slightly in its
  flat toe — Gold 200's blue drops 0.03 over its first third of a decade — and taking
  `min(D)` there puts the table's floor above the real base, so every shadow pixel falls off
  the bottom of the table.
"""

from __future__ import annotations

import json
import math
import re
import shutil
import subprocess
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
SHEETS = REPO / "docs" / "datasheets"
OUT = REPO / "src" / "film_stock" / "curves.json"

LOG18 = math.log10(0.18)

# stock key -> (datasheet file, publication, revision, grey aim, white aim, curve-chart
# page hint). Both aims are the *published* Judging Negative Exposures densities (range
# midpoints, Status M, red channel): the grey one fixes the log-exposure axis so that
# inverting the curve puts mid-grey at 0.18, and the pair gives Δ — which the Rust side
# cross-checks against the curve's own slope, an internal-consistency test on the sheet.
STOCKS = {
    "ektar-100": ("e4046-2025-01-ektar-100.pdf", "E-4046", "2025-01", 0.82, 1.18, None),
    "portra-160": ("e4051-2025-01-portra-160.pdf", "E-4051", "2025-01", 0.84, 1.20, None),
    "portra-400": ("e4050-2025-01-portra-400.pdf", "E-4050", "2025-01", 0.82, 1.18, None),
    "portra-800": ("e4040-2025-01-portra-800.pdf", "E-4040", "2025-01", 0.85, 1.10, None),
    "gold-200": ("e7022-2023-06-gold-200.pdf", "E-7022", "2023-06", 0.95, 1.35, None),
    "ultramax-400": ("e7023-2016-02-ultramax-400.pdf", "E-7023", "2016-02", 0.90, 1.30, None),
    "ultramax-800": ("e7024-2007-12-ultramax-800.pdf", "E-7024", "2007-12", 0.85, 1.10, None),
    # One publication, five stocks. Note E-4040 names *two* documents: this 2009 sheet and
    # the 2025 Portra 800 one above, so (publication, revision) is the identifier here —
    # the publication number alone is not.
    "portra-160vc": (
        "e4040-2009-02-portra-160nc-160vc-400nc-400vc-800.pdf",
        "E-4040",
        "2009-02",
        0.87,
        1.28,
        9,
    ),
    "portra-400vc": (
        "e4040-2009-02-portra-160nc-160vc-400nc-400vc-800.pdf",
        "E-4040",
        "2009-02",
        0.87,
        1.28,
        11,
    ),
}

# `mid aim − D-min` per stock, recomputed from the tables and cross-checked against the
# progress log. Not consumed by the render — the curve carries the placement — but it is
# what the Rust side asserts to prove the axis was shifted correctly.
BEZIER_STEPS = 12


def run(*args: str) -> str:
    return subprocess.run(args, capture_output=True, text=True, check=False).stdout


# --------------------------------------------------------------------------- svg parsing


def _mul(a, b):
    return (
        a[0] * b[0] + a[1] * b[2],
        a[0] * b[1] + a[1] * b[3],
        a[2] * b[0] + a[3] * b[2],
        a[2] * b[1] + a[3] * b[3],
        a[4] * b[0] + a[5] * b[2] + b[4],
        a[4] * b[1] + a[5] * b[3] + b[5],
    )


def _transform(s: str):
    m = re.match(r"matrix\(([-\d.eE,\s]+)\)", s.strip())
    if m:
        return tuple(float(x) for x in re.split(r"[,\s]+", m.group(1).strip()))
    m = re.match(r"translate\(([-\d.eE,\s]+)\)", s.strip())
    if m:
        v = [float(x) for x in re.split(r"[,\s]+", m.group(1).strip())]
        return (1, 0, 0, 1, v[0], v[1] if len(v) > 1 else 0)
    m = re.match(r"scale\(([-\d.eE,\s]+)\)", s.strip())
    if m:
        v = [float(x) for x in re.split(r"[,\s]+", m.group(1).strip())]
        return (v[0], 0, 0, v[1] if len(v) > 1 else v[0], 0, 0)
    return (1, 0, 0, 1, 0, 0)


def _bezier(p0, p1, p2, p3, n=BEZIER_STEPS):
    out = []
    for k in range(1, n + 1):
        t = k / n
        u = 1 - t
        out.append(
            (
                u**3 * p0[0] + 3 * u * u * t * p1[0] + 3 * u * t * t * p2[0] + t**3 * p3[0],
                u**3 * p0[1] + 3 * u * u * t * p1[1] + 3 * u * t * t * p2[1] + t**3 * p3[1],
            )
        )
    return out


def _path_points(d: str):
    """The path's **subpaths**, each as its own polyline.

    Splitting at `moveto` matters: one `<path>` can hold several curves, and UltraMax 800
    draws two of its three dye layers that way. Concatenating them produced a single
    doubling-back polyline whose green channel then reduced to two usable points.
    """
    subpaths, cur_path, cur = [], [], None
    for m in re.finditer(r"([MLCZmlcz])([-\d.eE,\s]*)", d):
        op = m.group(1)
        nums = [float(x) for x in re.findall(r"-?\d+\.?\d*(?:[eE]-?\d+)?", m.group(2))]
        if op in "Mm":
            for i in range(0, len(nums) - 1, 2):
                if cur_path:
                    subpaths.append(cur_path)
                cur = (nums[i], nums[i + 1])
                cur_path = [cur]
        elif op in "Ll":
            for i in range(0, len(nums) - 1, 2):
                cur = (nums[i], nums[i + 1])
                cur_path.append(cur)
        elif op in "Cc":
            for i in range(0, len(nums) - 5, 6):
                p1, p2, p3 = (
                    (nums[i], nums[i + 1]),
                    (nums[i + 2], nums[i + 3]),
                    (nums[i + 4], nums[i + 5]),
                )
                cur_path.extend(_bezier(cur or p1, p1, p2, p3))
                cur = p3
    if cur_path:
        subpaths.append(cur_path)
    return subpaths


def svg_polylines(svg: Path):
    """Every path in the SVG, in page coordinates (transforms resolved)."""
    text = svg.read_text()
    stack = [(1, 0, 0, 1, 0, 0)]
    out = []
    for m in re.finditer(r"<g\b([^>]*)>|</g>|<path\b([^>]*)/?>", text):
        tok = m.group(0)
        if tok.startswith("</g"):
            if len(stack) > 1:
                stack.pop()
        elif tok.startswith("<g"):
            tf = re.search(r'transform="([^"]+)"', m.group(1) or "")
            stack.append(_mul(_transform(tf.group(1)), stack[-1]) if tf else stack[-1])
        else:
            attrs = m.group(2) or ""
            dm = re.search(r'\sd="([^"]+)"', attrs)
            if not dm:
                continue
            ctm = stack[-1]
            tf = re.search(r'transform="([^"]+)"', attrs)
            if tf:
                ctm = _mul(_transform(tf.group(1)), ctm)
            for sub in _path_points(dm.group(1)):
                pts = [
                    (ctm[0] * x + ctm[2] * y + ctm[4], ctm[1] * x + ctm[3] * y + ctm[5])
                    for x, y in sub
                ]
                # Two points is a straight line, which is exactly how some sheets draw
                # their plot frame — dropping those made Portra 400's chart undetectable.
                if len(pts) >= 2:
                    out.append(pts)
    return out


def word_boxes(pdf: Path, page: int):
    """(centre-x, centre-y, text) for every word on the page, in SVG coordinates."""
    xml = run("pdftotext", "-bbox", "-f", str(page), "-l", str(page), str(pdf), "-")
    out = []
    for m in re.finditer(
        r'<word xMin="([\d.]+)" yMin="([\d.]+)" xMax="([\d.]+)" yMax="([\d.]+)">([^<]*)</word>',
        xml,
    ):
        x0, y0, x1, y1 = (float(v) for v in m.groups()[:4])
        out.append(((x0 + x1) / 2, (y0 + y1) / 2, m.group(5)))
    return out


# ------------------------------------------------------------------------ chart location


def _candidate_frames(polys):
    """Every plausible plot frame, however it was drawn.

    Some sheets stroke the frame as one closed rectangle and others as four separate axis
    segments — Portra 400's does, and looking only for a rectangle silently skips it. Both
    shapes are enumerated here so a layout change costs nothing.
    """
    frames = []
    for p in polys:
        xs = [q[0] for q in p]
        ys = [q[1] for q in p]
        w, h = max(xs) - min(xs), max(ys) - min(ys)
        if len(p) <= 6 and 150 < w < 230 and 150 < h < 230:
            frames.append((min(xs), min(ys), max(xs), max(ys)))
    segs = [p for p in polys if len(p) == 2]
    horiz = [p for p in segs if abs(p[0][1] - p[1][1]) < 0.3 and abs(p[0][0] - p[1][0]) > 100]
    vert = [p for p in segs if abs(p[0][0] - p[1][0]) < 0.3 and abs(p[0][1] - p[1][1]) > 100]
    for h_seg in horiz:
        hy = h_seg[0][1]
        hx0, hx1 = sorted([h_seg[0][0], h_seg[1][0]])
        for v_seg in vert:
            vx = v_seg[0][0]
            vy0, vy1 = sorted([v_seg[0][1], v_seg[1][1]])
            # Only a pair that actually meets at a corner is a frame edge.
            if min(abs(vx - hx0), abs(vx - hx1)) < 1.5 and min(abs(hy - vy0), abs(hy - vy1)) < 1.5:
                frames.append((hx0, vy0, hx1, vy1))
    return frames


def _stitch(fragments, tol=0.2):
    """Join polylines whose endpoints meet into continuous curves.

    Older sheets (UltraMax 800's 2007 one) draw each dye curve as dozens of short
    subpaths rather than one polyline. Splitting at `moveto` is still right — it is what
    separates two curves sharing a `<path>` — so the pieces are put back together here by
    geometry instead, which is the only signal that says which fragment continues which.
    """
    remaining = [list(f) for f in fragments]
    out = []
    while remaining:
        cur = remaining.pop(0)
        joined = True
        while joined:
            joined = False
            for i, cand in enumerate(remaining):
                for c in (cand, cand[::-1]):
                    if abs(c[0][0] - cur[-1][0]) < tol and abs(c[0][1] - cur[-1][1]) < tol:
                        cur.extend(c[1:])
                        remaining.pop(i)
                        joined = True
                        break
                    if abs(c[-1][0] - cur[0][0]) < tol and abs(c[-1][1] - cur[0][1]) < tol:
                        cur[:0] = c[:-1]
                        remaining.pop(i)
                        joined = True
                        break
                if joined:
                    break
        out.append(cur)
    return out


def find_chart(polys, labels):
    """The characteristic-curve plot: a square frame holding exactly three long curves,
    with a `DENSITY` axis title and density labels down its left side.

    Several charts on a page share the "square frame, three curves" shape — spectral
    sensitivity most of all — so the axis title is what disambiguates them. Getting this
    wrong silently reads log-sensitivity as density.
    """
    for fx0, fy0, fx1, fy1 in _candidate_frames(polys):
        contained = [
            q
            for q in polys
            if len(q) >= 2
            and fx0 - 2 <= min(a[0] for a in q)
            and max(a[0] for a in q) <= fx1 + 2
            and fy0 - 2 <= min(a[1] for a in q)
            and max(a[1] for a in q) <= fy1 + 2
        ]
        # A dye curve spans most of the plot; anything short after stitching is a tick,
        # a legend rule or a label stroke.
        inside = [
            q
            for q in _stitch(contained)
            if len(q) >= 8
            and max(a[0] for a in q) - min(a[0] for a in q) > 0.5 * (fx1 - fx0)
        ]
        ylab = [
            (y, float(t))
            for x, y, t in labels
            if fx0 - 30 < x < fx0 - 2 and fy0 - 8 < y < fy1 + 8 and re.fullmatch(r"\d\.\d", t)
        ]
        density_axis = any(t == "DENSITY" and fx0 - 45 < x < fx0 for x, y, t in labels)
        if len(inside) == 3 and len(ylab) >= 4 and density_axis:
            return (fx0, fy0, fx1, fy1), inside
    return None


def digitize(pdf: Path, page_hint: int | None, mid_aim: float):
    """The three per-channel tables for one stock, as `(relative logE, D above D-min)`."""
    pages = int(re.search(r"Pages:\s+(\d+)", run("pdfinfo", str(pdf))).group(1))
    candidates = [page_hint] if page_hint else range(1, pages + 1)
    for page in candidates:
        text = run("pdftotext", "-layout", "-f", str(page), "-l", str(page), str(pdf), "-")
        if not re.search(r"LOG EXPOSURE", text, re.I):
            continue
        svg = Path(f"/tmp/nc-digitize-{pdf.stem}-{page}.svg")
        subprocess.run(
            ["pdftocairo", "-svg", "-f", str(page), "-l", str(page), str(pdf), str(svg)],
            check=True,
        )
        labels = word_boxes(pdf, page)
        found = find_chart(svg_polylines(svg), labels)
        if not found:
            continue
        (fx0, fy0, fx1, fy1), curves = found
        # The frame spans 0..4 density on every Kodak sheet seen; the ticks give the
        # decade width. Both are read off geometry, never off label baselines.
        pt_per_density = (fy1 - fy0) / 4.0
        xlab = sorted(
            x
            for x, y, t in labels
            if fy1 + 2 < y < fy1 + 22 and fx0 - 14 < x < fx1 + 14 and re.fullmatch(r"-?\d\.\d", t)
        )
        if len(xlab) < 2:
            continue
        pt_per_decade = (xlab[-1] - xlab[0]) / (len(xlab) - 1)

        # Blue is the densest layer of a masked colour negative, red the least, so the
        # curves order top-to-bottom B, G, R in the plot (SVG y grows downward).
        ordered = sorted(curves, key=lambda p: p[0][1])
        raw = {}
        for name, poly in zip("BGR", ordered):
            poly = sorted(poly, key=lambda q: q[0])
            raw[name] = [
                ((q[0] - fx0) / pt_per_decade, (fy1 - q[1]) / pt_per_density) for q in poly
            ]

        # Where the published mid-grey aim sits on the red curve fixes the axis.
        red = raw["R"]
        red_dmin = _monotone(red)[0][1]
        target = mid_aim - red_dmin
        x_mid = _x_at(_monotone(red), target)
        if x_mid is None:
            continue

        out = {}
        for name in "RGB":
            mono = _monotone(raw[name])
            dmin = mono[0][1]
            out[name] = {
                "d_min_status_m": round(dmin, 4),
                "points": [
                    [round(x - x_mid + LOG18, 6), round(d - dmin, 6)] for x, d in mono
                ],
            }
        return out, page
    return None, None


def _monotone(points):
    """Strictly increasing in **both** coordinates, from the leftmost point onward.

    Density alone is not enough: a digitized curve can carry two points at the same log
    exposure, which leaves a zero-width segment that every interpolator divides by. The
    Rust side asserts both directions, so the JSON has to satisfy both.
    """
    out = [points[0]]
    for x, d in points[1:]:
        if d > out[-1][1] + 1e-6 and x > out[-1][0] + 1e-6:
            out.append((x, d))
    return out


def _x_at(points, density):
    for i in range(len(points) - 1):
        (x0, d0), (x1, d1) = points[i], points[i + 1]
        lo, hi = d0 - points[0][1], d1 - points[0][1]
        if (lo - density) * (hi - density) <= 0 and d0 != d1:
            return x0 + (density - lo) / (hi - lo) * (x1 - x0)
    return None


# ---------------------------------------------------------------------------- generic


def build_generic(stocks: dict) -> dict:
    """The average of every measured stock, resampled on their common exposure range.

    The tables are already aligned at mid-grey, so averaging them at fixed log exposure
    averages the *shapes*: the result carries the mean gamma and the mean mid-above-base
    placement, which is what an unnamed C-41 stock should get.
    """
    measured = [v for k, v in stocks.items() if k != "generic-c41"]
    lo = max(max(v["channels"][c]["points"][0][0] for c in "RGB") for v in measured)
    hi = min(min(v["channels"][c]["points"][-1][0] for c in "RGB") for v in measured)
    n = 48
    grid = [lo + (hi - lo) * i / (n - 1) for i in range(n)]

    def at(points, x):
        for i in range(len(points) - 1):
            if points[i][0] <= x <= points[i + 1][0]:
                t = (x - points[i][0]) / (points[i + 1][0] - points[i][0])
                return points[i][1] + t * (points[i + 1][1] - points[i][1])
        return points[0][1] if x < points[0][0] else points[-1][1]

    channels = {}
    for c in "RGB":
        vals = [
            [x, sum(at(v["channels"][c]["points"], x) for v in measured) / len(measured)]
            for x in grid
        ]
        zero = vals[0][1]
        channels[c] = {
            "d_min_status_m": None,
            "points": [[round(x, 6), round(max(0.0, y - zero), 6)] for x, y in vals],
        }
    return {
        "publication": "derived",
        "revision": "2026-09-04",
        "aim_grey_red": None,
        "aim_white_red": None,
        "channels": channels,
    }


RUST_OUT = REPO / "src" / "film_stock" / "curves.rs"

RUST_HEADER = '''//! Pinned characteristic-curve data — the literals `super`'s evidence tests read.
//!
//! **Generated.** `python3 scripts/analysis/digitize_datasheets.py --emit-rust` rewrites
//! this file from `curves.json`, which is itself digitized from the publications in
//! `docs/datasheets/`. `curves_match_the_digitized_json` audits the two against each
//! other, so a hand edit here is caught rather than silently rendered.
//!
//! Each table is `(relative log exposure, density above D-min)` for one dye layer. The
//! log-exposure axis is shifted so the stock's own **mid-grey aim density sits at
//! `log10(0.18)`**: relative scene exposure with mid-grey at 0.18, with no separate
//! anchor to resolve.
//!
//! Two invariants, both asserted in `super::tests`: **strictly increasing** in both
//! coordinates, and a **first point at `D′ = 0`** (the film base).
//!
//! No render reads these tables; they are the evidence for the fixed decode's constants.
//! Re-derive from the publication rather than hand-tuning.

// These are digitized measurements, not mathematical constants. `clippy::approx_constant`
// fires on `0.78539` (a log-exposure coordinate) because it is close to π/4, which is a
// coincidence of the plot's axis calibration and nothing more — "use the constant
// directly" would replace a reading with an unrelated number.
#![allow(clippy::approx_constant)]

/// One stock's published response, plus the provenance that makes every number in it
/// re-derivable from the named publication.
pub struct StockCurves {
    /// Wire name, matching [`crate::types::FilmStock`]'s serde spelling.
    pub name: &'static str,
    /// Kodak publication id, or `derived` for the generic average. **Not unique on its
    /// own** — E-4040 names both the 2009 five-stock Portra sheet and the 2025 Portra 800
    /// one — so the identifier is the pair with `revision`.
    pub publication: &'static str,
    /// Publication revision the tables were read from.
    pub revision: &'static str,
    /// The published *Judging Negative Exposures* grey-card and paper-white aim densities
    /// (Status M, red, range midpoints), or `None` for the derived generic. Their difference
    /// is `Δ`, which `aim_table_agrees_with_the_curve` checks against the curve's own slope
    /// — an internal-consistency test on the **sheet**, not on the extraction.
    pub aims: Option<[f32; 2]>,
    /// Status M `D-min` per channel, from the same plot. **Diagnostic only** — the tables
    /// are base-relative and the measured roll base stays authoritative
    /// (`film-stock-profiles` Constraint 1), so no render path reads this. `None` for the
    /// derived generic.
    pub d_min: Option<[f32; 3]>,
    /// `[R, G, B]`, each `(relative log exposure, density above D-min)`.
    pub channels: [&'static [(f32, f32)]; 3],
}
'''


def emit_rust(stocks: dict) -> str:
    order = ["generic-c41", *sorted(k for k in stocks if k != "generic-c41")]
    out = [RUST_HEADER]
    for key in order:
        ident = key.replace("-", "_").upper()
        for ch in "RGB":
            pts = stocks[key]["channels"][ch]["points"]
            out.append("#[rustfmt::skip]")
            out.append(f"const {ident}_{ch}: &[(f32, f32)] = &[")
            for i in range(0, len(pts), 4):
                out.append("    " + " ".join(f"({p[0]}, {p[1]})," for p in pts[i : i + 4]))
            out.append("];")
        out.append("")
    out.append("/// Every stock the registry knows, in `FilmStock::ALL`'s order.")
    out.append("pub const STOCKS: &[StockCurves] = &[")
    for key in order:
        ident = key.replace("-", "_").upper()
        dm = [stocks[key]["channels"][c]["d_min_status_m"] for c in "RGB"]
        d_min = "None" if dm[0] is None else f"Some([{dm[0]}, {dm[1]}, {dm[2]}])"
        g, w = stocks[key]["aim_grey_red"], stocks[key]["aim_white_red"]
        aims = "None" if g is None else f"Some([{g}, {w}])"
        out.append("    StockCurves {")
        out.append(f'        name: "{key}",')
        out.append(f'        publication: "{stocks[key]["publication"]}",')
        out.append(f'        revision: "{stocks[key]["revision"]}",')
        out.append(f"        aims: {aims},")
        out.append(f"        d_min: {d_min},")
        out.append(f"        channels: [{ident}_R, {ident}_G, {ident}_B],")
        out.append("    },")
    out.append("];")
    return "\n".join(out) + "\n"


def main() -> int:
    check = "--check" in sys.argv
    if "--emit-rust" in sys.argv:
        stocks = json.loads(OUT.read_text())
        RUST_OUT.write_text(emit_rust(stocks))
        print(f"wrote {RUST_OUT.relative_to(REPO)} from {OUT.relative_to(REPO)}")
        return 0
    for tool in ("pdftotext", "pdftocairo", "pdfinfo"):
        if not shutil.which(tool):
            print(f"error: {tool} not found — install poppler (brew install poppler)")
            return 2

    stocks: dict = {}
    for key, (name, pub, rev, mid, white, page) in STOCKS.items():
        pdf = SHEETS / name
        if not pdf.is_file():
            print(f"error: {pdf} is missing")
            return 2
        channels, page_used = digitize(pdf, page, mid)
        if channels is None:
            print(f"error: no characteristic-curve chart found in {name} for {key}")
            return 2
        stocks[key] = {
            "publication": pub,
            "revision": rev,
            "aim_grey_red": mid,
            "aim_white_red": white,
            "channels": channels,
        }
        counts = "/".join(str(len(channels[c]["points"])) for c in "RGB")
        print(f"  {key:14} {pub:8} {rev}  page {page_used}  points R/G/B {counts}")
    stocks["generic-c41"] = build_generic(stocks)

    # A stable key order keeps the diff readable when one stock is re-derived.
    ordered = {k: stocks[k] for k in ["generic-c41", *sorted(k for k in stocks if k != "generic-c41")]}
    text = json.dumps(ordered, indent=1) + "\n"
    if check:
        current = OUT.read_text() if OUT.is_file() else ""
        if current != text:
            print(f"error: {OUT.relative_to(REPO)} is stale — re-run without --check")
            return 1
        print(f"ok: {OUT.relative_to(REPO)} matches the datasheets")
        return 0
    OUT.write_text(text)
    print(f"wrote {OUT.relative_to(REPO)}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
