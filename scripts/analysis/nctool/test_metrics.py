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
from contextlib import redirect_stdout
from pathlib import Path

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from nctool import metrics  # noqa: E402

try:
    import numpy as np
    import tifffile
    HAVE_DEPS = True
except ImportError:  # pragma: no cover - exercised by the guard test below
    HAVE_DEPS = False

REPO = Path(__file__).resolve().parents[3]
COLORIMETRY = REPO / "src" / "pipeline" / "colorimetry"

needs_deps = unittest.skipUnless(
    HAVE_DEPS, "numpy/tifffile not installed (scripts/analysis/requirements.txt)")


class DependencyGuard(unittest.TestCase):
    """`NCTOOL_REQUIRE_DEPS=1` turns the skip into a failure.

    Without this, a CI job that forgot to install the requirements would run the
    metrics tests as *skips* and still print `test result: ok` — the same trap
    CLAUDE.md records for a `cargo test` filter that matches nothing.
    """

    def test_dependencies_present_when_required(self):
        if os.environ.get("NCTOOL_REQUIRE_DEPS") == "1":
            self.assertTrue(HAVE_DEPS,
                            "NCTOOL_REQUIRE_DEPS=1 but numpy/tifffile are missing; "
                            "the metrics tests would have silently skipped")


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
                   "prophoto": "PROPHOTO"}
        for local, symbol in mapping.items():
            self.assertIn(symbol, rust, f"{symbol} disappeared from definitions.rs")
            self.assertEqual(metrics.PRIMARIES[local], rust[symbol][0],
                             f"{local} primaries drifted from definitions::{symbol}")

    def test_white_points_match_definitions_rs(self):
        self.assertEqual(metrics.WHITE["d65"], self._rust_white("D65"))
        self.assertEqual(metrics.WHITE["d50"], self._rust_white("D50"))

    def test_space_whites_match_definitions_rs(self):
        rust = self._rust_colorspaces()
        symbols = {"rec709": "REC709", "display-p3": "DISPLAY_P3",
                   "adobe-rgb": "ADOBE_RGB", "bt2020": "BT2020",
                   "prophoto": "PROPHOTO"}
        expected = {"D65": "d65", "D50": "d50"}
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
            self.skipTest("numpy/tifffile not installed")
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


class Regions(unittest.TestCase):
    def setUp(self):
        if not HAVE_DEPS:
            self.skipTest("numpy/tifffile not installed")
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
            self.skipTest("numpy/tifffile not installed")
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

    def test_a_file_that_is_not_a_tiff_is_refused_cleanly(self):
        """The likeliest user mistake: pointing this at a JPEG or PNG export.

        It used to escape as a `TiffFileError` traceback — the one input class
        with no handled path.
        """
        path = self.dir / "not-a.tif"
        path.write_bytes(b"\xff\xd8\xff\xe0 not a tiff at all")
        with self.assertRaises(metrics.MetricsError) as caught:
            metrics.measure(path, "linear-srgb", digest=False)
        self.assertIn("cannot be read as a TIFF", str(caught.exception))

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
            self.skipTest("numpy/tifffile not installed")
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


if __name__ == "__main__":
    unittest.main()
