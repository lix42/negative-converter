"""Pixel-derived metrics for one converted image, whatever produced it.

Every other number this toolkit reports is read out of `nc`'s own JSON report,
which means it exists only for nc outputs. This module reads the output *image*
instead, so an NLP conversion, a SmartConvert TIFF, or an export the user edited
by hand can be measured on the same footing as an nc render.

Two rules make the numbers comparable at all, and both are load-bearing:

* **The colour space is declared, never guessed.** A TIFF says almost nothing
  about its own transfer function — the NLP exports carry 32-bit float samples
  with a *linear* sRGB profile, while nc writes transfer-encoded u16, and
  SmartConvert writes u16 with no profile at all. Measuring one as the other
  produces a plausible, wrong table, so an unstated space is refused.
* **Measurement happens in linear light.** Percentiles of transfer-encoded code
  values describe the encoding as much as the picture.

Tone lives in log2 stops relative to 0.18 (mid grey), the domain where an
exposure difference is an offset and a contrast difference is a slope. The
*geometric* mean is the exposure statistic; the arithmetic mean of display
values is not.

Only derived statistics leave this module. Sample pixels never reach a report,
a committed artifact, or an agent context (CLAUDE.md).
"""
from __future__ import annotations

import hashlib
import json
import math
import sys
from pathlib import Path

SCHEMA = 1

#: Mid grey. The anchor for every value reported in stops.
MID_GREY = 0.18

#: Diffuse white, in stops above mid grey: log2(1 / 0.18).
DIFFUSE_WHITE_STOPS = math.log2(1.0 / MID_GREY)

#: Reported floats are rounded here so artifacts diff cleanly across machines.
#: Reductions over tens of millions of samples can differ in their last bits
#: between platforms (vectorized pairwise summation is not associative); six
#: decimals is far coarser than that and far finer than anything photographic.
ROUND = 6

# -- colorimetry --------------------------------------------------------------
#
# Transcribed from `src/pipeline/colorimetry/definitions.rs`, which is the
# repository's single source of truth for standards-based colorimetry. Nothing
# here may be edited independently: `test_metrics.py` re-reads that Rust file and
# fails if these drift from it. That is also why `definitions::ADOBE_RGB` exists
# at all — nc renders to no such space, but references arrive in it, and a set of
# primaries living only in this file would be a second source of truth by
# construction. Add the definition there, then transcribe it here.

PRIMARIES: dict[str, tuple[tuple[float, float], ...]] = {
    "rec709": ((0.640, 0.330), (0.300, 0.600), (0.150, 0.060)),
    "display-p3": ((0.680, 0.320), (0.265, 0.690), (0.150, 0.060)),
    # Shares rec709's red and blue exactly; only green moves. See
    # `definitions::ADOBE_RGB`, which pins that relationship in its own tests.
    "adobe-rgb": ((0.640, 0.330), (0.210, 0.710), (0.150, 0.060)),
    "bt2020": ((0.708, 0.292), (0.170, 0.797), (0.131, 0.046)),
    "prophoto": ((0.7347, 0.2653), (0.1596, 0.8404), (0.0366, 0.0001)),
}

WHITE: dict[str, tuple[float, float]] = {
    "d65": (0.3127, 0.3290),
    "d50": (0.3457, 0.3585),
}

#: Bradford cone response, inverted numerically in f64 — `definitions::BRADFORD`,
#: the canonical convention there. (`BRADFORD_PUBLISHED_INVERSE` exists in the
#: Rust only to reproduce one frozen identifier and must not be used for new
#: work, so it is not mirrored here.)
BRADFORD = (
    (0.8951, 0.2664, -0.1614),
    (-0.7502, 1.7135, 0.0367),
    (0.0389, -0.0685, 1.0296),
)


class Space:
    """A declarable input colour space: primaries, adopted white, transfer."""

    def __init__(self, name: str, primaries: str, white: str, transfer: str,
                 note: str = "") -> None:
        self.name = name
        self.primaries = primaries
        self.white = white
        self.transfer = transfer
        self.note = note

    def describe(self) -> dict:
        return dict(declared=self.name, primaries=self.primaries,
                    white=self.white, transfer=self.transfer)


