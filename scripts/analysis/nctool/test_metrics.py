"""Unit tests for `nctool.metrics` (run: `python3 -m unittest`).

Hermetic: every image is synthesized in the test, so no Drive asset and no `nc`
binary is needed. Two groups carry most of the weight.

*Colorimetry is not restated here, it is checked against the Rust.*
`src/pipeline/colorimetry/` is the repository's single source of truth, so these
tests re-read `definitions.rs` and the generated `derived-artifacts.txt` and fail
if the Python table drifts from either. That is what keeps a transcription from
quietly becoming a second source of truth.

*The rest target failures that would be silent.* A wrong colour-space declaration
produces a plausible table rather than an error, so a test pins that the same file
measures differently under two declarations. Luminance at or below zero has no
logarithm, so a test pins that it is counted rather than folded to a floor —
folding it would invent shadow detail nobody could see in the output.
"""
from __future__ import annotations

import io
import json
import math
import os
import re
import sys
import tempfile
import unittest
from contextlib import redirect_stderr, redirect_stdout
from pathlib import Path

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from nctool import metrics  # noqa: E402

try:
    import numpy as np
    import PIL  # noqa: F401  (JPEG input; `from PIL import Image` inside tests)
    import tifffile
    HAVE_DEPS = True
except ImportError:  # pragma: no cover - exercised by the guard test below
    HAVE_DEPS = False

REPO = Path(__file__).resolve().parents[3]
COLORIMETRY = REPO / "src" / "pipeline" / "colorimetry"

needs_deps = unittest.skipUnless(
    HAVE_DEPS,
    "numpy/Pillow/tifffile not installed (scripts/analysis/requirements.txt)")


class DependencyGuard(unittest.TestCase):
    """`NCTOOL_REQUIRE_DEPS=1` turns the skip into a failure.

    Without this, a CI job that forgot to install the requirements would run the
    metrics tests as *skips* and still print `test result: ok` — the same trap
    CLAUDE.md records for a `cargo test` filter that matches nothing.
    """

    def test_dependencies_present_when_required(self):
        if os.environ.get("NCTOOL_REQUIRE_DEPS") == "1":
            self.assertTrue(HAVE_DEPS,
                            "NCTOOL_REQUIRE_DEPS=1 but numpy/Pillow/tifffile are "
                            "missing; the metrics tests would have silently "
                            "skipped")


# -- colorimetry is checked against the Rust source of truth ------------------


class ColorimetrySource(unittest.TestCase):
    def _rust_colorspaces(self) -> dict:
        """Parse `pub const NAME: ColorSpace = ColorSpace { ... }` out of the Rust."""
        text = (COLORIMETRY / "definitions.rs").read_text(encoding="utf-8")
        block = re.compile(
            r"pub const (\w+): ColorSpace = ColorSpace \{.*?"
            r"primaries: Primaries::new\(\s*"
            r"xy\(([-\d.]+), ([-\d.]+)\),\s*"
            r"xy\(([-\d.]+), ([-\d.]+)\),\s*"
            r"xy\(([-\d.]+), ([-\d.]+)\),?\s*\),\s*"
            r"white: (\w+),",
            re.DOTALL)
        found = {}
        for match in block.finditer(text):
            name = match.group(1)
            values = [float(match.group(i)) for i in range(2, 8)]
            found[name] = (((values[0], values[1]), (values[2], values[3]),
                            (values[4], values[5])), match.group(8))
        return found

    def _rust_white(self, symbol: str) -> tuple[float, float]:
        text = (COLORIMETRY / "definitions.rs").read_text(encoding="utf-8")
        match = re.search(
            rf"pub const {symbol}: Chromaticity = xy\(([-\d.]+), ([-\d.]+)\);", text)
        assert match, f"{symbol} not found in definitions.rs"
        return (float(match.group(1)), float(match.group(2)))

    def test_primaries_match_definitions_rs(self):
        rust = self._rust_colorspaces()
        mapping = {"rec709": "REC709", "display-p3": "DISPLAY_P3",
                   "adobe-rgb": "ADOBE_RGB", "bt2020": "BT2020",
                   "prophoto": "PROPHOTO", "acescg": "ACESCG"}
        for local, symbol in mapping.items():
            self.assertIn(symbol, rust, f"{symbol} disappeared from definitions.rs")
            self.assertEqual(metrics.PRIMARIES[local], rust[symbol][0],
                             f"{local} primaries drifted from definitions::{symbol}")

    def test_white_points_match_definitions_rs(self):
        self.assertEqual(metrics.WHITE["d65"], self._rust_white("D65"))
        self.assertEqual(metrics.WHITE["d50"], self._rust_white("D50"))
        self.assertEqual(metrics.WHITE["aces"], self._rust_white("ACES_WHITE"))

    def test_every_transcribed_space_is_covered_by_the_rust_cross_check(self):
        """The maps above are hand-written, so a new space can be added to
        `PRIMARIES` without joining the check that keeps it honest — which is what
        happened when ACEScg arrived, caught only because the white-point map
        raised a KeyError.
        """
        checked = {"rec709", "display-p3", "adobe-rgb", "bt2020", "prophoto",
                   "acescg"}
        self.assertEqual(set(metrics.PRIMARIES), checked,
                         "a space in PRIMARIES is not cross-checked against "
                         "definitions.rs; add it to the maps in this class")
        self.assertEqual({space.white for space in metrics.SPACES.values()},
                         set(metrics.WHITE))

    def test_space_whites_match_definitions_rs(self):
        rust = self._rust_colorspaces()
        symbols = {"rec709": "REC709", "display-p3": "DISPLAY_P3",
                   "adobe-rgb": "ADOBE_RGB", "bt2020": "BT2020",
                   "prophoto": "PROPHOTO", "acescg": "ACESCG"}
        expected = {"D65": "d65", "D50": "d50", "ACES_WHITE": "aces"}
        for space in metrics.SPACES.values():
            rust_white = rust[symbols[space.primaries]][1]
            self.assertEqual(space.white, expected[rust_white],
                             f"{space.name} adopted white disagrees with the Rust")

    def test_bradford_matches_definitions_rs(self):
        text = (COLORIMETRY / "definitions.rs").read_text(encoding="utf-8")
        block = re.search(r"pub const BRADFORD: ConeResponse = ConeResponse \{.*?"
                          r"matrix: \[(.*?)\],\s*inverse: None", text, re.DOTALL)
        assert block, "definitions::BRADFORD not found"
        numbers = [float(v) for v in re.findall(r"-?\d+\.\d+", block.group(1))]
        self.assertEqual(len(numbers), 9)
        flat = [v for row in metrics.BRADFORD for v in row]
        self.assertEqual(flat, numbers)

    def _audit_derived(self, section: str) -> list[float]:
        """The `derived=` column of one `derived-artifacts.txt` section."""
        text = (COLORIMETRY / "derived-artifacts.txt").read_text(encoding="utf-8")
        body = text.split(f"[{section}]", 1)[1].split("\n[", 1)[0]
        return [float(v) for v in re.findall(r"derived=(-?[\d.e+-]+)", body)]

    def test_luma_matches_rust_audit_derivation(self):
        """Python's derivation reproduces the Rust audit's binary64 derivation.

        Both derive from the same primaries, so agreement to 1e-12 says the two
        implementations of the same standard math agree — a real check, since
        this module derives its matrices independently rather than importing them.
        """
        for space, section in (("srgb", "SRGB_LUMA"),
                               ("display-p3", "DISPLAY_P3_LUMA")):
            derived = self._audit_derived(section)
            ours = metrics.luminance_weights(metrics.SPACES[space])
            for got, want in zip(ours, derived):
                self.assertAlmostEqual(got, want, delta=1e-12,
                                       msg=f"{space} luma vs {section}")

    def test_bt2020_derivation_deliberately_differs_from_the_tabulated_vector(self):
        """BT.2020's tabulated luma is not a linear-light luminance weighting.

        `definitions::BT2020_LUMA_TABULATED` is `[0.2627, 0.6780, 0.0593]`, the
        rounded non-constant-luminance coefficients an encoder applies to
        *transfer-encoded* values. Deriving from the primaries — which is what a
        luminance statistic needs — gives something ~2e-6 away. This test pins the
        gap so nobody "corrects" one to match the other; CLAUDE.md records that
        the standard's rounding is deliberate and that decoders invert the rounded
        form.
        """
        tabulated = self._audit_derived("BT2020_LUMA")
        ours = metrics.luminance_weights(metrics.SPACES["linear-bt2020"])
        largest = max(abs(a - b) for a, b in zip(ours, tabulated))
        self.assertGreater(largest, 1e-7, "the two forms have become identical")
        self.assertLess(largest, 1e-5, "the derivation moved further than documented")

    def test_adobe_rgb_shares_rec709_red_and_blue(self):
        """Only green differs, which is also the easy thing to transcribe wrongly.

        `definitions::ADOBE_RGB` asserts the same relationship on the Rust side;
        this is the Python half, so a one-sided edit cannot pass both suites.
        """
        rec709 = metrics.PRIMARIES["rec709"]
        adobe = metrics.PRIMARIES["adobe-rgb"]
        self.assertEqual(adobe[0], rec709[0])
        self.assertEqual(adobe[2], rec709[2])
        self.assertNotEqual(adobe[1], rec709[1])

    def test_adobe_rgb_luma_moves_the_way_its_green_primary_implies(self):
        """nc pins no Adobe RGB artifact, so `derived-artifacts.txt` cannot check
        this one. What *is* checkable is the relationship to Rec.709.

        Adobe RGB's green primary is less yellow and more saturated than
        Rec.709's, so normalizing to the same white shifts luminance weight off
        green and onto red. Blue's primary is unchanged but its weight still moves
        slightly, because the whole matrix is renormalized.

        A coarse absolute bound is kept beside it, at three decimals rather than
        four: published RGB->XYZ tables for this space round D65 to five decimals
        where `definitions::D65` rounds to four, and the two derivations disagree
        at ~3e-5 for that reason alone. Three decimals is far inside any
        transcription error (a swapped primary moves these by >0.05) and far
        outside the rounding argument.
        """
        adobe = metrics.luminance_weights(metrics.SPACES["adobe-rgb"])
        rec709 = metrics.luminance_weights(metrics.SPACES["srgb"])
        self.assertGreater(adobe[0], rec709[0])
        self.assertLess(adobe[1], rec709[1])
        for got, want in zip(adobe, (0.297, 0.627, 0.075)):
            self.assertAlmostEqual(got, want, places=3)

    def test_the_two_prophoto_decodes_differ_only_below_the_romm_toe(self):
        """One space for ISO 22028-2 as specified, one for what nc writes.

        They must agree above the toe — otherwise one of them is simply wrong —
        and diverge sharply below it, which is the whole reason both exist.
        """
        if not HAVE_DEPS:
            self.skipTest("numpy not installed")
        above = np.array([0.05, 0.2, 0.5, 1.0], dtype=np.float32)
        romm = metrics._decode_transfer(above.copy(), "prophoto")
        pure = metrics._decode_transfer(above.copy(), "gamma1.8")
        for got, want in zip(romm, pure):
            self.assertAlmostEqual(float(got), float(want), places=6)

        below = np.array([0.01], dtype=np.float32)
        romm = float(metrics._decode_transfer(below.copy(), "prophoto")[0])
        pure = float(metrics._decode_transfer(below.copy(), "gamma1.8")[0])
        self.assertGreater(romm / pure, 2.0)

    def test_prophoto_adapts_to_d65(self):
        """ProPhoto is a D50 space; its luminance row must be Bradford-adapted.

        Without the adaptation the weights still sum to 1 and look plausible, so
        this pins that the adaptation actually happened.
        """
        unadapted = metrics.rgb_to_xyz("prophoto", "d50")[1]
        adapted = metrics.luminance_weights(metrics.SPACES["prophoto"])
        self.assertNotAlmostEqual(unadapted[0], adapted[0], places=3)
        self.assertAlmostEqual(sum(adapted), 1.0, places=9)

    def test_luma_rows_sum_to_one(self):
        for name, space in metrics.SPACES.items():
            self.assertAlmostEqual(sum(metrics.luminance_weights(space)), 1.0,
                                   places=9, msg=name)


