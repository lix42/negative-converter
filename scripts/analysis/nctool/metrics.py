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

Colour is reported as cast, chroma and hue in CIELAB, and — the part that matters
for a negative conversion — **per tone band**, because the characteristic failure
is crossover: the cast drifting one way in the shadows and the other as the frame
brightens. A single whole-frame cast averages that out to nothing.

The tone bands themselves are cut in **CIELAB lightness**, not in stops: equal
steps of lightness are unequal steps of exposure, and a cut even in stops puts
most of a normal frame in one band. `tone.histogram` bins the same L* axis one
unit at a time, for luminance and for each channel, so a band is a whole number
of bins and one chart can draw both.

Only derived statistics leave this module. Sample pixels never reach a report,
a committed artifact, or an agent context (CLAUDE.md).
"""
from __future__ import annotations

import hashlib
import json
import math
import sys
from pathlib import Path

SCHEMA = 2

#: Mid grey. The anchor for every value reported in stops.
MID_GREY = 0.18

#: Diffuse white, in stops above mid grey: log2(1 / 0.18).
DIFFUSE_WHITE_STOPS = math.log2(1.0 / MID_GREY)

#: CIE 1976 L*: the knee between its cube-root and linear arms, and that arm's
#: slope 24389/27. Luminance is relative to diffuse white = 1, so L* = 100 is
#: diffuse white and L* ~ 49.5 is mid grey.
LSTAR_KNEE = 8.0
LSTAR_LINEAR_SLOPE = 24389.0 / 27.0
#: Y at the knee, (6/29)^3 — the point where both arms agree exactly.
LSTAR_KNEE_LUMINANCE = (6.0 / 29.0) ** 3
#: Diffuse white. The top band edge and the top of the histogram, named once so
#: the two cannot drift apart.
DIFFUSE_WHITE_LSTAR = 100.0


def luminance_of_lstar(lstar: float) -> float:
    """Relative luminance of a CIELAB lightness, diffuse white = 1."""
    if lstar > LSTAR_KNEE:
        return ((lstar + 16.0) / 116.0) ** 3
    return lstar / LSTAR_LINEAR_SLOPE


def stops_of_lstar(lstar: float) -> float:
    """A CIELAB lightness as stops relative to mid grey."""
    return math.log2(luminance_of_lstar(lstar) / MID_GREY)


def _lstar_scalar(luminance: float) -> float:
    """CIELAB lightness of one relative luminance. The inverse of the above."""
    if luminance > LSTAR_KNEE_LUMINANCE:
        return 116.0 * luminance ** (1.0 / 3.0) - 16.0
    return luminance * LSTAR_LINEAR_SLOPE


#: Tone bands, cut in **CIELAB lightness** — the same perceptual space the colour
#: stage measures cast in — every 15 L* up to 75, then diffuse white (L* = 100),
#: then an overflow band above it. In stops the widths narrow going up (1.71 /
#: 1.22 / 0.95 / 0.78 / 1.05), and that is the point: equal steps of lightness
#: are unequal steps of exposure, so a cut even in *stops* is even in nothing a
#: viewer sees. The previous cut was even in stops — -4 / -2 / +2 / diffuse
#: white, after Zones III and VII — and its `mid` band held 83% of the median
#: frame (95% at worst) while `highlight` spanned 0.47 stops and was empty on
#: four of five renders of one frame. Measured over 33 renders (six frames x five
#: `--preset` bundles, plus three Negative Lab Pro references), this cut's
#: largest band holds 47% of the median frame and 56% at worst.
#:
#: `above_diffuse_white` is an overflow bin, not a seventh of the range: an SDR
#: rendition cannot populate it at all, and on a float or HDR output it is the
#: only place in the tone stage that headroom above display white shows up.
#:
#: Shared with the colour stage, which reports the cast of each band — the same
#: edges, or "the shadows are cooler than the highlights" would be measured
#: against a different definition of shadow.
BAND_LSTAR_EDGES = (15.0, 30.0, 45.0, 60.0, 75.0, DIFFUSE_WHITE_LSTAR)
BAND_NAMES = ("deep_shadow", "shadow", "low_mid", "mid", "high_mid",
              "highlight", "above_diffuse_white")
BAND_EDGES = (-math.inf, *(stops_of_lstar(L) for L in BAND_LSTAR_EDGES), math.inf)

#: Below this share of the region a band's cast describes essentially none of the
#: picture, and `cast_by_tone_band` marks the entry `sparse`. A share and not a
#: pixel count: a few hundred pixels are plenty to average, so what is wrong with
#: such an entry is not noise but that it reads as a statement about the frame.
#: It is a caveat, not a filter — the entry stays, because a band set that varies
#: frame to frame cannot be diffed. It exists because the old cut let `highlight`
#: report the largest colour excursion in a measured record (a* = -19.5) off a
#: single pixel out of 15.1 million, beside a `mid` cast resting on 91.7% of the
#: frame, with nothing to tell the two apart.
BAND_SPARSE_FRACTION = 0.001

#: The level histogram: one bin per L* unit. Bins are L* so that every band edge
#: falls exactly on a bin edge and one chart can draw both, and because the
#: alternatives are wrong for drawing — stops give black an unbounded tail, and
#: the stored code values describe the file's encoding as much as the picture,
#: which is what decoding to linear light exists to avoid.
#:
#: The range runs to **twice** diffuse white in lightness, not to diffuse white,
#: for two reasons. A float or HDR rendition genuinely carries samples above
#: display white, and a scalar overflow counter cannot be drawn — L* 200 is 6.46x
#: diffuse white (+5.17 stops), which covers nc's own 1000/203 HDR ceiling
#: (L* 181.4) with margin. And on an SDR render the interesting fact is *how far
#: short of* diffuse white the highlights stop, which needs white inside the axis
#: rather than at its edge. Diffuse white therefore sits on the bin-100 boundary,
#: exactly halfway along. Anything beyond the range lands in `above_range`.
HISTOGRAM_MAX_LSTAR = 200.0
HISTOGRAM_BINS = 200

#: Chroma below this counts as neutral. A near-neutral pixel has a hue angle, but
#: it is noise — a*, b* of (0.01, -0.01) is a 135 degree hue that means nothing —
#: so hue statistics are taken over pixels above this and the rest are reported
#: as a neutral share instead. Chosen, not standard: it is about the smallest
#: chroma difference anyone would call a cast.
NEUTRAL_CHROMA = 2.0

#: The chroma histogram, which the chroma percentiles are read off. The ceiling is
#: generous — sRGB's most saturated colours reach C* ~ 130, and float working
#: spaces go further — and samples above it are clipped into the top bin rather
#: than dropped, with `max_chroma` reported exactly so a saturated percentile is
#: recognizable.
CHROMA_CEILING = 150.0
CHROMA_BINS = 600
CHROMA_STEP = CHROMA_CEILING / CHROMA_BINS

#: Rows per block in the colour pass. Colour needs only aggregates, so it streams
#: instead of materializing XYZ, Lab, chroma and hue for the whole frame — which
#: would have added ~40 bytes/pixel to a measurement already at ~63.
BLOCK_ROWS = 256

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
# fails if these drift from it. A space nc does not render to still gets its
# definition there (`definitions::PROPHOTO` is one, and `ADOBE_RGB` was until the
# new chain could render into it): references arrive in it, and a set of primaries living
# only in this file would be a second source of truth by construction. Add the
# definition there, then transcribe it here.

PRIMARIES: dict[str, tuple[tuple[float, float], ...]] = {
    "rec709": ((0.640, 0.330), (0.300, 0.600), (0.150, 0.060)),
    "display-p3": ((0.680, 0.320), (0.265, 0.690), (0.150, 0.060)),
    # Shares rec709's red and blue exactly; only green moves. See
    # `definitions::ADOBE_RGB`, which pins that relationship in its own tests.
    "adobe-rgb": ((0.640, 0.330), (0.210, 0.710), (0.150, 0.060)),
    "bt2020": ((0.708, 0.292), (0.170, 0.797), (0.131, 0.046)),
    "prophoto": ((0.7347, 0.2653), (0.1596, 0.8404), (0.0366, 0.0001)),
    "acescg": ((0.713, 0.293), (0.165, 0.830), (0.128, 0.044)),
}

WHITE: dict[str, tuple[float, float]] = {
    "d65": (0.3127, 0.3290),
    "d50": (0.3457, 0.3585),
    "aces": (0.32168, 0.33767),
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

#: The Adobe RGB (1998) decode exponent, `definitions::transfer::adobe_rgb::GAMMA`:
#: `563/256`, not the `2.2` often quoted. The encoder nc writes Adobe RGB with uses
#: the same constant, so a decode that drifted from it would misread nc's own output.
ADOBE_RGB_GAMMA = 563.0 / 256.0


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
    # Two ProPhoto decodes, deliberately. `prophoto` is ISO 22028-2 as specified,
    # with the linear toe — right for a Lightroom or third-party export.
    # `prophoto-gamma1.8` is the pure power law **nc itself writes** for
    # `--output-profile prophoto`. They agree above encoded 0.03125 and diverge
    # sharply below it, so the wrong one silently rewrites the shadow statistics.
    "prophoto": Space("prophoto", "prophoto", "d50", "prophoto"),
    "prophoto-gamma1.8": Space("prophoto-gamma1.8", "prophoto", "d50", "gamma1.8"),
    "linear-prophoto": Space("linear-prophoto", "prophoto", "d50", "linear"),
    "linear-bt2020": Space("linear-bt2020", "bt2020", "d65", "linear"),
    "linear-acescg": Space("linear-acescg", "acescg", "aces", "linear"),
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
        np.power(magnitude, np.float32(ADOBE_RGB_GAMMA), out=magnitude)
    elif transfer == "prophoto":
        # ISO 22028-2 (ROMM RGB): gamma 1.8 above 16 * (1/512).
        toe = magnitude < np.float32(16.0 / 512.0)
        patched = magnitude[toe] / np.float32(16.0)
        np.power(magnitude, np.float32(1.8), out=magnitude)
        magnitude[toe] = patched
    elif transfer == "gamma1.8":
        # A pure 1.8 power law with no toe. This is what **nc** writes for
        # `--output-profile prophoto`: `color::build_profile` synthesizes the
        # profile with a plain 1.8 TRC and says so — "the small ROMM linear toe
        # near black is omitted". Decoding nc's own output with the piecewise
        # ROMM curve above diverges as `16 * v^0.8` below encoded 0.03125: 1.3
        # stops out at v=0.01, ~4 stops at v=0.001 — exactly the samples that
        # `p0.1`, `toe_span_stops` and `bands.deep_shadow` are made of.
        np.power(magnitude, np.float32(1.8), out=magnitude)
    else:
        raise ValueError(f"unknown transfer function: {transfer}")

    return np.copysign(magnitude, array, out=magnitude)


# -- reading ------------------------------------------------------------------


class MetricsError(Exception):
    """A refusal the caller should print and exit non-zero on."""


def require_dependencies() -> None:
    try:
        import numpy  # noqa: F401
        import PIL  # noqa: F401
        import tifffile  # noqa: F401
    except ImportError as error:
        raise MetricsError(
            f"{error.name} is required by `nctool metrics` and is not importable. "
            "The rest of the toolkit is stdlib-only; this command is not. Set it "
            "up with:\n"
            "    uv venv --python 3.12\n"
            "    uv pip install -r scripts/analysis/requirements.txt\n"
            "and run the command with .venv/bin/python") from error


#: What to measure in a JPEG that carries a gain map.
JPEG_IMAGES = ("sdr", "hdr")


def _is_jpeg(path: Path) -> bool:
    """Sniff the magic bytes. Extensions lie, and a mislabelled file should get a
    real diagnosis rather than "not a TIFF"."""
    with open(path, "rb") as stream:
        return stream.read(3) == b"\xff\xd8\xff"


def _jpeg_decoder_identity() -> str:
    """Which decoder produced the samples, for the record.

    A JPEG's pixels are whatever the decoder says they are, so unlike the TIFF
    path the numbers are only reproducible against a named decoder. Recording it
    is the price of reading lossy input at all.
    """
    import PIL
    import PIL.features

    return f"Pillow {PIL.__version__} / libjpeg {PIL.features.version('jpg')}"


def _first_image_end(data: bytes) -> int | None:
    """Offset just past the first JPEG image's EOI, or None if it is malformed.

    Walks the marker segments rather than searching for bytes. Each `APPn` is
    skipped by its declared length, so a payload that happens to contain JPEG
    bytes — an EXIF thumbnail, most obviously — is stepped over rather than read
    as structure.
    """
    length = len(data)
    if length < 4 or data[0] != 0xFF or data[1] != 0xD8:
        return None
    i = 2
    while i + 1 < length:
        if data[i] != 0xFF:
            return None
        marker = data[i + 1]
        if marker == 0xD9:                        # EOI
            return i + 2
        if marker == 0x01 or 0xD0 <= marker <= 0xD7:   # standalone markers
            i += 2
            continue
        if i + 4 > length:
            return None
        segment = int.from_bytes(data[i + 2:i + 4], "big")
        if segment < 2:
            return None
        i += 2 + segment
        if marker == 0xDA:                        # SOS: entropy-coded data follows
            # Only here is byte stuffing in play: `FF` is written `FF00`, so the
            # next unstuffed, non-restart `FF xx` ends the scan.
            while i + 1 < length:
                if data[i] == 0xFF and data[i + 1] != 0x00 and not (
                        0xD0 <= data[i + 1] <= 0xD7):
                    break
                i += 1
    return None


def _gain_map_present(path: Path) -> bool:
    """Whether a JPEG carries a second (gain-map) image.

    MPF is the authority — it is the index that names the appended image. The
    fallback looks for another SOI *after the first image's EOI*, found by walking
    the markers.

    An earlier version counted `FFD8FF` occurrences in the whole file and
    justified it with byte stuffing. That reasoning was wrong: stuffing applies
    only to entropy-coded scan data, not to marker payloads — and an EXIF APP1
    payload contains a whole embedded thumbnail JPEG, so every camera and
    Lightroom export reported a gain map it does not have. Those exports are
    exactly the reference files this reader was added for.
    """
    data = path.read_bytes()
    if b"MPF\x00" in data[:65536]:
        return True
    end = _first_image_end(data)
    return end is not None and data.find(b"\xff\xd8\xff", end) != -1


def _read_jpeg(path: Path, which: str):
    """Decode a JPEG's base image as float32 in [0, 1].

    Pillow opens an nc gain-map JPEG as a single frame — the file is registered as
    JPEG rather than MPO, so the appended gain-map image is never touched — which
    means this returns exactly the base SDR rendition. That is the right thing for
    `sdr` and the reason the gain map has to be detected separately: the base of a
    dual-image file is byte-indistinguishable from a plain JPEG to the decoder, and
    calling it "the render" would be wrong on any platform that decodes the pair.
    """
    import numpy as np
    from PIL import Image

    has_gain_map = _gain_map_present(path)
    if which == "hdr":
        # Ordered by how specific the diagnosis is: "there is no gain map here"
        # before "reconstruction is not implemented".
        if not has_gain_map:
            raise MetricsError(
                f"{path.name}: --jpeg-image hdr, but this file carries no gain map "
                "(no MPF segment, one SOI), so it has no HDR rendition to measure")
        raise MetricsError(
            f"{path.name}: --jpeg-image hdr is not implemented. Reconstructing the "
            "HDR rendition means applying the gain map with its ISO 21496-1 / "
            "Ultra HDR metadata — offsets, gamma, min/max, declared headroom — and "
            "a reconstruction that is subtly wrong yields plausible wrong numbers "
            "rather than an error, which is the one failure this command must not "
            "have. Measure nc's own `hdr-linear-tiff` render of the same source "
            "instead: that is the display-linear HDR signal with no container in "
            "the way. See scripts/iso-decoder-oracle/README.md")

    try:
        with Image.open(path) as handle:
            if handle.mode not in ("RGB", "YCbCr"):
                raise MetricsError(
                    f"{path.name}: JPEG mode {handle.mode!r} is not RGB; grayscale "
                    "and CMYK are not supported")
            array = np.asarray(handle.convert("RGB"))
    except MetricsError:
        raise
    except Exception as error:
        raise MetricsError(
            f"{path.name}: cannot be decoded as a JPEG "
            f"({type(error).__name__}: {error})") from error

    if array.ndim != 3 or array.shape[2] != 3:
        raise MetricsError(f"{path.name}: decoded JPEG has shape {array.shape}")

    encoded = array.astype(np.float32) / np.float32(255.0)
    meta = dict(width=int(array.shape[1]), height=int(array.shape[0]),
                dtype=str(array.dtype), full_scale=255.0,
                extra_channels_ignored=0, container="jpeg",
                bits_per_sample=8, decoder=_jpeg_decoder_identity(),
                gain_map_present=has_gain_map, jpeg_image=which)
    return encoded, meta


def read_image(path: Path, jpeg_image: str = "sdr"):
    """Read one single-page RGB image as float32 in [0, 1]-nominal encoded units.

    TIFF and JPEG, dispatched on the file's magic bytes rather than its extension.
    Integer samples are divided by their full-scale code value, so an 8-bit JPEG
    and a 16-bit TIFF of the same picture measure the same — with the caveat that
    8 bits is 256 levels and the tone metrics are logarithmic, so a JPEG's
    deep-shadow percentiles and `toe_span` are quantization-limited in a way a
    16-bit TIFF's are not. That is a precision limit, not a bias: the record
    carries `bits_per_sample` so a reader can see which it is looking at.

    Float samples are taken as they are — a float file's 1.0 already means display
    white, and rescaling by an observed maximum would silently normalize away the
    exposure difference this tool is built to measure.
    """
    import numpy as np
    import tifffile

    if _is_jpeg(path):
        return _read_jpeg(path, jpeg_image)

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
            f"{path.name}: cannot be read as a TIFF or JPEG "
            f"({type(error).__name__}: {error}). This command reads single-page RGB "
            "TIFF and JPEG; PNG and other containers are not supported") from error

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
                extra_channels_ignored=extra, container="tiff",
                bits_per_sample=int(dtype.itemsize * 8))
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

    # Compared directly, not through a NaN-substituted copy. Substituting 0.0 for
    # a non-finite sample made it count as sitting *at black*, inventing an
    # endpoint population that is not in the file. Every numpy comparison against
    # NaN is already False, so masking costs nothing and is what the substitution
    # was reaching for.
    at_black = encoded <= np.float32(0.0)
    at_white = encoded >= np.float32(1.0)
    below_black = encoded < np.float32(0.0)

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

    counts = np.histogram(stops, bins=list(BAND_EDGES))[0]
    names = BAND_NAMES
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


def _lstar(values):
    """CIE 1976 L* of relative luminance. Expects positive, finite values."""
    import numpy as np

    low = values <= np.float32(LSTAR_KNEE_LUMINANCE)
    out = np.cbrt(values.astype(np.float64))
    out *= 116.0
    out -= 16.0
    out[low] = values[low].astype(np.float64) * LSTAR_LINEAR_SLOPE
    return out


def _series_histogram(values, excluded: "list[int]"):
    """One block of luminance-like values as bin counts, excluded ones tallied.

    `excluded` is `[non_positive, non_finite, above_range]`, accumulated in place.
    Those three plus the bins partition the block. A sample with no lightness is
    counted rather than folded to zero, for the same reason `tone_stats` counts
    one: folding would invent black pixels the file does not contain. One past
    the top of the range is counted rather than clipped into the last bin, which
    would invent a highlight pile-up instead.
    """
    import numpy as np

    finite = np.isfinite(values)
    positive = finite & (values > 0)
    lstar = _lstar(values[positive])
    inside = lstar < HISTOGRAM_MAX_LSTAR
    excluded[0] += int(np.count_nonzero(finite) - np.count_nonzero(positive))
    excluded[1] += int(values.size - np.count_nonzero(finite))
    excluded[2] += int(lstar.size - np.count_nonzero(inside))
    return np.histogram(lstar[inside], bins=HISTOGRAM_BINS,
                        range=(0.0, HISTOGRAM_MAX_LSTAR))[0]


def histogram_stats(linear, weights: tuple[float, float, float]) -> dict:
    """Level distributions for luminance and each channel, binned in L*.

    The one list-valued field in the record, and the only one a review tool can
    draw as a histogram rather than read as a number.

    The same L* curve is applied to each channel as to luminance. On a channel
    that is not a colorimetric lightness — only the luminance series is — but it
    is the one monotone mapping that puts all four series on a single axis, and
    it is the axis the tone bands are cut on, so a chart can shade the bands over
    the bars. Comparing the three channel series is how a cast reads as a shape
    rather than as `color.balance_stops`' single number per channel.

    Streamed in row blocks like `color_stats`: only the 100 accumulators survive
    a block, so this adds counters rather than another full-frame temporary.
    """
    import numpy as np

    height = linear.shape[0]
    total = int(height * linear.shape[1])
    names = ("luminance", "r", "g", "b")
    hist = {name: np.zeros(HISTOGRAM_BINS, dtype=np.int64) for name in names}
    # [non_positive, non_finite, above_diffuse_white] per series.
    extra = {name: [0, 0, 0] for name in names}

    for start in range(0, height, BLOCK_ROWS):
        block = linear[start:start + BLOCK_ROWS]
        y = (block[..., 0] * np.float32(weights[0])
             + block[..., 1] * np.float32(weights[1])
             + block[..., 2] * np.float32(weights[2]))
        for name, values in (("luminance", y), ("r", block[..., 0]),
                             ("g", block[..., 1]), ("b", block[..., 2])):
            hist[name] += _series_histogram(values, extra[name])

    def series(name: str) -> dict:
        non_positive, non_finite, above = extra[name]
        return dict(counts=[int(v) for v in hist[name]],
                    non_positive=non_positive, non_finite=non_finite,
                    above_range=above)

    return dict(
        # Says what the bins are, in the record, so a consumer never has to infer
        # it from the shape of the data.
        domain="cielab_lstar",
        domain_note=("bin i covers L* [i, i+1); L* 0 is black, ~49.5 is scene "
                     "mid grey, 100 is diffuse white, 200 is 6.46x diffuse white "
                     "(+5.17 stops). Samples above the range are counted in "
                     "`above_range`, not binned. The channel series apply the "
                     "same L* curve to one channel, which is a level, not a "
                     "colorimetric lightness"),
        bins=HISTOGRAM_BINS,
        lstar_range=[0.0, HISTOGRAM_MAX_LSTAR],
        bin_width_lstar=_round(HISTOGRAM_MAX_LSTAR / HISTOGRAM_BINS),
        # The two reference lines a chart wants to draw, as bin indices, so they
        # do not have to be re-derived from the L* formula by every consumer.
        mid_grey_bin=int(_lstar_scalar(MID_GREY) // (HISTOGRAM_MAX_LSTAR
                                                     / HISTOGRAM_BINS)),
        diffuse_white_bin=int(DIFFUSE_WHITE_LSTAR // (HISTOGRAM_MAX_LSTAR
                                                      / HISTOGRAM_BINS)),
        pixels=total,
        series={name: series(name) for name in names},
    )


# -- colour -------------------------------------------------------------------


def lab_reference_white(space: Space) -> tuple[float, float, float]:
    """The CIELAB reference white, derived from this module's own D65.

    Deliberately **not** the widely tabulated `(0.95047, 1.0, 1.08883)`. That
    triple comes from the CIE's D65 spectral distribution, while everything else
    here derives from `definitions::D65`'s rounded chromaticity `(0.3127,
    0.3290)`, which gives `(0.950456, 1, 1.089058)` — about 2e-4 away in Z.

    Self-consistency wins here because the headline colour number is a *cast*: an
    RGB-neutral pixel has to measure `a* = b* = 0` exactly, or every image
    acquires a small constant tint and the metric reports a fault the render does
    not have. A test pins that property.

    Note this is a different choice from the one
    `docs/tasks/analysis/display-output-acceptance.md` pins for its cross-encoding
    oracle, which needs the tabulated white because it compares absolute
    colorimetry across renditions rather than relative cast within one image.
    """
    # Always D65, whatever the space's own adopted white: `rgb_to_xyz_d65`
    # Bradford-adapts to D65 first, so this is the white those XYZ values are
    # relative to. (It used to be written as a conditional that could only ever
    # select D65, which read as if it were space-dependent.)
    x, y = WHITE["d65"]
    return (x / y, 1.0, (1.0 - x - y) / y)


def _to_lab(linear_block, matrix, white):
    """Linear RGB -> CIELAB, for one block of rows."""
    import numpy as np

    r, g, b = linear_block[..., 0], linear_block[..., 1], linear_block[..., 2]
    xyz = [r * matrix[i][0] + g * matrix[i][1] + b * matrix[i][2] for i in range(3)]

    delta = 6.0 / 29.0
    for i in range(3):
        t = xyz[i] / white[i]
        # The CIE 1976 piecewise f(t). Negative t — which real out-of-gamut float
        # samples produce — falls in the linear arm, where the function is
        # defined. (`np.cbrt` handles negatives fine; it is `t ** (1/3)` that
        # returns NaN. The piecewise definition, not the cube root, is the reason
        # this is correct.)
        low = t < delta ** 3
        xyz[i] = np.where(low, t / (3 * delta * delta) + 4.0 / 29.0, np.cbrt(t))
    fx, fy, fz = xyz
    return 500.0 * (fx - fy), 200.0 * (fy - fz)


def color_stats(linear, space: Space, weights: tuple[float, float, float]) -> dict:
    """Cast, chroma and hue, streamed in row blocks.

    Only aggregates are kept, so this adds counters rather than frames. The three
    things it is built to answer: how far from neutral is the render overall, does
    that cast *differ between one tone band and another* (crossover, the failure
    mode a negative conversion actually has), and how is chroma distributed across
    hue.
    """
    import numpy as np

    matrix = rgb_to_xyz_d65(space)
    white = lab_reference_white(space)
    height = linear.shape[0]
    total = int(linear.shape[0] * linear.shape[1])

    kept = 0
    sum_a = sum_b = sum_c = 0.0
    max_chroma = 0.0
    neutral = 0
    chroma_hist = np.zeros(CHROMA_BINS, dtype=np.int64)
    band_count = [0] * len(BAND_NAMES)
    band_a = [0.0] * len(BAND_NAMES)
    band_b = [0.0] * len(BAND_NAMES)
    sectors = 6
    sector_count = [0] * sectors
    sector_c = [0.0] * sectors
    sector_cos = [0.0] * sectors
    sector_sin = [0.0] * sectors
    channel_log_sum = [0.0] * 3
    channel_positive = [0] * 3

    for start in range(0, height, BLOCK_ROWS):
        block = linear[start:start + BLOCK_ROWS]
        finite = np.isfinite(block).all(axis=2)

        for c in range(3):
            channel = block[..., c]
            usable = finite & (channel > 0)
            count = int(np.count_nonzero(usable))
            if count:
                channel_log_sum[c] += float(
                    np.log2(channel[usable].astype(np.float64)).sum())
                channel_positive[c] += count

        a, b = _to_lab(np.where(finite[..., None], block, np.float32(0.0)).astype(
            np.float64), matrix, white)
        chroma = np.hypot(a, b)

        y = (block[..., 0] * np.float32(weights[0])
             + block[..., 1] * np.float32(weights[1])
             + block[..., 2] * np.float32(weights[2]))
        usable = finite & np.isfinite(y) & (y > 0)
        if not np.any(usable):
            continue

        a_u, b_u, c_u = a[usable], b[usable], chroma[usable]
        kept += int(a_u.size)
        sum_a += float(a_u.sum())
        sum_b += float(b_u.sum())
        sum_c += float(c_u.sum())

        # Clipped into the top bin, not passed through `range=`: np.histogram
        # *discards* out-of-range values, so a frame more saturated than the
        # ceiling emptied the histogram entirely and the percentiles below read
        # 0.12 while the mean read 208. `max_chroma` is exact, so a percentile
        # sitting at the ceiling is visible rather than merely wrong.
        max_chroma = max(max_chroma, float(c_u.max()))
        chroma_hist += np.histogram(
            np.clip(c_u, 0.0, CHROMA_CEILING - CHROMA_STEP / 2),
            bins=CHROMA_BINS, range=(0.0, CHROMA_CEILING))[0]

        stops = np.log2(y[usable].astype(np.float64) / MID_GREY)
        band = np.digitize(stops, list(BAND_EDGES)[1:-1])
        for index in range(len(BAND_NAMES)):
            in_band = band == index
            hits = int(np.count_nonzero(in_band))
            if hits:
                band_count[index] += hits
                band_a[index] += float(a_u[in_band].sum())
                band_b[index] += float(b_u[in_band].sum())

        colored = c_u >= NEUTRAL_CHROMA
        neutral += int(c_u.size - np.count_nonzero(colored))
        if np.any(colored):
            hue = np.degrees(np.arctan2(b_u[colored], a_u[colored])) % 360.0
            chroma_colored = c_u[colored]
            sector = np.minimum((hue // 60.0).astype(np.int64), sectors - 1)
            for index in range(sectors):
                in_sector = sector == index
                hits = int(np.count_nonzero(in_sector))
                if hits:
                    radians = np.radians(hue[in_sector])
                    sector_count[index] += hits
                    sector_c[index] += float(chroma_colored[in_sector].sum())
                    sector_cos[index] += float(np.cos(radians).sum())
                    sector_sin[index] += float(np.sin(radians).sum())

    result: dict = dict(samples=total, measured=kept)
    if kept == 0:
        result["measured_none"] = True
        return result

    # Per-channel geometric means, and the balance between them. Stated in stops
    # because that is what an exposure or white-balance difference reads as.
    # Each channel's geometric mean is taken over the pixels where *that channel*
    # is positive, so the three can rest on different supports. The ratios below
    # are only meaningful when they rest on the same one: a channel crushed to
    # black in half the frame — which is what a negative conversion produces —
    # otherwise reported a perfectly neutral balance for an image with a massive
    # cast, every counter reading clean. The supports are published, and the
    # ratios are omitted rather than stated wrongly when they disagree.
    balance = {}
    support = {}
    for name, index in (("r", 0), ("g", 1), ("b", 2)):
        support[name] = _round(channel_positive[index] / total) if total else 0.0
        if channel_positive[index]:
            balance[name] = _round(
                channel_log_sum[index] / channel_positive[index]
                - math.log2(MID_GREY))
    comparable = len(set(channel_positive)) == 1
    if comparable and {"r", "g", "b"} <= balance.keys():
        balance["r_over_g"] = _round(balance["r"] - balance["g"])
        balance["b_over_g"] = _round(balance["b"] - balance["g"])

    cumulative = chroma_hist.cumsum()

    def chroma_percentile(fraction: float) -> float:
        target = fraction * cumulative[-1]
        index = int(np.searchsorted(cumulative, target))
        return _round(min(index, CHROMA_BINS - 1) * CHROMA_STEP + CHROMA_STEP / 2)

    result.update(
        balance_stops=balance,
        # Fraction of the region each channel's mean rests on. Equal in the
        # ordinary case; unequal exactly when `r_over_g` / `b_over_g` are absent.
        balance_support=support,
        mean_a=_round(sum_a / kept),
        mean_b=_round(sum_b / kept),
        # The cast of the *average* pixel, as one number. Not a per-pixel error:
        # a frame of complementary casts averages to neutral, which is why the
        # per-band split below exists.
        mean_cast=_round(math.hypot(sum_a / kept, sum_b / kept)),
        mean_chroma=_round(sum_c / kept),
        median_chroma=chroma_percentile(0.5),
        p90_chroma=chroma_percentile(0.9),
        max_chroma=_round(max_chroma),
        # Every fraction here is of the whole region, the same denominator `tone`
        # uses. Dividing by the *measured* count instead made colour report a band
        # as 1.0 that tone reported as 0.95 on a frame with one NaN row — two
        # fields with one name and two bases, which reads as a bug in a stage.
        measured_fraction=_round(kept / total),
        neutral_fraction=_round(neutral / total),
        # `pixels` and `sparse` ride beside every cast so the number carries its
        # own denominator. Without them a band resting on a few hundred pixels
        # reads with exactly the authority of one resting on half the frame, and
        # that is how the largest colour excursion in a measured record came to be
        # the colour of a single pixel. Sparse entries are kept, not dropped: a
        # band set that varies frame to frame cannot be diffed.
        cast_by_tone_band={
            name: dict(fraction=_round(band_count[i] / total),
                       pixels=band_count[i],
                       sparse=band_count[i] / total < BAND_SPARSE_FRACTION,
                       mean_a=_round(band_a[i] / band_count[i]),
                       mean_b=_round(band_b[i] / band_count[i]))
            for i, name in enumerate(BAND_NAMES) if band_count[i]
        },
        # Shares are of the whole region, so they sum to
        # `measured_fraction - neutral_fraction`: a near-neutral pixel has no
        # meaningful hue and is counted there instead.
        hue_sectors={
            f"deg_{i * 60:03d}_{(i + 1) * 60:03d}": dict(
                fraction=_round(sector_count[i] / total),
                mean_chroma=_round(sector_c[i] / sector_count[i]),
                # Circular mean, the correct operator for an angle. With the
                # current 60-degree bins no sector spans the 0/360 wrap, so an
                # arithmetic mean would agree today; this stays correct if the
                # sectors are ever re-cut around hue centres instead.
                mean_hue=_round(math.degrees(math.atan2(
                    sector_sin[i] / sector_count[i],
                    sector_cos[i] / sector_count[i])) % 360.0))
            for i in range(sectors) if sector_count[i]
        },
    )
    return result


def describe_bands() -> dict:
    """The band cut, stated in the record it applies to.

    Two things make this worth carrying rather than documenting elsewhere: the
    edges moved once (schema 1 -> 2) and will read plausibly against the wrong
    definition if they move again, and a consumer drawing `tone.histogram` needs
    the edges in the histogram's own domain to shade the bands onto it.
    """
    return dict(
        domain="cielab_lstar",
        names=list(BAND_NAMES),
        lstar_edges=list(BAND_LSTAR_EDGES),
        stops_edges=[_round(edge) for edge in BAND_EDGES[1:-1]],
        sparse_below_fraction=BAND_SPARSE_FRACTION,
    )


def measure(path: Path, space_name: str,
            fraction: tuple[float, float, float, float] = (0.0, 0.0, 1.0, 1.0),
            digest: bool = True, jpeg_image: str = "sdr") -> dict:
    """Measure one image and return its metric record.

    Percentiles are taken over every sample in the region, not a subsample, so
    peak memory scales with the frame: measured at **1.43 GB for 18.66 MP**
    (~77 bytes/pixel), which extrapolates to ~5.7 GB on a 10368x7200 scan — inside
    nc's own 6 GiB default budget, but not by much. The decode is in place (see
    `_decode_transfer`) and colour streams in row blocks; what remains is the
    float32 frame, the float64 log values, and the sort inside `np.percentile`. If
    that ceiling bites, decimate deliberately and record the stride in the
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

    if jpeg_image not in JPEG_IMAGES:
        raise MetricsError(
            f"--jpeg-image must be one of {', '.join(JPEG_IMAGES)}, got "
            f"{jpeg_image!r}")
    encoded, meta = read_image(path, jpeg_image)
    region = resolve_region(meta["width"], meta["height"], fraction)
    view = encoded[region["y"]:region["y"] + region["height"],
                   region["x"]:region["x"] + region["width"], :]

    endpoints = endpoint_stats(view, meta)
    linear = _decode_transfer(view, space.transfer)
    weights = luminance_weights(space)
    tone = tone_stats(linear, weights)
    # Composed here rather than inside `tone_stats`, which is not streamed: the
    # histogram walks row blocks so it never holds a second full-frame temporary.
    tone["histogram"] = histogram_stats(linear, weights)
    color = color_stats(linear, space, weights)

    record = dict(
        schema_version=SCHEMA,
        file=path.name,
        image=dict(width=meta["width"], height=meta["height"],
                   dtype=meta["dtype"], container=meta["container"],
                   bits_per_sample=meta["bits_per_sample"],
                   extra_channels_ignored=meta["extra_channels_ignored"],
                   **{key: meta[key] for key in
                      ("decoder", "gain_map_present", "jpeg_image")
                      if key in meta}),
        space=space.describe(),
        region=region,
        bands=describe_bands(),
        endpoints=endpoints,
        tone=tone,
        color=color,
    )
    if digest:
        record["sha256"] = sha256(path)
    return record


# -- resolving a converted roll's colour space --------------------------------
#
# Derived from the run's **frozen recipe**, which is recorded provenance, not from
# the pixels — a different thing entirely from the guessing this module refuses to
# do. Only the presets whose output space is documented and verified appear here;
# everything else must be declared with --space, because being wrong produces a
# plausible wrong table rather than an error.
#
# Verified by conversion + exiftool on 2026-09-02:
#   legacy / compatibility -> "sRGB built-in"
#   display-p3             -> "RGB built-in"   (16-bit)
#   film-master            -> "RGB built-in"   (32-bit float)
#   hdr-linear-tiff        -> "NC Display-Linear BT.2020 (D65)"
# Note display-p3 and film-master carry the *same* profile description, so the ICC
# metadata cannot tell them apart. The preset can.

PRESET_SPACES: dict[str, str] = {
    # The gain-map pair is read as its **SDR base**, which is what
    # `--jpeg-image sdr` (the default) measures. Its base is Display P3, and the
    # record says `gain_map_present: true` so nobody mistakes the base for the
    # rendition an HDR-aware viewer shows.
    "gain-map-hdr": "display-p3",
    "ultra-hdr-v1": "display-p3",
    # Retired from nc (`nf-retire/legacy-custom`), kept because the reference build
    # still writes them and a review set measures its renders.
    "legacy": "srgb",
    "custom": "srgb",
    "compatibility": "srgb",
    "display-p3": "display-p3",
    "film-master": "linear-acescg",
    "hdr-linear-tiff": "linear-bt2020",
}

#: Presets whose output this command cannot read, with the reason. Distinguished
#: from "unknown preset" so the message can say whether the problem is the
#: container or the transfer.
PRESET_UNREADABLE: dict[str, str] = {
    "hdr-pq": "writes AVIF",
    "hdr-hlg": "writes AVIF",
    "hdr-pq-tiff": "is PQ-encoded, which needs a reference-white normalization "
                   "this command does not implement",
    "hdr-hlg-tiff": "is HLG-encoded; see hdr-pq-tiff",
}

#: `--output-profile` overrides the space on the two non-atomic presets.
PROFILE_SPACES: dict[str, str] = {
    "srgb": "srgb",
    # nc's own ProPhoto output, not a third party's — see the two ProPhoto spaces.
    "prophoto": "prophoto-gamma1.8",
    "display-p3": "display-p3",
    "acescg": "linear-acescg",
}


#: The new chain's destinations (`crate::destination` in nc), keyed on what fixes the
#: pixels' space — the gamut and the transfer. The range and the container do not
#: change it. Verified against the profiles nc embeds (`pipeline::color`).
DESTINATION_SPACES: dict[tuple[str, str], str] = {
    ("display-p3", "native"): "display-p3",
    ("adobe-rgb", "native"): "adobe-rgb",
    ("bt2020", "linear"): "linear-bt2020",
}

#: New-chain transfers this command cannot read, with the reason.
DESTINATION_UNREADABLE: dict[str, str] = {
    "pq": PRESET_UNREADABLE["hdr-pq-tiff"],
    "hlg": PRESET_UNREADABLE["hdr-hlg-tiff"],
}

#: The four axes of a new-chain destination, as its recipe `output.display` names them.
DESTINATION_AXES = ("range", "transfer", "gamut", "container")


def space_for_destination(output: object) -> tuple[str, str]:
    """The colour space a new-chain destination writes, and why.

    `output` is the recipe's `output` value — `"film-master"`, or `{"display": {...}}`
    with **every** axis stated, as nc's report records the resolved destination
    (`new_flow.destination`). An axis left to nc's derivation is refused rather than
    derived here: a second copy of the destination table is the drift the table exists
    to prevent.
    """
    if output == "film-master":
        return PRESET_SPACES["film-master"], "film-master destination"
    display = output.get("display") if isinstance(output, dict) else None
    if not isinstance(display, dict):
        raise MetricsError(
            f"unrecognised new-chain destination {output!r}; declare the space with --space")
    missing = [axis for axis in DESTINATION_AXES if not isinstance(display.get(axis), str)]
    if missing:
        raise MetricsError(
            "the destination leaves " + ", ".join(missing) + " to nc's derivation; read "
            "the resolved one from the report's new_flow.destination, or declare --space")
    if display["container"] == "avif":
        raise MetricsError("the destination writes AVIF")
    if display["transfer"] in DESTINATION_UNREADABLE:
        raise MetricsError(
            f"the destination {DESTINATION_UNREADABLE[display['transfer']]}")
    key = (display["gamut"], display["transfer"])
    if key not in DESTINATION_SPACES:
        raise MetricsError(
            f"no verified colour space for gamut {key[0]!r} with transfer {key[1]!r}; "
            "declare the space with --space")
    return DESTINATION_SPACES[key], f"{key[0]} {key[1]} destination"


def space_for_recipe(recipe: dict) -> tuple[str, str]:
    """The colour space a frozen recipe's output is in, and why.

    Raises `MetricsError` rather than falling back to a default: an
    under-determined space is exactly the condition that makes every number in
    the artifact wrong while every number still looks reasonable.
    """
    if recipe.get("recipe_version") == 2:
        # The new chain's recipe: its `output` is a destination, not a preset.
        return space_for_destination(recipe.get("output", {"display": {}}))
    output = recipe.get("output") if isinstance(recipe.get("output"), dict) else {}
    preset = output.get("preset", "gain-map-hdr")
    if preset in PRESET_UNREADABLE:
        raise MetricsError(
            f"the run's output preset {preset!r} {PRESET_UNREADABLE[preset]}")
    if preset not in PRESET_SPACES:
        raise MetricsError(
            f"unknown output preset {preset!r} in the frozen recipe; declare the "
            "space with --space")

    if preset in ("legacy", "custom"):
        # These two are the only presets that accept the depth/profile selectors,
        # so they are the only ones whose space is not fixed by the preset name.
        profile = output.get("output_profile")
        if isinstance(profile, str):
            key = profile.lower()
            if key not in PROFILE_SPACES:
                raise MetricsError(
                    f"the recipe pins output.output_profile = {profile!r}, which is "
                    "a path or a profile this command has no primaries for; declare "
                    "the space with --space")
            return PROFILE_SPACES[key], f"{preset} + output_profile {profile}"
        if output.get("depth") == "f32":
            # A rendered float TIFF in the selected output space. Whether its
            # samples carry the profile's transfer or are linear was not
            # established, and guessing decides every tone number, so it is
            # refused rather than assumed.
            raise MetricsError(
                f"{preset} with output.depth = f32 has an output space this command "
                "has not verified; declare it with --space")
        return PRESET_SPACES[preset], f"{preset} default profile"
    return PRESET_SPACES[preset], f"{preset} preset"


# -- roll rollup --------------------------------------------------------------

#: The scalars the rollup tracks across a roll's frames. Each is one number a
#: reader can act on, and each is a *stated* axis rather than a whole record, so
#: the spread table stays legible at 30 frames.
AXES: tuple[tuple[str, tuple[str, ...]], ...] = (
    ("key_stops", ("tone", "key_stops")),
    ("median_stops", ("tone", "percentiles_stops", "p50")),
    ("contrast_p95_p5", ("tone", "contrast", "p95_minus_p5")),
    ("shoulder_span", ("tone", "shoulder_span_stops")),
    ("toe_span", ("tone", "toe_span_stops")),
    ("deep_shadow", ("tone", "bands", "deep_shadow")),
    # Trackable only since the schema 2 band cut: on the stops-even edges this
    # band was 0.47 stops wide and read 0.00 on four of five renders of a frame,
    # so an axis built on it said nothing about a roll.
    ("highlight", ("tone", "bands", "highlight")),
    ("above_diffuse_white", ("tone", "bands", "above_diffuse_white")),
    ("at_top_code", ("endpoints", "at_or_above_white", "any")),
    ("cast", ("color", "mean_cast")),
    ("chroma", ("color", "mean_chroma")),
    ("neutral", ("color", "neutral_fraction")),
    ("r_over_g", ("color", "balance_stops", "r_over_g")),
    ("b_over_g", ("color", "balance_stops", "b_over_g")),
)


def _dig(record: dict, path: tuple[str, ...]):
    value = record
    for key in path:
        if not isinstance(value, dict) or key not in value:
            return None
        value = value[key]
    return value if isinstance(value, (int, float)) else None


def frame_axes(record: dict) -> dict:
    """One frame's tracked scalars, plus the two crossover terms.

    Crossover — the cast drifting with brightness — is the characteristic
    negative-conversion fault. It is not a field of the per-image record; it is
    the *difference* between two of its bands, so it is derived here rather than
    stored twice.

    The two bands are **`shadow` and `mid`**, not shadow and highlight. Those two
    carry the frame on essentially every render — measured across 33 of them, the
    smallest `shadow` was 4.2% of the region and the smallest `mid` 5.7% — while
    `highlight` legitimately empties on a dark frame, which would make the axis
    vanish exactly where a render is darkest. Read it as shadow-to-midtone drift;
    the full per-band casts are in `color.cast_by_tone_band` for anything finer.

    Either band being `sparse` withholds the axis rather than reporting it. A
    difference of two means is only as good as the thinner of them, and a roll
    spread is worse than useless if one frame's entry rests on a few hundred
    pixels — the reader cannot tell which one did.
    """
    axes = {}
    for name, path in AXES:
        value = _dig(record, path)
        if value is not None:
            axes[name] = value
    bands = record.get("color", {}).get("cast_by_tone_band", {})
    dark, light = bands.get("shadow"), bands.get("mid")
    if (isinstance(dark, dict) and isinstance(light, dict)
            and not dark.get("sparse") and not light.get("sparse")):
        axes["crossover_a"] = _round(light.get("mean_a", 0) - dark.get("mean_a", 0))
        axes["crossover_b"] = _round(light.get("mean_b", 0) - dark.get("mean_b", 0))
    return axes


def spread(frames: list[dict]) -> dict:
    """Dispersion of each axis across the roll, and which frame sits at each end.

    The spread is reported rather than the mean because the mean is close to
    meaningless here: frame 3 is a backlit portrait and frame 11 a shaded street,
    so their exposures *should* differ and averaging them describes the subjects.

    What the spread is **not** is attributable. One frozen recipe served every
    frame, so variation combines scene content with how well that calibration
    fits — those cannot be separated from one roll's numbers, and a measured
    b_over_g range of 0.6 stops across three Ektar frames is as easily three
    different scenes as a calibration that does not fit. Hence no outlier rule
    and no verdict: the extremes are named so a human can look at those frames.
    """
    result = {}
    for axis in [name for name, _ in AXES] + ["crossover_a", "crossover_b"]:
        present = [(frame["axes"][axis], frame["frame"]) for frame in frames
                   if axis in frame.get("axes", {})]
        if len(present) < 2:
            continue
        present.sort(key=lambda pair: (pair[0], pair[1]))
        values = [value for value, _ in present]
        middle = len(values) // 2
        median = (values[middle] if len(values) % 2
                  else (values[middle - 1] + values[middle]) / 2)
        result[axis] = dict(
            min=_round(values[0]), min_frame=present[0][1],
            median=_round(median),
            max=_round(values[-1]), max_frame=present[-1][1],
            spread=_round(values[-1] - values[0]),
            frames=len(values),
        )
    return result


# -- markdown -----------------------------------------------------------------

#: How each axis reads in a report: its unit and how many decimals are worth
#: printing. The JSON keeps every axis as a plain fraction or a raw value — this
#: is the presentation layer, and the only place a fraction becomes a percentage.
AXIS_FORMAT: dict[str, tuple[str, int]] = {
    "key_stops": ("stops", 2),
    "median_stops": ("stops", 2),
    "contrast_p95_p5": ("stops", 2),
    "shoulder_span": ("stops", 2),
    "toe_span": ("stops", 2),
    "deep_shadow": ("%", 2),
    "highlight": ("%", 2),
    "above_diffuse_white": ("%", 2),
    "at_top_code": ("%", 2),
    "neutral": ("%", 2),
    "cast": ("Lab", 1),
    "chroma": ("Lab", 1),
    "r_over_g": ("stops", 3),
    "b_over_g": ("stops", 3),
    "crossover_a": ("Lab", 1),
    "crossover_b": ("Lab", 1),
}

#: Columns of the per-frame table, in reading order: exposure, then contrast,
#: then range use, then colour. Narrower than the JSON on purpose — the table is
#: for a human scanning a roll, and the record is for the diff.
TABLE_COLUMNS: tuple[tuple[str, str], ...] = (
    ("key_stops", "key"),
    ("median_stops", "p50"),
    ("contrast_p95_p5", "p95-p5"),
    ("shoulder_span", "shldr"),
    ("deep_shadow", "deep %"),
    ("highlight", "high %"),
    ("at_top_code", "top %"),
    ("cast", "cast"),
    ("b_over_g", "B/G"),
    ("crossover_b", "xover b"),
)


def format_axis(axis: str, value: float | None) -> str:
    """One axis value as it appears in a report.

    Fixed-point always: a `1e-06` in a Markdown cell is correct and unreadable,
    and it is what `at_top_code` produced. A value that rounds to zero but is not
    zero renders as `<0.01` rather than `0.00`, because "nothing clipped" and
    "one pixel in a million clipped" are different findings.
    """
    if value is None:
        return "-"
    unit, places = AXIS_FORMAT.get(axis, ("", 3))
    if unit == "%":
        value = value * 100.0
    text = f"{value:.{places}f}"
    if float(text) == 0.0 and value != 0.0:
        smallest = f"{10.0 ** -places:.{places}f}"
        text = f"<{smallest}" if value > 0 else f">-{smallest}"
    return text


def _identity_lines(record: dict) -> list[str]:
    """Which nc build produced the images.

    A committed report that does not say this is hard to trust later, and
    `git_dirty` is the part a reader most needs: those images came from a tree
    with uncommitted changes and cannot be reproduced from a commit alone.
    """
    identity = record.get("identity")
    if not isinstance(identity, dict) or not identity:
        return ["- Build: not recorded in the run's tags"]
    commit = identity.get("git_commit", "unknown")
    dirty = " + **uncommitted changes**" if identity.get("git_dirty") else ""
    parts = [f"- Build: nc {identity.get('nc_version', '?')}, "
             f"commit `{commit}`{dirty}"]
    detail = []
    for key, label in (("pipeline_version", "pipeline"), ("params_hash", "params"),
                       ("target", "target")):
        if identity.get(key) is not None:
            detail.append(f"{label} `{identity[key]}`")
    if detail:
        parts.append("- " + ", ".join(detail).capitalize())
    return parts


def markdown_table(record: dict) -> str:
    """Render a roll metrics record as Markdown.

    Separate from the measuring so it can be re-run on a stored record, and so
    the layout can change without re-reading 20 frames of pixels.
    """
    lines: list[str] = []
    roll = record.get("roll", "(unknown)")
    config = record.get("config", "(unknown)")
    lines.append(f"# {roll} / {config}")
    lines.append("")
    space = record.get("space", {})
    lines.append(f"- Colour space: `{space.get('declared')}` "
                 f"({space.get('source', 'declared')})")
    region = record.get("region_fraction")
    if region:
        lines.append(f"- Region: `{region}` of each frame")
    frames = record.get("frames", [])
    lines.append(f"- Frames measured: {len(frames)}")
    skipped = record.get("skipped") or []
    if skipped:
        lines.append(f"- Frames skipped: {len(skipped)} "
                     + ", ".join(f"`{entry.get('frame')}`" for entry in skipped))
    lines.extend(_identity_lines(record))
    lines.append("")

    if frames:
        header = ["frame"] + [label for _, label in TABLE_COLUMNS]
        lines.append("| " + " | ".join(header) + " |")
        lines.append("|" + "|".join(["---"] * len(header)) + "|")
        for frame in frames:
            axes = frame.get("axes", {})
            cells = [frame.get("frame", "?")]
            cells += [format_axis(key, axes.get(key)) for key, _ in TABLE_COLUMNS]
            lines.append("| " + " | ".join(cells) + " |")
        lines.append("")
        lines.append("Units: `key`, `p50`, `p95-p5`, `shldr` and `B/G` in stops; "
                     "`deep`, `high` and `top` as a share of the region; `cast` and "
                     "`xover b` in CIELAB units.")
        lines.append("")

    dispersion = record.get("spread", {})
    if dispersion:
        lines.append("## Spread across the roll")
        lines.append("")
        lines.append("One frozen recipe served every frame, so frame-to-frame "
                     "variation combines what was photographed with how well that "
                     "one calibration fits the roll — it does not separate them. "
                     "A wide spread is a prompt to look at the named frames, not "
                     "evidence of miscalibration.")
        lines.append("")
        lines.append("| axis | unit | min | median | max | spread | "
                     "min frame | max frame |")
        lines.append("|---|---|---|---|---|---|---|---|")
        # Explicit order, not the dict's: the artifact is written with sorted keys,
        # so relying on insertion order rendered a stored record alphabetically and
        # an in-memory one in AXES order — the same record, two different tables.
        ordered = [name for name, _ in AXES] + ["crossover_a", "crossover_b"]
        for axis in ordered + sorted(set(dispersion) - set(ordered)):
            values = dispersion.get(axis)
            if values is None:
                continue
            unit = AXIS_FORMAT.get(axis, ("", 3))[0]
            cells = [format_axis(axis, values[key])
                     for key in ("min", "median", "max", "spread")]
            lines.append(
                f"| {axis} | {unit or '-'} | " + " | ".join(cells)
                + f" | {values['min_frame']} | {values['max_frame']} |")
        lines.append("")
        lines.append("A `%` axis's spread is in percentage points.")
        lines.append("")

    note = record.get("note")
    if note:
        lines.append(f"> {note}")
        lines.append("")
    return "\n".join(lines)


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
        record = measure(path, args.space, fraction, digest=not args.no_checksum,
                         jpeg_image=getattr(args, "jpeg_image", "sdr"))
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


def cmd_roll(args) -> int:
    """Measure every converted frame of one roll and roll the scalars up."""
    from . import roll as _roll

    try:
        require_dependencies()
        # `is not None`, matching cmd_image: `--region ""` is a malformed region,
        # not an absent one.
        if args.region is not None and args.inset is not None:
            raise MetricsError("--region and --inset both select a region; pass one")
        if args.region is not None:
            fraction = parse_region(args.region)
        elif args.inset is not None:
            fraction = inset_fraction(args.inset)
        else:
            fraction = (0.0, 0.0, 1.0, 1.0)

        root = Path(args.asset_root).resolve()
        tag_path = _roll._tag_path(root, args.roll, args.run)
        tag, error = _roll._load_object(tag_path)
        if error:
            raise MetricsError(error)
        assert tag is not None
        if tag.get("kind") != "nctool-roll-conversion":
            raise MetricsError(f"{tag_path} is not an nctool roll tag")
        if tag.get("roll") != args.roll:
            raise MetricsError(
                f"tags are for roll {tag.get('roll')!r}, not {args.roll!r}")

        recipe = tag.get("recipe") if isinstance(tag.get("recipe"), dict) else {}
        if args.space:
            # Validated here, not on first use. `measure()` would raise the same
            # error per frame, where the loop swallows it into `skipped` — so a
            # typo decoded the whole roll and then reported "no frame could be
            # measured", naming neither the fault nor the remedy, and returned
            # before writing the `skipped` list that carried the reason.
            space_name, space_source = args.space, "declared on the command line"
            if space_name in REFUSED:
                raise MetricsError(f"colour space {space_name!r} is not supported "
                                   f"here: {REFUSED[space_name]}")
            if space_name not in SPACES:
                raise MetricsError(
                    f"unknown colour space {space_name!r}. Declare one of: "
                    + ", ".join(sorted(SPACES)))
        else:
            space_name, space_source = space_for_recipe(recipe)

        report_ref = tag.get("report_file")
        if not isinstance(report_ref, str):
            raise MetricsError("tags have no report_file")
        report_path = Path(report_ref)
        if not report_path.is_absolute():
            report_path = root / report_path
        report, error = _roll._load_object(report_path)
        if error:
            raise MetricsError(error)
        assert report is not None
    except MetricsError as error:
        print(f"error: {error}", file=sys.stderr)
        return 2

    frames: list[dict] = []
    skipped: list[dict] = []
    for entry in sorted(report.get("frames", []),
                        key=lambda frame: str(frame.get("input"))):
        if not isinstance(entry, dict):
            continue
        name = Path(str(entry.get("input", "?"))).name
        if entry.get("status") != "ok":
            skipped.append(dict(frame=name, reason=str(entry.get("status"))))
            continue
        output = entry.get("output")
        if not isinstance(output, str):
            skipped.append(dict(frame=name, reason="the report names no output file"))
            continue
        path = Path(output)
        if not path.is_absolute():
            path = root / path
        if not path.is_file():
            # Relative to the asset root: an absolute path in the artifact makes
            # it machine-specific, which is what the rest of these records avoid.
            try:
                shown = path.relative_to(root)
            except ValueError:
                shown = path
            skipped.append(dict(frame=name, reason=f"missing output {shown}"))
            continue
        try:
            record = measure(path, space_name, fraction, digest=False,
                             jpeg_image=getattr(args, "jpeg_image", "sdr"))
        except MetricsError as error:
            # One unreadable frame must not lose the rest of the roll, and it must
            # not vanish either: it lands in `skipped`, which the artifact carries.
            skipped.append(dict(frame=name, reason=str(error)))
            continue
        frames.append(dict(frame=name, output=str(path.name),
                           axes=frame_axes(record), metrics=record))
        print(f"  {name}: key={record['tone'].get('key_stops')} "
              f"cast={record['color'].get('mean_cast')}", file=sys.stderr)

    if not frames:
        print("error: no frame of this roll could be measured"
              + (f" ({len(skipped)} skipped)" if skipped else ""), file=sys.stderr)
        return 1

    out = dict(
        schema_version=SCHEMA,
        kind="nctool-roll-metrics",
        roll=args.roll,
        config=tag.get("config"),
        identity=tag.get("identity"),
        space=dict(source=space_source, **SPACES[space_name].describe()),
        bands=describe_bands(),
        region_fraction=list(fraction),
        frames=frames,
        skipped=skipped,
        spread=spread(frames),
        note=("Pixel-derived measurement of this roll's own output images. The "
              "spread is the point, but it is not attributable on its own: one "
              "frozen recipe served every frame, so variation combines scene "
              "content with calibration fit. No axis here is a verdict."),
    )

    out_path = (Path(args.out).resolve() if args.out
                else tag_path.with_name("metrics.json"))
    try:
        _roll._write_json(out_path, out)
        if args.markdown:
            table = Path(args.markdown).resolve()
            table.parent.mkdir(parents=True, exist_ok=True)
            table.write_text(markdown_table(out), encoding="utf-8")
    except OSError as error:
        print(f"error: cannot write: {error}", file=sys.stderr)
        return 2
    print(f"wrote {out_path} ({len(frames)} frames"
          + (f", {len(skipped)} skipped)" if skipped else ")"), file=sys.stderr)
    if args.markdown:
        print(f"wrote {args.markdown}", file=sys.stderr)
    return 0 if not skipped else 1


def cmd_table(args) -> int:
    """Render a stored roll metrics record as Markdown."""
    path = Path(args.record)
    try:
        record = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        print(f"error: cannot read {path}: {error}", file=sys.stderr)
        return 2
    if not isinstance(record, dict) or record.get("kind") != "nctool-roll-metrics":
        print(f"error: {path} is not an nctool roll metrics record", file=sys.stderr)
        return 2
    # Refused rather than rendered, because the renderer's columns would still
    # fit: schema 1 cut the tone bands on stops-even edges, so its `deep_shadow`
    # means "below -4 stops" where this build's means "below L* 15" (-3.24
    # stops). The table would look right and compare two definitions of shadow.
    version = record.get("schema_version")
    if version != SCHEMA:
        print(f"error: {path} is a schema {version} metrics record and this build "
              f"renders schema {SCHEMA}; the tone bands were re-cut between them, "
              "so re-measure the roll rather than re-rendering this record",
              file=sys.stderr)
        return 2
    text = markdown_table(record)
    if args.out:
        out = Path(args.out)
        out.parent.mkdir(parents=True, exist_ok=True)
        out.write_text(text, encoding="utf-8")
        print(f"wrote {out}", file=sys.stderr)
    else:
        sys.stdout.write(text)
    return 0