SPACES: dict[str, Space] = {
    "srgb": Space("srgb", "rec709", "d65", "srgb"),
    "linear-srgb": Space("linear-srgb", "rec709", "d65", "linear"),
    "display-p3": Space("display-p3", "display-p3", "d65", "srgb"),
    "linear-display-p3": Space("linear-display-p3", "display-p3", "d65", "linear"),
    "adobe-rgb": Space("adobe-rgb", "adobe-rgb", "d65", "adobe-rgb"),
    "linear-adobe-rgb": Space("linear-adobe-rgb", "adobe-rgb", "d65", "linear"),
    "prophoto": Space("prophoto", "prophoto", "d50", "prophoto"),
    "linear-prophoto": Space("linear-prophoto", "prophoto", "d50", "linear"),
    "linear-bt2020": Space("linear-bt2020", "bt2020", "d65", "linear"),
}

#: Spaces named here are recognized and refused, so the message can say *why*
#: rather than "unknown space". PQ and HLG are display-referred and absolute:
#: normalizing them against a declared reference-white luminance is a policy
#: decision that belongs with the HDR acceptance work, not a decode.
REFUSED: dict[str, str] = {
    "pq": "PQ is absolute and display-referred; comparing it with an SDR "
          "rendition needs the reference-white normalization specified in "
          "docs/tasks/analysis/display-output-acceptance.md, which this command "
          "does not implement yet",
    "hlg": "HLG is display-referred and its OOTF depends on a declared peak "
           "luminance; see the note for pq",
    "rec2020-pq": "see pq",
    "rec2020-hlg": "see pq",
}


def _mat_mul(a, b):
    return tuple(tuple(sum(a[i][k] * b[k][j] for k in range(3)) for j in range(3))
                 for i in range(3))


def _mat_vec(m, v):
    return tuple(sum(m[i][k] * v[k] for k in range(3)) for i in range(3))


def _inverse(m):
    """Exact 3x3 inverse by cofactors, in binary64.

    Hand-rolled rather than `numpy.linalg.inv` on purpose: these matrices are
    compared against the Rust audit's binary64 derivation to 1e-12, and a LAPACK
    routine's pivoting is not the same arithmetic. The bulk pixel work below is
    numpy's; this is not.
    """
    (a, b, c), (d, e, f), (g, h, i) = m
    det = a * (e * i - f * h) - b * (d * i - f * g) + c * (d * h - e * g)
    if det == 0.0:
        raise ValueError("singular matrix")
    return (
        ((e * i - f * h) / det, (c * h - b * i) / det, (b * f - c * e) / det),
        ((f * g - d * i) / det, (a * i - c * g) / det, (c * d - a * f) / det),
        ((d * h - e * g) / det, (b * g - a * h) / det, (a * e - b * d) / det),
    )


def _xyz_of(xy: tuple[float, float]) -> tuple[float, float, float]:
    """The XYZ of a chromaticity, normalized to Y = 1."""
    x, y = xy
    return (x / y, 1.0, (1.0 - x - y) / y)


def rgb_to_xyz(primaries: str, white: str):
    """Linear RGB -> XYZ under the space's own adopted white."""
    cols = tuple(_xyz_of(p) for p in PRIMARIES[primaries])
    m = tuple(tuple(cols[j][i] for j in range(3)) for i in range(3))
    scale = _mat_vec(_inverse(m), _xyz_of(WHITE[white]))
    return tuple(tuple(m[i][j] * scale[j] for j in range(3)) for i in range(3))


def bradford_adaptation(src_white: str, dst_white: str):
    """Bradford chromatic adaptation between two adopted whites."""
    if src_white == dst_white:
        return ((1.0, 0.0, 0.0), (0.0, 1.0, 0.0), (0.0, 0.0, 1.0))
    src = _mat_vec(BRADFORD, _xyz_of(WHITE[src_white]))
    dst = _mat_vec(BRADFORD, _xyz_of(WHITE[dst_white]))
    ratio = tuple(tuple((dst[i] / src[i]) if i == j else 0.0 for j in range(3))
                  for i in range(3))
    return _mat_mul(_inverse(BRADFORD), _mat_mul(ratio, BRADFORD))


def rgb_to_xyz_d65(space: Space):
    """Linear RGB in `space` -> XYZ adapted to D65.

    Every image is measured against one white so that luminance means the same
    thing across producers; ProPhoto (D50) is the only supported space where the
    adaptation is not the identity.
    """
    return _mat_mul(bradford_adaptation(space.white, "d65"),
                    rgb_to_xyz(space.primaries, space.white))