class Transfer(unittest.TestCase):
    @needs_deps
    def test_srgb_decode_known_points(self):
        got = metrics._decode_transfer(np.array([0.0, 0.04045, 0.5, 1.0],
                                                dtype=np.float32), "srgb")
        self.assertAlmostEqual(float(got[0]), 0.0, places=6)
        self.assertAlmostEqual(float(got[1]), 0.04045 / 12.92, places=6)
        self.assertAlmostEqual(float(got[2]), ((0.5 + 0.055) / 1.055) ** 2.4, places=6)
        self.assertAlmostEqual(float(got[3]), 1.0, places=6)

    @needs_deps
    def test_negatives_decode_symmetrically(self):
        got = metrics._decode_transfer(np.array([-0.5, 0.5], dtype=np.float32), "srgb")
        self.assertAlmostEqual(float(got[0]), -float(got[1]), places=7)

    @needs_deps
    def test_adobe_rgb_decode_is_a_pure_power_law(self):
        """No linear segment near black, unlike sRGB and ProPhoto."""
        got = metrics._decode_transfer(
            np.array([0.0, 0.001, 0.5, 1.0], dtype=np.float32), "adobe-rgb")
        self.assertAlmostEqual(float(got[0]), 0.0, places=7)
        self.assertAlmostEqual(float(got[1]), 0.001 ** (563 / 256), places=7)
        self.assertAlmostEqual(float(got[2]), 0.5 ** (563 / 256), places=6)
        self.assertAlmostEqual(float(got[3]), 1.0, places=6)

    @needs_deps
    def test_prophoto_decode_known_points(self):
        got = metrics._decode_transfer(
            np.array([16.0 / 512.0 - 1e-6, 0.5, 1.0], dtype=np.float32), "prophoto")
        self.assertAlmostEqual(float(got[0]), (16.0 / 512.0 - 1e-6) / 16.0, places=6)
        self.assertAlmostEqual(float(got[1]), 0.5 ** 1.8, places=6)
        self.assertAlmostEqual(float(got[2]), 1.0, places=6)


# -- image helpers ------------------------------------------------------------


def write_tiff(directory: Path, name: str, array) -> Path:
    path = directory / name
    # `photometric` is stated because tifffile warns that it will stop inferring
    # RGB from a 3-sample array; an inferred MINISBLACK layout would make these
    # fixtures multi-page grayscale and every test would fail obscurely.
    kwargs = {"photometric": "rgb"} if array.ndim == 3 else {}
    tifffile.imwrite(str(path), array, **kwargs)
    return path


def flat_u16(value: int, size: int = 8):
    return np.full((size, size, 3), value, dtype=np.uint16)


