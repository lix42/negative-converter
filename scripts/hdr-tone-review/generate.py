#!/usr/bin/env python3
"""Generate the HDR visual-review set for output/display-tone-mapping.

Drives the `nc` binary end to end and writes real gain-map JPEGs, which macOS
(Safari/Chrome, HDR display) decodes as HDR. Deliberately NOT converted to PNG:
that would flatten the very thing under review.

Three configs per frame, which are the three outcomes the task established:
  default       shipped sigmoid + shipped shoulder      -> gain map inert (1.00x)
  s0-shoulder   shoulder-less recon + shipped shoulder  -> saturated (~4.87x, plateau)
  s0-reinhard   shoulder-less recon + unbounded tone    -> separated (~4.79x)

`GainMapMax` is NOT the metric — see the README. It barely separates the two live
configs (4.87x vs 4.79x, identical on every frame); the plateau share does. The 3.35x
figure that used to sit here is `tests/fixtures/hdr-48bit.tif`, a synthetic frame with
less highlight than the real scans, and quoting it here made the script look broken
against its own output.

Env: NC_TONEMAP_FRAMES (default P3,P4,G2,E2), NC_TONEMAP_OUT (default ../temp/…).
"""
import json, os, pathlib, subprocess, sys

# Resolved from this file, not from the cwd: a `git rev-parse` on the cwd would make the
# script operate on whatever repo you happen to be standing in, and raise CalledProcessError
# outside one. The layout is scripts/hdr-tone-review/generate.py, so the root is two up.
REPO = pathlib.Path(__file__).resolve().parents[2]
NC = REPO/"target/release/nc"
ASSETS = (REPO/"../nc-assets").resolve()
OUT = pathlib.Path(os.environ.get(
    "NC_TONEMAP_OUT", REPO/"../temp/tonemap-hdr-review")).resolve()
FRAMES = os.environ.get("NC_TONEMAP_FRAMES","P3,P4,G2,E2").split(",")

CONFIGS = [
    ("default",     []),
    ("s0-shoulder", ["--sigmoid-shoulder","0"]),
    ("s0-reinhard", ["--sigmoid-shoulder","0","--display-tone","reinhard"]),
]