def luminance_weights(space: Space) -> tuple[float, float, float]:
    """The Y row of the D65-adapted primary matrix.

    Derived from the primaries, deliberately, even for BT.2020: that space's
    *tabulated* `[0.2627, 0.6780, 0.0593]` is the non-constant-luminance luma
    used on transfer-encoded values for Y'CbCr, not a linear-light luminance
    weighting, and the two disagree by ~2e-6. See the note on
    `definitions::BT2020_LUMA_TABULATED`.
    """
    return rgb_to_xyz_d65(space)[1]


# -- transfer functions -------------------------------------------------------


def _decode_transfer(array, transfer: str):
    """Decode encoded values to linear.

    Works on one full-frame temporary (`np.abs`) and mutates it in place. The
    obvious spelling — `np.where(v <= t, low(v), high(v))` — allocates four or
    five full-frame copies instead, because numpy evaluates both arms eagerly and
    `copysign` allocates again. At the 10368x7200 scans this tool is pointed at,
    each of those is ~900 MB, so the difference is not academic.

    Negatives are decoded symmetrically (`sign(v) * f(|v|)`). They occur only in
    float files, where they are out-of-gamut excursions rather than errors, and
    folding them to zero would silently move the shadow statistics this module
    exists to report.
    """
    import numpy as np

    if transfer == "linear":
        return array
    magnitude = np.abs(array)

    if transfer == "srgb":
        # IEC 61966-2-1. The linear segment is evaluated only on the samples it
        # covers, so the shape below is "power law everywhere, then patch the toe".
        toe = magnitude <= np.float32(0.04045)
        patched = magnitude[toe] / np.float32(12.92)
        magnitude += np.float32(0.055)
        magnitude /= np.float32(1.055)
        np.power(magnitude, np.float32(2.4), out=magnitude)
        magnitude[toe] = patched
    elif transfer == "adobe-rgb":
        # Adobe RGB (1998): a pure 563/256 power law. Unlike sRGB and ProPhoto it
        # has no linear segment near black, so there is no threshold to get wrong
        # — and no floor, which is why a near-zero sample stays near zero rather
        # than being lifted onto a linear ramp.
        np.power(magnitude, np.float32(563.0 / 256.0), out=magnitude)
    elif transfer == "prophoto":
        # ISO 22028-2 (ROMM RGB): gamma 1.8 above 16 * (1/512).
        toe = magnitude < np.float32(16.0 / 512.0)
        patched = magnitude[toe] / np.float32(16.0)
        np.power(magnitude, np.float32(1.8), out=magnitude)
        magnitude[toe] = patched
    else:
        raise ValueError(f"unknown transfer function: {transfer}")

    return np.copysign(magnitude, array, out=magnitude)


# -- reading ------------------------------------------------------------------


class MetricsError(Exception):
    """A refusal the caller should print and exit non-zero on."""


def require_dependencies() -> None:
    try:
        import numpy  # noqa: F401
        import tifffile  # noqa: F401
    except ImportError as error:
        raise MetricsError(
            f"{error.name} is required by `nctool metrics` and is not importable. "
            "The rest of the toolkit is stdlib-only; this command is not. Set it "
            "up with:\n"
            "    python3 -m venv .venv\n"
            "    .venv/bin/python -m pip install -r scripts/analysis/requirements.txt\n"
            "and run the command with .venv/bin/python") from error