class Measurement(unittest.TestCase):
    def setUp(self):
        if not HAVE_DEPS:
            self.skipTest("numpy/Pillow/tifffile not installed")
        self._tmp = tempfile.TemporaryDirectory(prefix="nctool-metrics-")
        self.dir = Path(self._tmp.name)
        self.addCleanup(self._tmp.cleanup)

    def test_mid_grey_measures_zero_stops(self):
        """A frame of exactly 18% linear grey sits at 0 stops by definition."""
        array = np.full((16, 16, 3), 0.18, dtype=np.float32)
        record = metrics.measure(write_tiff(self.dir, "grey.tif", array),
                                 "linear-srgb", digest=False)
        tone = record["tone"]
        self.assertAlmostEqual(tone["key_stops"], 0.0, places=5)
        for value in tone["percentiles_stops"].values():
            self.assertAlmostEqual(value, 0.0, places=5)
        self.assertAlmostEqual(tone["contrast"]["stdev_stops"], 0.0, places=6)

    def test_one_stop_apart_reads_as_one_stop(self):
        """Doubling luminance moves the key by exactly one stop."""
        base = np.full((16, 16, 3), 0.18, dtype=np.float32)
        record_a = metrics.measure(write_tiff(self.dir, "a.tif", base),
                                   "linear-srgb", digest=False)
        record_b = metrics.measure(write_tiff(self.dir, "b.tif", base * 2),
                                   "linear-srgb", digest=False)
        self.assertAlmostEqual(
            record_b["tone"]["key_stops"] - record_a["tone"]["key_stops"], 1.0,
            places=5)

    def test_percentiles_track_a_known_distribution(self):
        """A ramp of 101 distinct luminances puts p50 on its middle step.

        An odd count on purpose: percentiles are taken over the *log* values, so
        with an even count p50 interpolates between two steps and the expectation
        would have to interpolate in log space too. Landing p50 exactly on a
        sample tests the mapping instead of the interpolation rule.
        """
        steps = np.linspace(0.01, 1.0, 101, dtype=np.float32)
        array = np.repeat(steps.reshape(1, 101, 1), 3, axis=2)
        record = metrics.measure(write_tiff(self.dir, "ramp.tif", array),
                                 "linear-srgb", digest=False)
        percentiles = record["tone"]["percentiles_stops"]
        expected_median = math.log2(float(steps[50]) / metrics.MID_GREY)
        self.assertAlmostEqual(percentiles["p50"], expected_median, places=4)
        self.assertLess(percentiles["p5"], percentiles["p50"])
        self.assertLess(percentiles["p50"], percentiles["p95"])

    def test_endpoint_counts_are_exact(self):
        array = flat_u16(30000, size=10)
        array[0, :, 0] = 0           # 10 red samples at black
        array[1:3, :, 2] = 65535     # 20 blue samples at white
        record = metrics.measure(write_tiff(self.dir, "ends.tif", array),
                                 "srgb", digest=False)
        endpoints = record["endpoints"]
        self.assertEqual(endpoints["pixels"], 100)
        self.assertEqual(endpoints["samples"], 300)
        self.assertAlmostEqual(endpoints["at_or_below_black"]["r"], 0.10, places=6)
        self.assertAlmostEqual(endpoints["at_or_below_black"]["g"], 0.0, places=6)
        self.assertAlmostEqual(endpoints["at_or_above_white"]["b"], 0.20, places=6)
        self.assertAlmostEqual(endpoints["at_or_above_white"]["any"], 0.20, places=6)

    def test_bit_depth_does_not_change_the_measurement(self):
        """u8 and u16 encodings of one picture measure the same.

        Integer samples are divided by their own full scale, so depth is not a
        difference the comparison should ever report.
        """
        eight = np.full((8, 8, 3), 128, dtype=np.uint8)
        sixteen = np.full((8, 8, 3), 128 * 257, dtype=np.uint16)
        a = metrics.measure(write_tiff(self.dir, "8.tif", eight), "srgb", digest=False)
        b = metrics.measure(write_tiff(self.dir, "16.tif", sixteen), "srgb",
                            digest=False)
        self.assertAlmostEqual(a["tone"]["key_stops"], b["tone"]["key_stops"],
                               places=5)

    def test_non_positive_luminance_is_counted_not_folded(self):
        """Zero and negative luminance are excluded from the log statistics.

        Folding them to a floor would invent shadow detail; dropping them silently
        would overstate the key. They are counted instead.
        """
        array = np.full((10, 10, 3), 0.18, dtype=np.float32)
        array[0, :, :] = 0.0
        array[1, :, :] = -0.05
        record = metrics.measure(write_tiff(self.dir, "zeros.tif", array),
                                 "linear-srgb", digest=False)
        tone = record["tone"]
        self.assertAlmostEqual(tone["non_positive_pixel_fraction"], 0.20, places=6)
        self.assertEqual(tone["measured"], 80)
        self.assertAlmostEqual(tone["key_stops"], 0.0, places=5)
        self.assertAlmostEqual(record["endpoints"]["below_black"]["r"], 0.10,
                               places=6)

    def test_non_finite_samples_are_reported(self):
        array = np.full((10, 10, 3), 0.18, dtype=np.float32)
        array[0, 0, :] = np.nan
        record = metrics.measure(write_tiff(self.dir, "nan.tif", array),
                                 "linear-srgb", digest=False)
        self.assertEqual(record["endpoints"]["non_finite_samples"], 3)
        self.assertAlmostEqual(
            record["endpoints"]["non_finite_sample_fraction"], 0.01, places=6)
        self.assertAlmostEqual(record["tone"]["non_finite_pixel_fraction"], 0.01, places=6)

    def test_a_non_finite_sample_is_not_counted_at_black(self):
        """Substituting 0.0 for NaN before the comparison invented an endpoint
        population: a frame of 0.5 with one NaN pixel reported 1% at black."""
        array = np.full((10, 10, 3), 0.5, dtype=np.float32)
        array[0, 0, :] = np.nan
        record = metrics.measure(write_tiff(self.dir, "nanblack.tif", array),
                                 "linear-srgb", digest=False)
        endpoints = record["endpoints"]
        self.assertEqual(endpoints["at_or_below_black"]["any"], 0.0)
        self.assertEqual(endpoints["below_black"]["any"], 0.0)
        self.assertEqual(endpoints["non_finite_samples"], 3)

    def test_bands_partition_the_frame(self):
        rng = np.random.default_rng(20260902)
        array = rng.random((64, 64, 3), dtype=np.float32)
        record = metrics.measure(write_tiff(self.dir, "rand.tif", array),
                                 "linear-srgb", digest=False)
        bands = record["tone"]["bands"]
        self.assertAlmostEqual(sum(bands.values()), 1.0, places=4)

    def test_bands_still_partition_with_non_finite_and_non_positive_samples(self):
        """The case the random fixture above can never produce.

        `rng.random` yields no NaN and no zero, so the partition claim held there
        by luck. Luminance that is non-finite or non-positive has no logarithm and
        no tone band; each needs an entry of its own or the shares quietly sum to
        finite/total instead of 1.
        """
        array = np.full((10, 10, 3), 0.18, dtype=np.float32)
        array[0, :, :] = np.nan
        array[1, :, :] = 0.0
        array[2, :, :] = -0.2
        record = metrics.measure(write_tiff(self.dir, "mixed.tif", array),
                                 "linear-srgb", digest=False)
        bands = record["tone"]["bands"]
        self.assertAlmostEqual(bands["non_finite"], 0.10, places=6)
        self.assertAlmostEqual(bands["non_positive"], 0.20, places=6)
        self.assertAlmostEqual(sum(bands.values()), 1.0, places=6)

    def test_declaring_the_wrong_space_changes_the_answer(self):
        """The reason the space is never inferred.

        One file, two declarations, no error either way — only a materially
        different table. If this ever stopped differing, the declaration would be
        decorative and a guess would look safe.
        """
        array = np.linspace(0.0, 1.0, 64 * 64 * 3, dtype=np.float32)
        array = array.reshape(64, 64, 3)
        path = write_tiff(self.dir, "ramp2.tif", array)
        as_srgb = metrics.measure(path, "srgb", digest=False)["tone"]["key_stops"]
        as_linear = metrics.measure(path, "linear-srgb",
                                    digest=False)["tone"]["key_stops"]
        self.assertGreater(abs(as_linear - as_srgb), 1.0)

    def test_primaries_change_luminance(self):
        """Two spaces sharing a transfer still weight the channels differently."""
        array = np.zeros((8, 8, 3), dtype=np.float32)
        array[:, :, 1] = 0.5
        path = write_tiff(self.dir, "green.tif", array)
        srgb = metrics.measure(path, "linear-srgb", digest=False)["tone"]["key_stops"]
        p3 = metrics.measure(path, "linear-display-p3",
                             digest=False)["tone"]["key_stops"]
        self.assertNotAlmostEqual(srgb, p3, places=3)

    def test_measurement_is_deterministic(self):
        rng = np.random.default_rng(7)
        array = rng.random((48, 48, 3), dtype=np.float32)
        path = write_tiff(self.dir, "det.tif", array)
        first = metrics.measure(path, "linear-srgb", digest=False)
        second = metrics.measure(path, "linear-srgb", digest=False)
        self.assertEqual(json.dumps(first, sort_keys=True),
                         json.dumps(second, sort_keys=True))

    def test_extra_channels_are_dropped_and_recorded(self):
        array = np.full((8, 8, 4), 0.18, dtype=np.float32)
        record = metrics.measure(write_tiff(self.dir, "rgba.tif", array),
                                 "linear-srgb", digest=False)
        self.assertEqual(record["image"]["extra_channels_ignored"], 1)