def _png_gray(path: pathlib.Path):
    """Decode an 8-bit grayscale PNG with the stdlib. Enough for a gain map."""
    import struct, zlib
    d = path.read_bytes()
    pos, idat, hdr = 8, b"", None
    while pos < len(d):
        ln, typ = struct.unpack(">I4s", d[pos:pos+8])
        # All seven IHDR fields: the last three (compression, filter, interlace) were
        # skipped, so an interlaced PNG fell past the bitdepth/colortype guard and decoded
        # to garbage — feeding the histogram a plausible-looking wrong plateau share.
        if typ == b"IHDR": hdr = struct.unpack(">IIBBBBB", d[pos+8:pos+21])
        elif typ == b"IDAT": idat += d[pos+8:pos+8+ln]
        pos += 12 + ln
    w, h, bd, ct, comp, filt, interlace = hdr
    if (bd, ct, comp, filt, interlace) != (8, 0, 0, 0, 0): return None
    raw = zlib.decompress(idat)
    out = bytearray(w * h)
    prev = bytearray(w)
    i = 0
    for y in range(h):
        f = raw[i]; i += 1
        line = bytearray(raw[i:i+w]); i += w
        if f == 1:
            for x in range(1, w): line[x] = (line[x] + line[x-1]) & 0xFF
        elif f == 2:
            for x in range(w): line[x] = (line[x] + prev[x]) & 0xFF
        elif f == 3:
            for x in range(w):
                line[x] = (line[x] + ((line[x-1] if x else 0) + prev[x]) // 2) & 0xFF
        elif f == 4:
            for x in range(w):
                a = line[x-1] if x else 0; b = prev[x]; c = prev[x-1] if x else 0
                pa, pb, pc = abs(b-c), abs(a-c), abs(a+b-2*c)
                pred = a if (pa <= pb and pa <= pc) else (b if pb <= pc else c)
                line[x] = (line[x] + pred) & 0xFF
        out[y*w:(y+1)*w] = line
        prev = line
    return out


class MeasurementFailed(Exception):
    """A measurement this directory exists to produce could not be taken."""


def _extract_gain_map(jpeg: pathlib.Path) -> bytes:
    """The second MPF image's bytes. Extracted **once** per file and shared.

    Both metrics used to run their own `exiftool -b -GainMapImage` into the same
    `OUT/_gm.jpg`, which doubled the subprocess cost and left that temp file sitting in
    the directory the review page is served from whenever a run was interrupted.
    """
    r = subprocess.run(["exiftool", "-b", "-GainMapImage", str(jpeg)], capture_output=True)
    if r.returncode or not r.stdout:
        raise MeasurementFailed(
            f"exiftool could not extract the gain map from {jpeg.name} "
            f"(exit {r.returncode}). Is exiftool installed?")
    return r.stdout


def gain_map_shape(gm_bytes: bytes, tag: str):
    """Separation metrics off the stored gain map itself.

    `GainMapMax` is a single extremum and it saturates: on real frames both the
    shouldered and the unbounded render sit near the ceiling, so it cannot tell them
    apart. What differs is the *distribution* — a plateau puts every highlight on one
    code, which is the flat blob in numbers.

    **Raises rather than returning `None`.** This is the column the README tells the
    reader to use, so a run with a broken `exiftool` or `sips` must not produce a page
    that looks complete and answers the review question with `nan`.
    """
    gm, png = OUT/f"_gm-{tag}.jpg", OUT/f"_gm-{tag}.png"
    try:
        gm.write_bytes(gm_bytes)
        r = subprocess.run(["sips", "-s", "format", "png", str(gm), "--out", str(png)],
                           capture_output=True)
        if r.returncode:
            raise MeasurementFailed(f"sips could not convert the gain map for {tag}")
        px = _png_gray(png)
        if not px:
            raise MeasurementFailed(
                f"the gain map for {tag} is not the 8-bit non-interlaced grayscale PNG "
                f"this decoder handles")
    finally:
        gm.unlink(missing_ok=True)
        png.unlink(missing_ok=True)

    hist = [0]*256
    for v in px: hist[v] += 1
    n = len(px)
    top = hist[255]
    # Spread across the top decile of the gain map: how many codes the brightest
    # tenth of the image occupies. A plateau collapses this toward zero.
    want_lo, want_hi, acc, p90, p999 = 0.90*n, 0.999*n, 0, None, None
    for code, c in enumerate(hist):
        acc += c
        if p90 is None and acc >= want_lo: p90 = code
        if p999 is None and acc >= want_hi: p999 = code
    return {"top_code_share": 100.0*top/n, "p90_code": p90, "p999_code": p999,
            "code_spread": (p999 - p90) if (p90 is not None and p999 is not None) else None}


def gain_map_max(gm_bytes: bytes, tag: str):
    """`GainMapMax` (log2) from the gain map image's XMP. Not the metric — see the README."""
    gm = OUT/f"_gmx-{tag}.jpg"
    try:
        gm.write_bytes(gm_bytes)
        r = subprocess.run(["exiftool", "-b", "-XMP", str(gm)], capture_output=True)
        if r.returncode:
            raise MeasurementFailed(f"exiftool could not read the gain map XMP for {tag}")
    finally:
        gm.unlink(missing_ok=True)
    for tok in r.stdout.decode("utf-8", "replace").replace(">", "\n").split():
        if "GainMapMax" in tok:
            # Positional, and assumes the single-valued attribute form nc writes.
            # Ultra HDR v1 permits a per-channel rdf:Seq here; not handled, deliberately —
            # this number is not the metric, so a latent parse gap costs nothing.
            try: return float(tok.split('"')[1])
            except (IndexError, ValueError):
                raise MeasurementFailed(f"unparseable GainMapMax for {tag}") from None
    raise MeasurementFailed(f"no GainMapMax in the gain map XMP for {tag}")


def main():
    if not NC.exists(): sys.exit(f"build the release binary first: {NC}")
    if not (ASSETS/"manifest.json").exists(): sys.exit(f"no assets at {ASSETS}")
    fx = json.loads((REPO/"scripts/sigmoid-baseline/fixtures.json").read_text())
    OUT.mkdir(parents=True, exist_ok=True)

    page = []
    for key in FRAMES:
        f = fx["frames"].get(key)
        if not f: print(f"{key}: not in fixtures.json, skipped"); continue
        roll = f["roll"]
        dmin = fx["rolls"][roll]["dmin"]
        src = ASSETS/"rolls"/roll/f["file"]
        if not src.exists(): print(f"{key}: {src} missing, skipped"); continue
        row = {"key":key,"file":f["file"],"roll":roll,"metrics":{}}
        for name,extra in CONFIGS:
            dest = OUT/f"{key}-{name}.jpg"
            cmd = [str(NC),"convert",str(src),
                   "--output-preset","gain-map-hdr",
                   "--film-base",",".join(str(c) for c in dmin),
                   "--d-max",str(f["roll_dmax"]),
                   "-o",str(dest),"--report","json",*extra]
            r = subprocess.run(cmd,capture_output=True,text=True)
            if r.returncode:
                print(f"{key}/{name}: nc exited {r.returncode}: {r.stderr.strip()[:200]}")
                continue
            rep = json.loads(r.stdout)
            gm_bytes = _extract_gain_map(dest)
            g = gain_map_max(gm_bytes, f"{key}-{name}")
            shape = gain_map_shape(gm_bytes, f"{key}-{name}")
            row["metrics"][name] = {
                "gain_log2": g,
                "gain_linear": 2.0**g,
                "tone": rep.get("output_render",{}).get("display_tone"),
                "bytes": dest.stat().st_size,
                **shape,
            }
            lin = row["metrics"][name]["gain_linear"]
            print(f"{key:4} {name:12} GainMapMax {round(g,5)!s:>9} "
                  f"= {lin:.4f}x   "
                  f"top-code {shape['top_code_share']:6.2f}%  "
                  f"spread p90..p99.9 {shape['code_spread']!s:>4} codes")
        page.append(row)

    if not page:
        sys.exit(f"no frames rendered from {FRAMES!r} — check NC_TONEMAP_FRAMES "
                 f"(keys look like G1-G3, E1-E3, P1-P4). Writing no page: review.html "
                 f"cannot render an empty frame list and would show a blank page.")
    tmpl = (REPO/"scripts/hdr-tone-review/review.html").read_text()
    (OUT/"index.html").write_text(tmpl.replace("__DATA__", json.dumps(
        {"configs":[c for c,_ in CONFIGS], "frames":page}, indent=1)))
    print(f"\nwrote {OUT/'index.html'}")


if __name__ == "__main__":
    try:
        main()
    except MeasurementFailed as e:
        # Non-zero exit with one actionable line, not a traceback: the failure is an
        # environment problem (exiftool/sips), not a bug for the reader to debug.
        sys.exit(f"measurement failed: {e}")