def read_image(path: Path):
    """Read one single-page RGB TIFF as float32 in [0, 1]-nominal encoded units.

    Integer samples are divided by their full-scale code value, so a u8 and a u16
    export of the same picture measure the same. Float samples are taken as they
    are — a float file's 1.0 already means display white, and rescaling by an
    observed maximum would silently normalize away the exposure difference this
    tool is built to measure.
    """
    import numpy as np
    import tifffile

    try:
        with tifffile.TiffFile(str(path)) as handle:
            pages = len(handle.pages)
            if pages != 1:
                raise MetricsError(
                    f"{path.name}: {pages} TIFF pages; this command measures "
                    "single-page output images, not the multi-IFD HDRi scan layout. "
                    "Point it at a converted positive")
            page = handle.pages[0]
            planar = int(getattr(page, "planarconfig", 1) or 1)
            samples = int(page.samplesperpixel)
            declared = (int(page.imagelength), int(page.imagewidth))
            array = handle.asarray()
    except MetricsError:
        raise
    except Exception as error:
        # Anything the reader itself raises — not a TIFF at all, truncated, an
        # unsupported compressor — becomes the same clean refusal as every other
        # bad input. Pointing this at a JPEG or PNG export is the likeliest
        # mistake there is, and a library traceback is not an answer.
        raise MetricsError(
            f"{path.name}: cannot be read as a TIFF ({type(error).__name__}: "
            f"{error}). This command reads single-page RGB TIFFs; export the "
            "reference as TIFF rather than JPEG or PNG") from error

    # PLANARCONFIG 2 stores each channel in its own plane, and the array then
    # arrives as (samples, height, width). Its *shape* still passes a naive
    # `ndim == 3 and shape[2] >= 3` test, so such a file was accepted and measured
    # as a 3-row image with the rest of the rows silently dropped — a plausible
    # wrong answer, which is the one failure this module must not have. The
    # file's own tags are the authority, so they are what gets checked.
    if planar != 1:
        raise MetricsError(
            f"{path.name}: planar (PLANARCONFIG={planar}) sample layout is not "
            "supported; re-export it interleaved/chunky, which is what every "
            "producer here writes by default")
    # Ordered by how specific the diagnosis is: "this is grayscale" before the
    # generic "this layout does not match the file's tags", or a 1-sample image
    # gets blamed on its shape.
    if samples < 3:
        raise MetricsError(
            f"{path.name}: expected an RGB image, got {samples} samples per pixel. "
            "Grayscale is not supported")
    if array.ndim != 3 or array.shape[:2] != declared or array.shape[2] != samples:
        raise MetricsError(
            f"{path.name}: the decoded array {array.shape} does not match what the "
            f"file declares ({declared[0]}x{declared[1]}, {samples} samples per "
            "pixel), so its layout is not one this command can measure")
    extra = samples - 3
    array = array[:, :, :3]

    dtype = array.dtype
    if dtype.kind == "u":
        full_scale = float(np.iinfo(dtype).max)
        encoded = array.astype(np.float32) / np.float32(full_scale)
    elif dtype.kind == "f":
        full_scale = 1.0
        encoded = array.astype(np.float32, copy=False)
    else:
        raise MetricsError(f"{path.name}: unsupported sample dtype {dtype}")

    meta = dict(width=int(array.shape[1]), height=int(array.shape[0]),
                dtype=str(dtype), full_scale=full_scale,
                extra_channels_ignored=extra)
    return encoded, meta


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with open(path, "rb") as stream:
        for chunk in iter(lambda: stream.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


# -- region -------------------------------------------------------------------


def parse_region(text: str) -> tuple[float, float, float, float]:
    """Parse `x,y,w,h` as **fractions** of the frame."""
    parts = [p.strip() for p in text.split(",")]
    if len(parts) != 4:
        raise MetricsError(f"--region wants x,y,w,h as fractions, got {text!r}")
    try:
        values = tuple(float(p) for p in parts)
    except ValueError as error:
        raise MetricsError(f"--region {text!r}: {error}") from error
    x, y, w, h = values
    # NaN compares false against every bound below, so without this it survives
    # validation and dies much later in `int(round(nan * width))`.
    if not all(math.isfinite(v) for v in values):
        raise MetricsError(f"--region {text!r}: all four values must be finite")
    if w <= 0 or h <= 0:
        raise MetricsError(f"--region {text!r}: width and height must be positive")
    if x < 0 or y < 0 or x + w > 1.0 + 1e-9 or y + h > 1.0 + 1e-9:
        raise MetricsError(
            f"--region {text!r}: fractions must lie within the frame (0..1)")
    return values


def resolve_region(width: int, height: int,
                   fraction: tuple[float, float, float, float]) -> dict:
    """Turn a fractional rectangle into whole pixels.

    Fractions rather than pixels because the images being compared do not share
    dimensions; a pixel rectangle measured on one would land somewhere else on
    the other.
    """
    fx, fy, fw, fh = fraction
    x = min(int(round(fx * width)), width - 1)
    y = min(int(round(fy * height)), height - 1)
    w = max(1, min(int(round(fw * width)), width - x))
    h = max(1, min(int(round(fh * height)), height - y))
    return dict(x=x, y=y, width=w, height=h, pixels=w * h,
                fraction=dict(x=round(fx, ROUND), y=round(fy, ROUND),
                              width=round(fw, ROUND), height=round(fh, ROUND)))


def inset_fraction(inset: float) -> tuple[float, float, float, float]:
    if not 0.0 <= inset < 0.5:
        raise MetricsError(f"--inset must be in [0, 0.5), got {inset}")
    return (inset, inset, 1.0 - 2 * inset, 1.0 - 2 * inset)


# -- metrics ------------------------------------------------------------------


def _round(value) -> float:
    value = float(value)
    if not math.isfinite(value):
        return value
    # `+ 0.0` normalizes -0.0, which json.dumps would otherwise write as "-0.0"
    # and make a diff-friendly artifact differ from itself across platforms.
    return round(value, ROUND) + 0.0


def endpoint_stats(encoded, meta: dict) -> dict:
    """Endpoint occupancy of the **encoded** samples, before any decode.

    Sitting at an endpoint is a property of the encoding, so it is counted on the
    stored values. This is an upper bound on what the producer *clipped*: a
    sample that legitimately landed on the endpoint is indistinguishable from one
    that was clamped to it. For nc outputs the report's `loss.*` counters are the
    independent check.
    """
    import numpy as np

    total = int(encoded.shape[0] * encoded.shape[1])
    finite = np.isfinite(encoded)
    non_finite = int(finite.size - np.count_nonzero(finite))
    safe = np.where(finite, encoded, np.float32(0.0))

    at_black = safe <= np.float32(0.0)
    at_white = safe >= np.float32(1.0)
    below_black = safe < np.float32(0.0)

    def share(mask) -> dict:
        per = [int(np.count_nonzero(mask[:, :, c])) for c in range(3)]
        return dict(r=_round(per[0] / total), g=_round(per[1] / total),
                    b=_round(per[2] / total),
                    any=_round(int(np.count_nonzero(mask.any(axis=2))) / total))

    return dict(
        samples=total * 3,
        pixels=total,
        at_or_below_black=share(at_black),
        at_or_above_white=share(at_white),
        below_black=share(below_black),
        non_finite_samples=non_finite,
        # Named for its denominator. `tone` reports a non-finite fraction too, over
        # *pixels*, and two identically named fields differing by 3x read as a bug
        # in one of the stages.
        non_finite_sample_fraction=_round(non_finite / (total * 3)),
    )


def tone_stats(linear, weights: tuple[float, float, float]) -> dict:
    """Luminance-domain tone statistics, in stops relative to mid grey."""
    import numpy as np

    y = (linear[:, :, 0] * np.float32(weights[0])
         + linear[:, :, 1] * np.float32(weights[1])
         + linear[:, :, 2] * np.float32(weights[2]))
    total = int(y.size)
    finite = np.isfinite(y)
    positive = finite & (y > 0)
    kept = int(np.count_nonzero(positive))
    non_positive = int(np.count_nonzero(finite) - kept)
    non_finite = total - int(np.count_nonzero(finite))

    result = dict(
        samples=total,
        measured=kept,
        # Luminance at or below zero has no logarithm. It is excluded from every
        # statistic below and counted here instead: folding it to a floor would
        # invent shadow detail, and dropping it silently would overstate the key.
        non_positive_pixel_fraction=_round(non_positive / total) if total else 0.0,
        non_finite_pixel_fraction=_round(non_finite / total) if total else 0.0,
    )
    if kept == 0:
        result["measured_none"] = True
        return result

    stops = np.log2(y[positive].astype(np.float64) / MID_GREY)
    wanted = [0.1, 1, 5, 10, 25, 50, 75, 90, 95, 99, 99.9]
    values = np.percentile(stops, wanted)
    percentiles = {f"p{p:g}": _round(v) for p, v in zip(wanted, values)}

    edges = [-math.inf, -4.0, -2.0, 2.0, DIFFUSE_WHITE_STOPS, math.inf]
    names = ["deep_shadow", "shadow", "mid", "highlight", "above_diffuse_white"]
    counts = np.histogram(stops, bins=edges)[0]
    bands = {name: _round(int(count) / total)
             for name, count in zip(names, counts)}
    # Every sample lands in exactly one band, so the shares sum to 1. The two
    # non-logarithmic outcomes need entries of their own for that to hold: without
    # a `non_finite` band the sum quietly becomes finite/total on any float file
    # carrying a NaN.
    bands["non_positive"] = result["non_positive_pixel_fraction"]
    bands["non_finite"] = result["non_finite_pixel_fraction"]

    result.update(
        # Mean of log2 luminance *is* the geometric mean expressed in stops, so
        # the exposure statistic needs no separate reduction.
        key_stops=_round(stops.mean()),
        percentiles_stops=percentiles,
        contrast=dict(
            p95_minus_p5=_round(percentiles["p95"] - percentiles["p5"]),
            p75_minus_p25=_round(percentiles["p75"] - percentiles["p25"]),
            stdev_stops=_round(stops.std()),
        ),
        toe_span_stops=_round(percentiles["p5"] - percentiles["p0.1"]),
        shoulder_span_stops=_round(percentiles["p99.9"] - percentiles["p95"]),
        bands=bands,
    )
    return result


def measure(path: Path, space_name: str,
            fraction: tuple[float, float, float, float] = (0.0, 0.0, 1.0, 1.0),
            digest: bool = True) -> dict:
    """Measure one image and return its metric record.

    Percentiles are taken over every sample in the region, not a subsample, so
    peak memory scales with the frame: measured at **1.18 GB for 18.66 MP**
    (~63 bytes/pixel), which extrapolates to ~4.7 GB on a 10368x7200 scan. The
    decode is in place (see `_decode_transfer`); what remains is the float32
    frame, the float64 log values, and the sort inside `np.percentile`. If that
    ceiling ever bites, decimate deliberately and record the stride in the
    artifact — do not let it become a silent subsample.
    """
    require_dependencies()
    if space_name in REFUSED:
        raise MetricsError(f"colour space {space_name!r} is not supported here: "
                           f"{REFUSED[space_name]}")
    space = SPACES.get(space_name)
    if space is None:
        known = ", ".join(sorted(SPACES))
        raise MetricsError(
            f"unknown colour space {space_name!r}. Declare one of: {known}. "
            "The space is never inferred — a TIFF's samples do not say whether "
            "they are transfer-encoded, and guessing wrong produces a plausible "
            "wrong answer")

    encoded, meta = read_image(path)
    region = resolve_region(meta["width"], meta["height"], fraction)
    view = encoded[region["y"]:region["y"] + region["height"],
                   region["x"]:region["x"] + region["width"], :]

    endpoints = endpoint_stats(view, meta)
    linear = _decode_transfer(view, space.transfer)
    tone = tone_stats(linear, luminance_weights(space))

    record = dict(
        schema_version=SCHEMA,
        file=path.name,
        image=dict(width=meta["width"], height=meta["height"],
                   dtype=meta["dtype"],
                   extra_channels_ignored=meta["extra_channels_ignored"]),
        space=space.describe(),
        region=region,
        endpoints=endpoints,
        tone=tone,
    )
    if digest:
        record["sha256"] = sha256(path)
    return record


# -- command ------------------------------------------------------------------


def cmd_image(args) -> int:
    """Measure one image and write its metric record."""
    try:
        # `is not None`, not truthiness: `--region ""` is a malformed region, not
        # an absent one, and testing truthiness let it slip past this refusal and
        # silently hand the run to --inset.
        if args.region is not None and args.inset is not None:
            raise MetricsError("--region and --inset both select a region; pass one")
        if args.region is not None:
            fraction = parse_region(args.region)
        elif args.inset is not None:
            fraction = inset_fraction(args.inset)
        else:
            fraction = (0.0, 0.0, 1.0, 1.0)
        path = Path(args.image)
        if not path.is_file():
            raise MetricsError(f"not a file: {path}")
        record = measure(path, args.space, fraction, digest=not args.no_checksum)
    except MetricsError as error:
        print(f"error: {error}", file=sys.stderr)
        return 2

    text = json.dumps(record, indent=2, sort_keys=True) + "\n"
    if args.out:
        out = Path(args.out)
        out.parent.mkdir(parents=True, exist_ok=True)
        out.write_text(text, encoding="utf-8")
        print(f"wrote {out}", file=sys.stderr)
    else:
        sys.stdout.write(text)
    return 0