class Color(unittest.TestCase):
    def setUp(self):
        if not HAVE_DEPS:
            self.skipTest("numpy/Pillow/tifffile not installed")
        self._tmp = tempfile.TemporaryDirectory(prefix="nctool-color-")
        self.dir = Path(self._tmp.name)
        self.addCleanup(self._tmp.cleanup)

    def _color(self, array, space="linear-srgb", name=None):
        path = write_tiff(self.dir, name or f"c{len(list(self.dir.iterdir()))}.tif",
                          array)
        return metrics.measure(path, space, digest=False)["color"]

    def test_rgb_neutral_measures_exactly_neutral_in_every_space(self):
        """The property the reference white is chosen for.

        The headline colour number is a *cast*, so an RGB-neutral frame has to
        read a* = b* = 0 exactly. Deriving the CIELAB white from this module's own
        D65 rather than the tabulated (0.95047, 1, 1.08883) is what guarantees it;
        the tabulated triple would give every image a small constant tint. Note
        ProPhoto is the interesting case — it is a D50 space, so this only holds
        because the Bradford adaptation runs first.
        """
        for space in ("linear-srgb", "linear-display-p3", "linear-adobe-rgb",
                      "linear-bt2020", "linear-prophoto"):
            color = self._color(np.full((40, 40, 3), 0.18, dtype=np.float32),
                                space, f"n-{space}.tif")
            self.assertAlmostEqual(color["mean_a"], 0.0, places=6, msg=space)
            self.assertAlmostEqual(color["mean_b"], 0.0, places=6, msg=space)
            self.assertEqual(color["neutral_fraction"], 1.0, msg=space)

    def test_lab_white_is_derived_not_the_tabulated_triple(self):
        """Pins the deliberate difference so nobody "corrects" it.

        `display-output-acceptance` pins the tabulated white for its own oracle,
        which compares absolute colorimetry across renditions; this one measures
        relative cast within an image and needs self-consistency instead.
        """
        white = metrics.lab_reference_white(metrics.SPACES["srgb"])
        self.assertAlmostEqual(white[1], 1.0, places=12)
        self.assertAlmostEqual(white[0], 0.950456, places=5)
        self.assertGreater(abs(white[2] - 1.08883), 1e-4)

    def test_a_warm_cast_reads_yellow_and_a_cool_one_blue(self):
        """b* is the blue-yellow axis; the sign has to point the right way."""
        warm = np.full((40, 40, 3), 0.18, dtype=np.float32)
        warm[..., 2] *= 0.5
        cool = np.full((40, 40, 3), 0.18, dtype=np.float32)
        cool[..., 2] *= 2.0
        self.assertGreater(self._color(warm, name="warm.tif")["mean_b"], 5.0)
        self.assertLess(self._color(cool, name="cool.tif")["mean_b"], -5.0)

    def test_channel_balance_reads_a_known_offset_in_stops(self):
        """Halving one channel is exactly one stop, by construction."""
        array = np.full((40, 40, 3), 0.18, dtype=np.float32)
        array[..., 2] *= 0.5
        balance = self._color(array, name="bal.tif")["balance_stops"]
        self.assertAlmostEqual(balance["b_over_g"], -1.0, places=5)
        self.assertAlmostEqual(balance["r_over_g"], 0.0, places=5)

    def test_a_crushed_channel_withholds_the_balance_ratios(self):
        """The review case: a channel at black over half the frame.

        Each channel's geometric mean is taken over the pixels where that channel
        is positive, so the three can rest on different supports. Blue crushed to
        black in half the frame used to report `b_over_g = 0.0` — a perfectly
        neutral balance for an image with a `mean_b` of +26 — with every counter
        reading clean. The ratios are now withheld and the supports published.
        """
        array = np.full((40, 40, 3), 0.18, dtype=np.float32)
        array[:20, :, 2] = 0.0
        color = self._color(array, name="crush.tif")
        self.assertNotIn("b_over_g", color["balance_stops"])
        self.assertNotIn("r_over_g", color["balance_stops"])
        self.assertAlmostEqual(color["balance_support"]["b"], 0.5, places=6)
        self.assertAlmostEqual(color["balance_support"]["g"], 1.0, places=6)
        self.assertGreater(color["mean_b"], 10.0, "the cast is real and large")

    def test_equal_supports_still_report_the_ratios(self):
        array = np.full((40, 40, 3), 0.18, dtype=np.float32)
        array[..., 2] *= 0.5
        color = self._color(array, name="ok.tif")
        self.assertAlmostEqual(color["balance_stops"]["b_over_g"], -1.0, places=5)
        self.assertEqual(set(color["balance_support"].values()), {1.0})

    def test_the_block_size_does_not_change_the_answer(self):
        """Colour streams in row blocks to keep memory flat; the seam must not show."""
        rng = np.random.default_rng(11)
        array = rng.random((300, 40, 3), dtype=np.float32)
        path = write_tiff(self.dir, "blocks.tif", array)
        whole = metrics.measure(path, "linear-srgb", digest=False)["color"]
        original = metrics.BLOCK_ROWS
        metrics.BLOCK_ROWS = 7
        try:
            split = metrics.measure(path, "linear-srgb", digest=False)["color"]
        finally:
            metrics.BLOCK_ROWS = original
        self.assertEqual(json.dumps(whole, sort_keys=True),
                         json.dumps(split, sort_keys=True))

    def test_a_uniform_frame_reports_its_one_hue_in_the_right_sector(self):
        """Ties three things together: the a*/b* means, the hue angle derived from
        them, and which sector the pixels were binned into.

        A uniform frame has exactly one hue, so the reported `mean_hue` must equal
        `atan2(mean_b, mean_a)` and must sit inside the sector reporting it. A
        binning or unit error (radians for degrees, an off-by-one sector) breaks
        one of the three and not the others.
        """
        for name, blue in (("warm2.tif", 0.5), ("cool2.tif", 2.0)):
            array = np.full((40, 40, 3), 0.18, dtype=np.float32)
            array[..., 2] *= blue
            color = self._color(array, name=name)
            expected = math.degrees(math.atan2(color["mean_b"],
                                               color["mean_a"])) % 360.0
            populated = {k: v for k, v in color["hue_sectors"].items()
                         if v["fraction"] > 0}
            self.assertEqual(len(populated), 1, f"{name}: {populated}")
            key, sector = next(iter(populated.items()))
            low, high = (int(part) for part in key.removeprefix("deg_").split("_"))
            self.assertAlmostEqual(sector["mean_hue"], expected, places=3)
            self.assertTrue(low <= expected < high,
                            f"{name}: hue {expected} reported in {key}")

    def test_cast_by_band_uses_the_same_edges_as_tone(self):
        """Or "the shadows are cooler than the highlights" is measured against a
        different definition of shadow than the tone table reports."""
        rng = np.random.default_rng(5)
        array = rng.random((64, 64, 3), dtype=np.float32)
        path = write_tiff(self.dir, "bands.tif", array)
        record = metrics.measure(path, "linear-srgb", digest=False)
        for name, band in record["color"]["cast_by_tone_band"].items():
            self.assertAlmostEqual(band["fraction"],
                                   record["tone"]["bands"][name], places=4,
                                   msg=name)

    def test_hue_sector_shares_and_neutral_fraction_partition(self):
        rng = np.random.default_rng(9)
        array = rng.random((64, 64, 3), dtype=np.float32)
        array[0, :, :] = np.nan
        color = self._color(array, name="part.tif")
        shares = sum(s["fraction"] for s in color["hue_sectors"].values())
        self.assertAlmostEqual(shares + color["neutral_fraction"],
                               color["measured_fraction"], places=3)
        self.assertLess(color["measured_fraction"], 1.0)

    def test_every_colour_fraction_shares_tone_s_denominator(self):
        """Colour used to divide band shares by its own measured count.

        On a frame with one NaN row that made colour report a band as 1.0 which
        tone reported as 0.95 — one name, two bases, which reads as a bug in one
        of the stages rather than as two definitions.
        """
        array = np.full((20, 20, 3), 0.18, dtype=np.float32)
        array[0, :, :] = np.nan
        path = write_tiff(self.dir, "denom.tif", array)
        record = metrics.measure(path, "linear-srgb", digest=False)
        self.assertAlmostEqual(record["tone"]["bands"]["mid"], 0.95, places=6)
        self.assertAlmostEqual(
            record["color"]["cast_by_tone_band"]["mid"]["fraction"], 0.95,
            places=6)

    def test_chroma_beyond_the_histogram_ceiling_is_clipped_not_dropped(self):
        """`np.histogram(range=...)` discards out-of-range values.

        A frame more saturated than the ceiling therefore emptied the histogram
        and the percentiles read 0.12 while the mean read 208 — a wrong answer
        with nothing to signal it. Samples are clipped into the top bin now, and
        `max_chroma` is exact so a saturated percentile is recognizable.
        """
        array = np.zeros((40, 40, 3), dtype=np.float32)
        array[..., 1] = 0.02
        array[..., 2] = 4.0
        color = self._color(array, name="vivid.tif")
        self.assertGreater(color["max_chroma"], metrics.CHROMA_CEILING)
        self.assertGreater(color["median_chroma"],
                           metrics.CHROMA_CEILING - metrics.CHROMA_STEP)
        self.assertAlmostEqual(color["median_chroma"], color["p90_chroma"],
                               places=6)

    def test_chroma_percentiles_track_a_known_split(self):
        """Half the frame near-neutral, half strongly coloured."""
        array = np.full((40, 40, 3), 0.18, dtype=np.float32)
        array[20:, :, 2] *= 0.5
        color = self._color(array, name="split.tif")
        self.assertLess(color["median_chroma"], color["p90_chroma"])
        self.assertAlmostEqual(color["neutral_fraction"], 0.5, places=3)

    def test_non_finite_and_non_positive_samples_are_excluded(self):
        array = np.full((20, 20, 3), 0.18, dtype=np.float32)
        array[0, :, :] = np.nan
        array[1, :, :] = 0.0
        color = self._color(array, name="skip.tif")
        self.assertEqual(color["measured"], 360)
        self.assertAlmostEqual(color["mean_a"], 0.0, places=6)

    def test_color_is_deterministic(self):
        rng = np.random.default_rng(13)
        array = rng.random((48, 48, 3), dtype=np.float32)
        path = write_tiff(self.dir, "det2.tif", array)
        a = metrics.measure(path, "linear-srgb", digest=False)["color"]
        b = metrics.measure(path, "linear-srgb", digest=False)["color"]
        self.assertEqual(json.dumps(a, sort_keys=True), json.dumps(b, sort_keys=True))


class Jpeg(unittest.TestCase):
    """JPEG input. Verified against the TIFF path, which has its own oracle in
    nc's `loss.*` counters — so the chain is JPEG -> TIFF -> nc's own report."""

    def setUp(self):
        if not HAVE_DEPS:
            self.skipTest("numpy/Pillow/tifffile not installed")
        self._tmp = tempfile.TemporaryDirectory(prefix="nctool-jpeg-")
        self.dir = Path(self._tmp.name)
        self.addCleanup(self._tmp.cleanup)

    def _pair(self, array):
        """The same 8-bit content written as both a TIFF and a JPEG."""
        from PIL import Image
        tiff = write_tiff(self.dir, "same.tif", array)
        jpeg = self.dir / "same.jpg"
        Image.fromarray(array).save(jpeg, quality=100, subsampling=0)
        return tiff, jpeg

    def test_a_jpeg_measures_like_a_tiff_of_the_same_content(self):
        rng = np.random.default_rng(4)
        array = (rng.random((64, 64, 3)) * 200 + 20).astype(np.uint8)
        tiff, jpeg = self._pair(array)
        a = metrics.measure(tiff, "srgb", digest=False)
        b = metrics.measure(jpeg, "srgb", digest=False)
        self.assertAlmostEqual(a["tone"]["key_stops"], b["tone"]["key_stops"],
                               places=1)
        self.assertAlmostEqual(a["color"]["mean_cast"], b["color"]["mean_cast"],
                               places=0)
        self.assertEqual(a["image"]["width"], b["image"]["width"])

    def test_the_container_is_sniffed_not_taken_from_the_extension(self):
        """Extensions lie. A JPEG named `.tif` must still be read as a JPEG —
        otherwise it gets the TIFF path's refusal and a misleading message."""
        from PIL import Image
        path = self.dir / "actually-a.tif"
        Image.fromarray(np.full((16, 16, 3), 120, dtype=np.uint8)).save(
            path, format="JPEG")
        record = metrics.measure(path, "srgb", digest=False)
        self.assertEqual(record["image"]["container"], "jpeg")

    def test_the_record_names_the_decoder_and_the_precision(self):
        """A lossy read's numbers are only reproducible against a named decoder,
        and 8-bit shadow percentiles are quantization-limited — both belong in
        the record rather than in a caveat someone has to remember."""
        _, jpeg = self._pair(np.full((16, 16, 3), 120, dtype=np.uint8))
        image = metrics.measure(jpeg, "srgb", digest=False)["image"]
        self.assertEqual(image["bits_per_sample"], 8)
        self.assertIn("Pillow", image["decoder"])
        self.assertIn("libjpeg", image["decoder"])
        self.assertEqual(image["jpeg_image"], "sdr")
        self.assertFalse(image["gain_map_present"])

    def test_a_tiff_record_carries_no_decoder_field(self):
        """There is no decoder ambiguity to record on that path."""
        tiff, _ = self._pair(np.full((16, 16, 3), 120, dtype=np.uint8))
        image = metrics.measure(tiff, "srgb", digest=False)["image"]
        self.assertNotIn("decoder", image)
        self.assertEqual(image["container"], "tiff")
        self.assertEqual(image["bits_per_sample"], 8)

    def _gain_map_file(self) -> Path:
        """A second appended image is exactly how a gain-map JPEG is built."""
        from PIL import Image
        base = self.dir / "base.jpg"
        Image.fromarray(np.full((16, 16, 3), 120, dtype=np.uint8)).save(base)
        path = self.dir / "gain.jpg"
        path.write_bytes(base.read_bytes() * 2)
        return path

    def test_a_gain_map_is_detected_and_the_base_is_what_gets_measured(self):
        record = metrics.measure(self._gain_map_file(), "display-p3", digest=False)
        self.assertTrue(record["image"]["gain_map_present"])
        self.assertEqual(record["image"]["jpeg_image"], "sdr")
        # The base only: the appended image must not change the dimensions.
        self.assertEqual((record["image"]["width"], record["image"]["height"]),
                         (16, 16))

    def test_an_exif_thumbnail_is_not_mistaken_for_a_gain_map(self):
        """An EXIF APP1 payload contains a whole embedded JPEG.

        Counting `FFD8FF` across the file therefore reported a gain map on every
        camera and Lightroom export — precisely the reference files this reader
        was added for. Byte stuffing does not save the byte scan: it applies to
        entropy-coded scan data, not to marker payloads.
        """
        from PIL import Image
        thumb = io.BytesIO()
        Image.fromarray(np.full((8, 8, 3), 100, dtype=np.uint8)).save(
            thumb, format="JPEG")
        base = io.BytesIO()
        Image.fromarray(np.full((32, 32, 3), 120, dtype=np.uint8)).save(
            base, format="JPEG")
        payload = b"Exif\x00\x00" + thumb.getvalue()
        data = bytearray(base.getvalue())
        data[2:2] = b"\xff\xe1" + (len(payload) + 2).to_bytes(2, "big") + payload
        path = self.dir / "with-thumb.jpg"
        path.write_bytes(bytes(data))

        self.assertFalse(metrics._gain_map_present(path))
        # ...and the more specific `hdr` diagnosis is restored for such a file.
        with self.assertRaises(metrics.MetricsError) as caught:
            metrics.measure(path, "srgb", digest=False, jpeg_image="hdr")
        self.assertIn("carries no gain map", str(caught.exception))
        # A genuinely appended second image is still found.
        self.assertTrue(metrics._gain_map_present(self._gain_map_file()))

    def test_hdr_is_refused_with_the_more_specific_diagnosis_first(self):
        """Two tiers. "This file has no gain map" beats "reconstruction is not
        implemented" when both could be said."""
        _, plain = self._pair(np.full((16, 16, 3), 120, dtype=np.uint8))
        with self.assertRaises(metrics.MetricsError) as caught:
            metrics.measure(plain, "srgb", digest=False, jpeg_image="hdr")
        self.assertIn("carries no gain map", str(caught.exception))
        self.assertNotIn("not implemented", str(caught.exception))

        with self.assertRaises(metrics.MetricsError) as caught:
            metrics.measure(self._gain_map_file(), "display-p3", digest=False,
                            jpeg_image="hdr")
        message = str(caught.exception)
        self.assertIn("not implemented", message)
        self.assertIn("hdr-linear-tiff", message)

    def test_an_unknown_jpeg_image_value_is_refused(self):
        _, jpeg = self._pair(np.full((16, 16, 3), 120, dtype=np.uint8))
        with self.assertRaises(metrics.MetricsError):
            metrics.measure(jpeg, "srgb", digest=False, jpeg_image="gain-map")

    def test_a_grayscale_jpeg_is_refused(self):
        from PIL import Image
        path = self.dir / "grey.jpg"
        Image.fromarray(np.full((16, 16), 120, dtype=np.uint8)).save(path)
        with self.assertRaises(metrics.MetricsError) as caught:
            metrics.measure(path, "srgb", digest=False)
        self.assertIn("not RGB", str(caught.exception))


class Regions(unittest.TestCase):
    def setUp(self):
        if not HAVE_DEPS:
            self.skipTest("numpy/Pillow/tifffile not installed")
        self._tmp = tempfile.TemporaryDirectory(prefix="nctool-region-")
        self.dir = Path(self._tmp.name)
        self.addCleanup(self._tmp.cleanup)

    def test_inset_resolves_to_whole_pixels(self):
        region = metrics.resolve_region(100, 100, metrics.inset_fraction(0.1))
        self.assertEqual((region["x"], region["y"], region["width"],
                          region["height"]), (10, 10, 80, 80))
        self.assertEqual(region["pixels"], 6400)

    def test_a_fractional_region_lands_on_the_same_content_at_two_sizes(self):
        """Why regions are fractions: the compared images differ in size."""
        small = metrics.resolve_region(100, 100, (0.25, 0.25, 0.5, 0.5))
        large = metrics.resolve_region(400, 400, (0.25, 0.25, 0.5, 0.5))
        self.assertEqual(small["x"] / 100, large["x"] / 400)
        self.assertEqual(small["width"] / 100, large["width"] / 400)

    def test_region_excludes_a_dark_border(self):
        """The film-holder case: a dark frame edge moves the statistics until the
        region excludes it."""
        array = np.full((100, 100, 3), 0.18, dtype=np.float32)
        array[:10, :, :] = 0.0001
        array[-10:, :, :] = 0.0001
        array[:, :10, :] = 0.0001
        array[:, -10:, :] = 0.0001
        path = write_tiff(self.dir, "bordered.tif", array)
        whole = metrics.measure(path, "linear-srgb", digest=False)
        inset = metrics.measure(path, "linear-srgb", metrics.inset_fraction(0.1),
                                digest=False)
        self.assertLess(whole["tone"]["key_stops"], -1.0)
        self.assertAlmostEqual(inset["tone"]["key_stops"], 0.0, places=5)
        self.assertAlmostEqual(inset["tone"]["bands"]["deep_shadow"], 0.0, places=6)

    def test_region_is_recorded_in_the_record(self):
        array = np.full((100, 100, 3), 0.18, dtype=np.float32)
        record = metrics.measure(write_tiff(self.dir, "r.tif", array), "linear-srgb",
                                 (0.1, 0.2, 0.5, 0.4), digest=False)
        self.assertEqual(record["region"]["x"], 10)
        self.assertEqual(record["region"]["y"], 20)
        self.assertEqual(record["region"]["width"], 50)
        self.assertEqual(record["region"]["height"], 40)
        self.assertEqual(record["region"]["fraction"]["width"], 0.5)

    def test_region_parsing_rejects_bad_input(self):
        # "nan,0,1,1" is the interesting one: NaN compares false against every
        # bound, so it passed validation and died later inside int(round(...)).
        for bad in ("1,2,3", "a,b,c,d", "0,0,0,1", "0.5,0,0.8,1",
                    "nan,0,1,1", "0,0,inf,1"):
            with self.assertRaises(metrics.MetricsError, msg=bad):
                metrics.parse_region(bad)

    def test_inset_bounds(self):
        with self.assertRaises(metrics.MetricsError):
            metrics.inset_fraction(0.5)
        with self.assertRaises(metrics.MetricsError):
            metrics.inset_fraction(-0.01)


class Refusals(unittest.TestCase):
    def setUp(self):
        if not HAVE_DEPS:
            self.skipTest("numpy/Pillow/tifffile not installed")
        self._tmp = tempfile.TemporaryDirectory(prefix="nctool-refuse-")
        self.dir = Path(self._tmp.name)
        self.addCleanup(self._tmp.cleanup)

    def _grey(self, name="g.tif"):
        return write_tiff(self.dir, name,
                          np.full((8, 8, 3), 0.18, dtype=np.float32))

    def test_unknown_space_lists_the_supported_ones(self):
        with self.assertRaises(metrics.MetricsError) as caught:
            metrics.measure(self._grey(), "srgb-ish", digest=False)
        message = str(caught.exception)
        self.assertIn("linear-srgb", message)
        self.assertIn("never inferred", message)

    def test_named_refusals_say_why(self):
        for name, expected in (("pq", "reference-white"),
                               ("hlg", "display-referred")):
            with self.assertRaises(metrics.MetricsError) as caught:
                metrics.measure(self._grey(), name, digest=False)
            self.assertIn(expected, str(caught.exception), name)

    def test_planar_layout_is_refused(self):
        """A planar RGB TIFF passes a naive `ndim == 3 and shape[2] >= 3` test.

        tifffile hands PLANARCONFIG=2 back as (samples, height, width), so a
        30x20 RGB file was accepted and measured as a 20x3 image with 27 "extra
        channels" — 27 of its 30 rows silently discarded, exit 0, plausible
        numbers. The file's own tags are checked now, not the array's shape.
        """
        path = self.dir / "planar.tif"
        tifffile.imwrite(str(path), np.full((3, 20, 30), 0.18, dtype=np.float32),
                         photometric="rgb", planarconfig="separate")
        with self.assertRaises(metrics.MetricsError) as caught:
            metrics.measure(path, "linear-srgb", digest=False)
        self.assertIn("planar", str(caught.exception))

    def test_an_unreadable_file_is_refused_cleanly_on_either_path(self):
        """Reader failures used to escape as library tracebacks.

        Both branches are covered because the container is sniffed from the magic
        bytes: a truncated JPEG takes the JPEG path, and anything else takes the
        TIFF path. Neither may raise a raw `TiffFileError` / `UnidentifiedImage`.
        """
        truncated = self.dir / "truncated.jpg"
        truncated.write_bytes(b"\xff\xd8\xff\xe0 not a whole jpeg")
        with self.assertRaises(metrics.MetricsError) as caught:
            metrics.measure(truncated, "linear-srgb", digest=False)
        self.assertIn("cannot be decoded as a JPEG", str(caught.exception))

        other = self.dir / "not-an-image.tif"
        other.write_bytes(b"# this is a markdown file, actually\n")
        with self.assertRaises(metrics.MetricsError) as caught:
            metrics.measure(other, "linear-srgb", digest=False)
        self.assertIn("cannot be read as a TIFF or JPEG", str(caught.exception))

    def test_multi_page_tiff_is_refused(self):
        path = self.dir / "multi.tif"
        page = np.full((8, 8, 3), 0.18, dtype=np.float32)
        with tifffile.TiffWriter(str(path)) as writer:
            writer.write(page, photometric="rgb")
            writer.write(page, photometric="rgb")
        with self.assertRaises(metrics.MetricsError) as caught:
            metrics.measure(path, "linear-srgb", digest=False)
        self.assertIn("2 TIFF pages", str(caught.exception))

    def test_grayscale_is_refused(self):
        path = write_tiff(self.dir, "grey1.tif",
                          np.full((8, 8), 0.18, dtype=np.float32))
        with self.assertRaises(metrics.MetricsError) as caught:
            metrics.measure(path, "linear-srgb", digest=False)
        self.assertIn("RGB", str(caught.exception))


class Command(unittest.TestCase):
    def setUp(self):
        if not HAVE_DEPS:
            self.skipTest("numpy/Pillow/tifffile not installed")
        self._tmp = tempfile.TemporaryDirectory(prefix="nctool-cmd-")
        self.dir = Path(self._tmp.name)
        self.addCleanup(self._tmp.cleanup)
        self.image = write_tiff(self.dir, "cmd.tif",
                                np.full((8, 8, 3), 0.18, dtype=np.float32))

    class Args:
        def __init__(self, **kwargs):
            self.image = None
            self.space = "linear-srgb"
            self.region = None
            self.inset = None
            self.out = None
            self.no_checksum = True
            self.__dict__.update(kwargs)

    def test_json_reaches_stdout_clean(self):
        buffer = io.StringIO()
        with redirect_stdout(buffer):
            code = metrics.cmd_image(self.Args(image=str(self.image)))
        self.assertEqual(code, 0)
        record = json.loads(buffer.getvalue())
        self.assertEqual(record["schema_version"], metrics.SCHEMA)
        self.assertEqual(record["file"], "cmd.tif")

    def test_region_and_inset_together_are_refused(self):
        code = metrics.cmd_image(self.Args(image=str(self.image),
                                           region="0,0,1,1", inset=0.1))
        self.assertEqual(code, 2)

    def test_an_empty_region_is_a_malformed_region_not_an_absent_one(self):
        """`--region "" --inset 0.1` used to succeed, silently measuring the inset.

        The mutual-exclusion check tested truthiness, so the empty string read as
        "no region given" and the run reported a region it was never asked for.
        """
        code = metrics.cmd_image(self.Args(image=str(self.image),
                                           region="", inset=0.1))
        self.assertEqual(code, 2)
        code = metrics.cmd_image(self.Args(image=str(self.image), region=""))
        self.assertEqual(code, 2)

    def test_missing_file_exits_two(self):
        code = metrics.cmd_image(self.Args(image=str(self.dir / "nope.tif")))
        self.assertEqual(code, 2)

    def test_checksum_identifies_the_measured_file(self):
        record = metrics.measure(self.image, "linear-srgb", digest=True)
        self.assertEqual(len(record["sha256"]), 64)


class Help(unittest.TestCase):
    """`--help` is what a user reads, so it must not drift from the table.

    A hand-written list of supported spaces in the parser went stale the moment
    Adobe RGB was added, with every other gate green. The parser now builds the
    list from `metrics.SPACES`; this asserts it stays that way.
    """

    def _image_parser_help(self) -> str:
        import argparse
        from nctool.__main__ import build_parser

        def sub(parser, name):
            for action in parser._actions:
                if isinstance(action, argparse._SubParsersAction):
                    return action.choices[name]
            raise AssertionError(f"no subparsers under {parser.prog}")

        return sub(sub(build_parser(), "metrics"), "image").format_help()

    def test_help_lists_every_supported_space(self):
        # Whitespace is collapsed first: argparse wraps long help text and will
        # split a name across lines at its hyphen ("linear-\n  prophoto"), which
        # defeats a naive substring check and made this test fail on a list that
        # was in fact complete.
        text = re.sub(r"\s+", "", self._image_parser_help())
        for name in metrics.SPACES:
            self.assertIn(name, text,
                          f"{name} is supported but absent from --help")


class SpaceFromRecipe(unittest.TestCase):
    """The one place this module derives a colour space instead of being told one.

    It is derived from the run's *frozen recipe* — recorded provenance — not from
    the pixels, which is a different thing from the guessing the command refuses
    to do. The table is verified against real conversions (see the comment beside
    `PRESET_SPACES`), and everything not on it is refused.
    """

    def test_verified_presets_resolve(self):
        for preset, expected in (("legacy", "srgb"),
                                 ("compatibility", "srgb"),
                                 ("display-p3", "display-p3"),
                                 ("film-master", "linear-acescg"),
                                 ("hdr-linear-tiff", "linear-bt2020")):
            space, why = metrics.space_for_recipe({"output": {"preset": preset}})
            self.assertEqual(space, expected, preset)
            self.assertIn(preset, why)

    def test_unreadable_containers_say_which_and_what_to_do(self):
        for preset, expected in (("hdr-pq", "AVIF"),
                                 ("hdr-hlg", "AVIF"),
                                 ("hdr-pq-tiff", "reference-white"),
                                 ("hdr-hlg-tiff", "HLG")):
            with self.assertRaises(metrics.MetricsError) as caught:
                metrics.space_for_recipe({"output": {"preset": preset}})
            self.assertIn(expected, str(caught.exception), preset)

    def test_the_gain_map_presets_resolve_to_their_sdr_base(self):
        """They were refused until JPEG reading landed. Now they resolve, and
        what gets measured is the base image — which the per-frame record marks
        with `gain_map_present` so the base is never mistaken for the rendition an
        HDR-aware viewer shows."""
        for preset in ("gain-map-hdr", "ultra-hdr-v1"):
            space, why = metrics.space_for_recipe({"output": {"preset": preset}})
            self.assertEqual(space, "display-p3", preset)
            self.assertIn(preset, why)

    def test_the_default_preset_resolves(self):
        """A recipe with no output section is nc's default, `gain-map-hdr`."""
        space, why = metrics.space_for_recipe({})
        self.assertEqual(space, "display-p3")
        self.assertIn("gain-map-hdr", why)

    def test_output_profile_overrides_the_non_atomic_presets(self):
        # `prophoto` resolves to the **pure 1.8** space, not the piecewise ROMM
        # one: `color::build_profile` synthesizes nc's ProPhoto output with a
        # plain 1.8 TRC and omits the ROMM toe, and decoding it with the toe
        # rewrites the deep-shadow statistics.
        for profile, expected in (("prophoto", "prophoto-gamma1.8"),
                                  ("acescg", "linear-acescg"),
                                  ("sRGB", "srgb"),
                                  ("display-p3", "display-p3")):
            space, why = metrics.space_for_recipe(
                {"output": {"preset": "legacy", "output_profile": profile}})
            self.assertEqual(space, expected, profile)
            self.assertIn("output_profile", why)

    def test_an_icc_path_and_an_f32_legacy_are_refused(self):
        """Two under-determined cases. A profile path has no primaries here, and
        whether an f32 legacy TIFF carries the profile's transfer or is linear was
        never established — and that choice decides every tone number."""
        for output in ({"preset": "custom", "output_profile": "/x/y.icc"},
                       {"preset": "legacy", "depth": "f32"}):
            with self.assertRaises(metrics.MetricsError):
                metrics.space_for_recipe({"output": output})


class Rollup(unittest.TestCase):
    def setUp(self):
        if not HAVE_DEPS:
            self.skipTest("numpy/Pillow/tifffile not installed")
        self._tmp = tempfile.TemporaryDirectory(prefix="nctool-rollup-")
        self.root = Path(self._tmp.name)
        self.addCleanup(self._tmp.cleanup)

    def write_run(self, frames, preset="display-p3", config="cfg"):
        """A minimal roll run: tags, a report, and real (tiny) output TIFFs."""
        base = self.root / "converted/nc" / config / "R"
        base.mkdir(parents=True)
        rows = []
        for name, scale, status in frames:
            output = base / f"{name}_positive.tiff"
            if scale is not None:
                array = np.full((16, 16, 3), 0.18 * scale, dtype=np.float32)
                array[..., 2] *= 0.8
                tifffile.imwrite(str(output), np.clip(array, 0, 1),
                                 photometric="rgb")
            rows.append({"input": f"/assets/rolls/R/{name}.tif",
                         "output": str(output), "status": status})
        (base / "roll-report.json").write_text(json.dumps(
            {"summary": {"total": len(rows)}, "frames": rows}))
        (base / "tags.json").write_text(json.dumps({
            "schema_version": 1, "kind": "nctool-roll-conversion", "roll": "R",
            "config": config,
            "report_file": f"converted/nc/{config}/R/roll-report.json",
            "recipe": {"output": {"preset": preset}},
            "identity": {"params_hash": config},
        }))
        return base

    def args(self, **kwargs):
        defaults = dict(asset_root=str(self.root), roll="R", run="cfg", space=None,
                        region=None, inset=None, out=None, markdown=None)
        defaults.update(kwargs)
        return type("Args", (), defaults)()

    def _run(self, **kwargs):
        buffer = io.StringIO()
        with redirect_stderr(buffer):
            code = metrics.cmd_roll(self.args(**kwargs))
        return code, buffer.getvalue()

    def test_measures_every_frame_and_rolls_up(self):
        base = self.write_run([("a", 1.0, "ok"), ("b", 2.0, "ok"),
                               ("c", 0.5, "ok")])
        # Declared linear on purpose. The fixture's frames are one stop apart in
        # *stored* values; under display-p3's sRGB transfer that is 1.8 stops of
        # linear light, so the clean expectation below only holds if the stored
        # values are the linear ones.
        code, _ = self._run(space="linear-srgb")
        self.assertEqual(code, 0)
        record = json.loads((base / "metrics.json").read_text())
        self.assertEqual(record["kind"], "nctool-roll-metrics")
        self.assertEqual([frame["frame"] for frame in record["frames"]],
                         ["a.tif", "b.tif", "c.tif"])
        self.assertEqual(record["space"]["declared"], "linear-srgb")

        # The frames are one stop apart by construction, so the key spread is
        # exactly two stops and the extremes are the frames that made them.
        key = record["spread"]["key_stops"]
        self.assertAlmostEqual(key["spread"], 2.0, places=4)
        self.assertEqual(key["min_frame"], "c.tif")
        self.assertEqual(key["max_frame"], "b.tif")
        self.assertEqual(key["frames"], 3)

    def test_a_failed_or_missing_frame_is_recorded_not_dropped(self):
        """A roll must not lose its other frames to one bad one, and the bad one
        must not vanish — it is the thing a reader needs to know about."""
        self.write_run([("a", 1.0, "ok"), ("b", None, "error"),
                        ("c", None, "ok")])
        code, _ = self._run()
        self.assertEqual(code, 1, "a skipped frame must not report success")
        record = json.loads(
            (self.root / "converted/nc/cfg/R/metrics.json").read_text())
        self.assertEqual([frame["frame"] for frame in record["frames"]], ["a.tif"])
        self.assertEqual({entry["frame"] for entry in record["skipped"]},
                         {"b.tif", "c.tif"})

    def test_no_measurable_frame_fails_loudly(self):
        self.write_run([("a", None, "error")])
        code, err = self._run()
        self.assertEqual(code, 1)
        self.assertIn("no frame", err)

    def test_the_space_comes_from_the_recipe_and_can_be_overridden(self):
        self.write_run([("a", 1.0, "ok")], preset="compatibility")
        self._run()
        record = json.loads(
            (self.root / "converted/nc/cfg/R/metrics.json").read_text())
        self.assertEqual(record["space"]["declared"], "srgb")
        self.assertIn("compatibility", record["space"]["source"])

        self._run(space="linear-srgb")
        record = json.loads(
            (self.root / "converted/nc/cfg/R/metrics.json").read_text())
        self.assertEqual(record["space"]["declared"], "linear-srgb")
        self.assertIn("command line", record["space"]["source"])

    def test_an_avif_preset_is_refused_before_any_measurement(self):
        """A container with no reader must fail before a single frame is read."""
        self.write_run([("a", 1.0, "ok")], preset="hdr-pq")
        code, err = self._run()
        self.assertEqual(code, 2)
        self.assertIn("AVIF", err)
        self.assertFalse((self.root / "converted/nc/cfg/R/metrics.json").exists())

    def test_an_unknown_space_fails_before_any_frame_is_decoded(self):
        """A typo used to decode the whole roll first.

        `measure()` raises the same error per frame, where the loop folds it into
        `skipped` — so the run ended with "no frame could be measured", naming
        neither the fault nor the remedy, and returned before writing the
        `skipped` list that held the reason. On a 30-frame roll of 75 MP scans
        that is minutes of decoding for an unusable message.
        """
        self.write_run([("a", 1.0, "ok")])
        for space, expected in (("srgb-ish", "unknown colour space"),
                                ("pq", "reference-white")):
            code, err = self._run(space=space)
            self.assertEqual(code, 2, space)
            self.assertIn(expected, err, space)
            self.assertFalse(
                (self.root / "converted/nc/cfg/R/metrics.json").exists())

    def test_the_region_reaches_every_frame(self):
        base = self.write_run([("a", 1.0, "ok")])
        self._run(inset=0.25)
        record = json.loads((base / "metrics.json").read_text())
        self.assertEqual(record["region_fraction"], [0.25, 0.25, 0.5, 0.5])
        self.assertEqual(record["frames"][0]["metrics"]["region"]["width"], 8)

    def test_the_artifact_is_deterministic(self):
        base = self.write_run([("a", 1.0, "ok"), ("b", 2.0, "ok")])
        self._run()
        first = (base / "metrics.json").read_text()
        self._run()
        self.assertEqual(first, (base / "metrics.json").read_text())

    def test_markdown_renders_from_the_stored_record(self):
        base = self.write_run([("a", 1.0, "ok"), ("b", 2.0, "ok")])
        self._run(markdown=str(self.root / "table.md"))
        written = (self.root / "table.md").read_text()
        record = json.loads((base / "metrics.json").read_text())
        # `metrics table` must reproduce it from the record alone, without pixels.
        buffer = io.StringIO()
        with redirect_stdout(buffer):
            code = metrics.cmd_table(
                type("A", (), dict(record=str(base / "metrics.json"), out=None))())
        self.assertEqual(code, 0)
        self.assertEqual(buffer.getvalue(), written)
        self.assertIn("| a.tif |", written)
        self.assertIn("Spread across the roll", written)
        self.assertIn(record["space"]["declared"], written)

    def test_report_formatting(self):
        """Four things a report needs that the JSON does not.

        The record keeps every axis as a raw fraction or value; the presentation
        layer is the only place that changes, which is why these are asserted on
        the rendered text rather than on the record.
        """
        # Fractions become percentages, with the unit named.
        self.assertEqual(metrics.format_axis("deep_shadow", 0.283833), "28.38")
        self.assertEqual(metrics.format_axis("neutral", 0.5), "50.00")
        # Stops and Lab units are not rescaled.
        self.assertEqual(metrics.format_axis("key_stops", -2.649938), "-2.65")
        self.assertEqual(metrics.format_axis("cast", 9.954216), "10.0")
        # Fixed point always: `1e-06` in a table cell is correct and unreadable.
        self.assertNotIn("e", metrics.format_axis("at_top_code", 0.000001))
        # ...but a value that rounds to zero and is not zero must not read as
        # zero: "nothing clipped" and "one pixel in a million" are different
        # findings.
        self.assertEqual(metrics.format_axis("at_top_code", 0.000001), "<0.01")
        self.assertEqual(metrics.format_axis("at_top_code", 0.0), "0.00")
        self.assertEqual(metrics.format_axis("crossover_b", -0.0001), ">-0.1")
        self.assertEqual(metrics.format_axis("key_stops", None), "-")

    def test_the_report_names_the_build_that_produced_the_images(self):
        """A committed report that does not say which build made the pixels is
        hard to trust later, and `git_dirty` is the part that matters most."""
        base = self.write_run([("a", 1.0, "ok")])
        self._run()
        record = json.loads((base / "metrics.json").read_text())
        record["identity"] = {"nc_version": "0.1.0", "git_commit": "abc123",
                              "git_dirty": True, "pipeline_version": 3,
                              "params_hash": "deadbeef", "target": "x86_64"}
        text = metrics.markdown_table(record)
        self.assertIn("abc123", text)
        self.assertIn("uncommitted changes", text)
        self.assertIn("deadbeef", text)

        record["identity"]["git_dirty"] = False
        self.assertNotIn("uncommitted changes", metrics.markdown_table(record))
        record["identity"] = {}
        self.assertIn("not recorded", metrics.markdown_table(record))

    def test_a_rendered_report_never_uses_scientific_notation(self):
        base = self.write_run([("a", 1.0, "ok"), ("b", 2.0, "ok")])
        self._run()
        record = json.loads((base / "metrics.json").read_text())
        record["spread"]["at_top_code"] = dict(
            min=0.0, median=5e-07, max=1e-06, spread=1e-06,
            min_frame="a.tif", max_frame="b.tif", frames=2)
        text = metrics.markdown_table(record)
        for row in text.splitlines():
            if row.startswith("| at_top_code"):
                self.assertNotIn("e-", row)
                break
        else:
            self.fail("the at_top_code row was not rendered")

    def test_skipped_frames_are_named_in_the_report(self):
        self.write_run([("a", 1.0, "ok"), ("b", None, "error")])
        self._run()
        record = json.loads(
            (self.root / "converted/nc/cfg/R/metrics.json").read_text())
        text = metrics.markdown_table(record)
        self.assertIn("Frames skipped: 1", text)
        self.assertIn("b.tif", text)

    def test_table_refuses_a_record_of_the_wrong_kind(self):
        path = self.root / "other.json"
        path.write_text(json.dumps({"kind": "nctool-roll-analysis"}))
        buffer = io.StringIO()
        with redirect_stderr(buffer):
            code = metrics.cmd_table(
                type("A", (), dict(record=str(path), out=None))())
        self.assertEqual(code, 2)

    def test_crossover_is_derived_from_the_band_casts(self):
        """Crossover is not a field of the per-image record — it is the difference
        between two of its bands, so the rollup derives it."""
        record = dict(color=dict(cast_by_tone_band=dict(
            shadow=dict(mean_a=1.0, mean_b=-8.0),
            mid=dict(mean_a=2.5, mean_b=4.0))))
        axes = metrics.frame_axes(record)
        self.assertAlmostEqual(axes["crossover_a"], 1.5, places=6)
        self.assertAlmostEqual(axes["crossover_b"], 12.0, places=6)

    def test_spread_needs_two_frames(self):
        self.assertEqual(metrics.spread([{"frame": "a", "axes": {"cast": 1.0}}]), {})
        pair = metrics.spread([{"frame": "a", "axes": {"cast": 1.0}},
                               {"frame": "b", "axes": {"cast": 3.0}}])
        self.assertEqual(pair["cast"]["median"], 2.0)


if __name__ == "__main__":
    unittest.main()
