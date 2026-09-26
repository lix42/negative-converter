"""Hermetic tests for manifest-driven roll conversion and analysis."""
from __future__ import annotations

import argparse
import contextlib
import hashlib
import io
import json
import os
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from nctool import roll  # noqa: E402


def _frame(path: str, role: str, width=100, height=80) -> dict:
    return {"file": path, "role": role, "width": width, "height": height,
            "sha256": hashlib.sha256(b"x").hexdigest(), "bytes": 1}


class TestRecipe(unittest.TestCase):
    def test_partial_recipe_recursively_overlays_defaults(self):
        merged = roll._deep_merge(
            {"print": {"print_exposure": 0, "black_point": 0}, "output": {"preset": "x"}},
            {"print": {"print_exposure": 1}})
        self.assertEqual(merged["print"], {"print_exposure": 1, "black_point": 0})
        self.assertEqual(merged["output"], {"preset": "x"})

    def test_switching_tagged_curve_replaces_incompatible_keys(self):
        merged = roll._deep_merge(
            {"curve": {"type": "exponential", "gamma": 2, "anchor": {"mid-at-base-offset": .62}}},
            {"curve": {"type": "characteristic", "stock": "ektar-100"}})
        self.assertEqual(merged["curve"], {"type": "characteristic", "stock": "ektar-100"})

    def test_a_retired_curve_tag_is_dropped_only_where_the_build_writes_none(self):
        # This build's defaults carry no curve tag, so an old recipe's
        # `"type": "exponential"` must not read as a variant switch that drops the
        # default curve's keys.
        partial = {"reconstruction": {"curve": {"type": "exponential", "gamma": 1.5}}}
        defaults = {"reconstruction": {"curve": {"gamma": 2.0,
                                                 "anchor": {"mid-at-base-offset": .62}}}}
        roll._drop_retired_curve_tag(defaults, partial)
        merged = roll._deep_merge(defaults, partial)
        self.assertEqual(merged["reconstruction"]["curve"],
                         {"gamma": 1.5, "anchor": {"mid-at-base-offset": .62}})
        # The reference build writes the tag, and there it is a selector: kept.
        partial = {"reconstruction": {"curve": {"type": "exponential", "gamma": 1.5}}}
        roll._drop_retired_curve_tag(
            {"reconstruction": {"curve": {"type": "sigmoid"}}}, partial)
        self.assertEqual(partial["reconstruction"]["curve"]["type"], "exponential")
        # A malformed section is left untouched for `_freeze_recipe` to refuse.
        for partial in ({"reconstruction": "invalid"}, {"reconstruction": {"curve": 1}}):
            roll._drop_retired_curve_tag(defaults, partial)
        _, error = roll._freeze_recipe({"reconstruction": "invalid"}, [.1, .2, .3],
                                       None, None, None)
        self.assertIn("must be an object", error)

    def test_measured_values_override_recipe_calibration(self):
        base = {
            "calibration": {"film_base": {"explicit": [9, 9, 9]}},
            "reconstruction": {"curve": {"type": "exponential", "gamma": 1.5}},
        }
        recipe, error = roll._freeze_recipe(base, [.1, .2, .3],
                                             "chromogenic", "display-p3", .5)
        self.assertIsNone(error)
        self.assertEqual(recipe["calibration"], {"film_base": {"explicit": [.1, .2, .3]}})
        self.assertEqual(recipe["reconstruction"]["curve"]["type"], "exponential")
        self.assertEqual(recipe["input"]["film_type"], "chromogenic")
        self.assertEqual(recipe["output"]["preset"], "display-p3")
        self.assertEqual(recipe["print"]["print_exposure"], .5)

    def test_the_reference_builds_retired_dmax_default_is_dropped(self):
        # The reference build's `hanten params` still writes `"dmax": "fixed"`; a
        # stated reference is kept for `hanten` to refuse by name.
        recipe, error = roll._freeze_recipe(
            {"calibration": {"film_base": None, "dmax": "fixed"}}, [.1, .2, .3],
            None, None, None)
        self.assertIsNone(error)
        self.assertNotIn("dmax", recipe["calibration"])
        recipe, error = roll._freeze_recipe(
            {"calibration": {"dmax": {"explicit": 1.4}}}, [.1, .2, .3], None, None, None)
        self.assertIsNone(error)
        self.assertEqual(recipe["calibration"]["dmax"], {"explicit": 1.4})

    def test_an_unstated_curve_is_left_to_the_build(self):
        # `hanten params` writes the build's own curve into the merged base; freezing
        # adds no tag this build no longer writes.
        recipe, error = roll._freeze_recipe({}, [.1, .2, .3], None, None, None)
        self.assertIsNone(error)
        self.assertNotIn("curve", recipe["reconstruction"])
        _, error = roll._freeze_recipe({"reconstruction": {"curve": 1}}, [.1, .2, .3],
                                       None, None, None)
        self.assertIn("must be an object", error)

    def test_default_region_is_center_eighty_percent(self):
        value, error = roll._region(None, {"width": 100, "height": 80}, "Dmin")
        self.assertIsNone(error)
        self.assertEqual(value, "10,8,80,64")

class TestConvert(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        frames = [
            _frame("rolls/R/u.tif", "unexposed"),
            _frame("rolls/R/l.tif", "leader"),
            _frame("rolls/R/a.tif", "real"),
            _frame("rolls/R/b.tif", "real"),
        ]
        for frame in frames:
            path = self.root / frame["file"]
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(b"x")
        (self.root / "manifest.json").write_text(json.dumps({
            "schema_version": 1, "generated": "today",
            "rolls": {"R": {"frames": frames}}, "converted": {}, "samples": [],
        }))

    def args(self, **updates):
        values = dict(asset_root=str(self.root), roll="R", nc="fake-nc", config="test",
                      out_dir=None, recipe=None, dmin_region=None, dmin_mode="grid",
                      film_type=None, output_preset="legacy", print_exposure=None,
                      max_memory="1GiB", strict_estimate=False, strict_roll=False)
        values.update(updates)
        return argparse.Namespace(**values)

    def fake_run(self, argv, **_kwargs):
        if argv[1] == "params":
            return mock.Mock(returncode=0, stdout=json.dumps({
                "reconstruction": {"schema_version": 1,
                                   "curve": {"type": "exponential", "gamma": 2.0,
                                             "anchor": {"mid-at-base-offset": .62}}},
                "calibration": {"film_base": None},
                "print": {"print_exposure": 0},
                "output": {"preset": "gain-map-hdr", "depth": "u16"},
            }), stderr="")
        if argv[1] == "estimate":
            return mock.Mock(returncode=0, stdout=json.dumps({
                "film_base": {"r": .1, "g": .2, "b": .3}}), stderr="")
        self.assertEqual(argv[1], "roll")
        report_path = Path(argv[argv.index("--report-file") + 1])
        inputs = argv[2:argv.index("--out-dir")]
        report = {
            "identity": {"nc_version": "x", "pipeline_version": 3,
                         "target": "test", "params_hash": "abc"},
            "recipe": json.loads(Path(argv[argv.index("--params") + 1]).read_text()),
            "frames": [
                {"input": path, "status": "ok", "output_stats": {"mean": [.1, .2, .3]},
                 "loss": {"total_samples": 3, "clipped_low": 0, "clipped_high": 0},
                 "identity": {"params_hash": "abc"}}
                for path in inputs
            ],
            "summary": {"total": 2, "succeeded": 2, "failed": 0},
        }
        report_path.write_text(json.dumps(report))
        return mock.Mock(returncode=0, stdout="", stderr="")

    def test_converts_manifest_real_frames_and_writes_provenance(self):
        seen = []

        def capture(argv, **kwargs):
            seen.append(argv)
            return self.fake_run(argv, **kwargs)

        out, err = io.StringIO(), io.StringIO()
        with mock.patch.object(roll.subprocess, "run", side_effect=capture), \
             contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            code = roll.cmd_convert(self.args())
        self.assertEqual(code, 0, err.getvalue())
        run = self.root / "converted/nc/test/R"
        tags = json.loads((run / "tags.json").read_text())
        recipe = json.loads((run / "recipe.json").read_text())
        calibration = json.loads((run / "calibration.json").read_text())
        self.assertEqual(tags["roll"], "R")
        self.assertEqual(tags["summary"]["succeeded"], 2)
        self.assertEqual(recipe["calibration"], {"film_base": {"explicit": [.1, .2, .3]}})
        # Only the unexposed frame is estimated; a manifest leader is not measured.
        self.assertEqual(len([a for a in seen if a[1] == "estimate"]), 1)
        self.assertEqual(calibration["dmin"]["region"], "10,8,80,64")
        self.assertEqual(calibration["dmin"]["mode"], "grid")
        self.assertNotIn("dmax", calibration)
        self.assertEqual(tags["calibration"], {"dmin": {
            "frame": "rolls/R/u.tif", "region": "10,8,80,64", "mode": "grid",
            "value": [.1, .2, .3]}})
        self.assertEqual(json.loads(out.getvalue())["config"], "test")

    def test_the_retired_density_tag_does_not_replace_the_default_reconstruction(self):
        recipe_path = self.root / "partial.json"
        recipe_path.write_text(json.dumps({"reconstruction": {
            "type": "density", "density": {"offset": [.1, 0, 0]}}}))
        with mock.patch.object(roll.subprocess, "run", side_effect=self.fake_run), \
             contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
            code = roll.cmd_convert(self.args(config="tagged", recipe=str(recipe_path)))
        self.assertEqual(code, 0)
        recipe = json.loads((self.root / "converted/nc/tagged/R/recipe.json").read_text())
        reconstruction = recipe["reconstruction"]
        self.assertNotIn("type", reconstruction)
        self.assertEqual(reconstruction["density"], {"offset": [.1, 0, 0]})
        self.assertEqual(reconstruction["curve"]["gamma"], 2.0)
        self.assertEqual(reconstruction["curve"]["anchor"], {"mid-at-base-offset": .62})

    def test_a_missing_leader_does_not_fail_the_run(self):
        (self.root / "rolls/R/l.tif").unlink()
        with mock.patch.object(roll.subprocess, "run", side_effect=self.fake_run), \
             contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()) as err:
            code = roll.cmd_convert(self.args(config="noleader"))
        self.assertEqual(code, 0, err.getvalue())
        tags = json.loads((self.root / "converted/nc/noleader/R/tags.json").read_text())
        self.assertNotIn("rolls/R/l.tif", [f["file"] for f in tags["source_frames"]])

    def test_region_mode_omits_grid_flag(self):
        seen = []

        def capture(argv, **kwargs):
            seen.append(argv)
            return self.fake_run(argv, **kwargs)

        with mock.patch.object(roll.subprocess, "run", side_effect=capture), \
             contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
            code = roll.cmd_convert(self.args(config="region", dmin_mode="region"))
        self.assertEqual(code, 0)
        dmin_argv = next(argv for argv in seen if argv[1] == "estimate")
        self.assertNotIn("--grid", dmin_argv)

    def test_refuses_nonempty_output_before_roll(self):
        run = self.root / "converted/nc/test/R"
        run.mkdir(parents=True)
        (run / "old.jpg").write_bytes(b"old")
        with mock.patch.object(roll.subprocess, "run", side_effect=self.fake_run) as run_mock:
            code = roll.cmd_convert(self.args())
        # Calibration precedes config hashing/output resolution, but the existing
        # directory is still refused before hanten roll can overwrite an artifact.
        self.assertEqual(code, 2)
        # `params` and the Dmin estimate.
        self.assertEqual(run_mock.call_count, 2)


class TestAnalyze(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)

    def write_run(self, config: str, mean: list[float], clipped: int = 0):
        base = self.root / "converted/nc" / config / "R"
        base.mkdir(parents=True)
        report = {
            "summary": {"total": 1, "succeeded": 1, "failed": 0},
            "frames": [{"input": "/assets/rolls/R/a.tif",
                        "output": "/outputs/a_positive.tiff", "status": "ok",
                        "film_base": {"r": .1, "g": .2, "b": .3}, "dmax": 1.3,
                        "output_stats": {"mean": mean},
                        "loss": {"total_samples": 100, "clipped_low": 0,
                                 "clipped_high": clipped},
                        "memory": {"detected_total_ram_bytes": 123},
                        "identity": {"params_hash": config}}],
        }
        (base / "roll-report.json").write_text(json.dumps(report))
        (base / "tags.json").write_text(json.dumps({
            "schema_version": 1, "kind": "nctool-roll-conversion", "roll": "R",
            "config": config, "report_file": f"converted/nc/{config}/R/roll-report.json",
            "source_frames": [{"file": "rolls/R/a.tif", "role": "real", "sha256": "x"}],
            "recipe": {"output": {"preset": "legacy"}},
            "calibration": {"dmax": {"value": 1.3}},
            "identity": {"params_hash": config},
        }))

    def test_writes_stable_analysis_beside_tags(self):
        self.write_run("a", [.1, .2, .3])
        args = argparse.Namespace(asset_root=str(self.root), roll="R", run="a", out=None)
        with contextlib.redirect_stderr(io.StringIO()):
            code = roll.cmd_analyze(args)
        self.assertEqual(code, 0)
        path = self.root / "converted/nc/a/R/analysis.json"
        first = path.read_bytes()
        result = json.loads(first)
        self.assertEqual(result["kind"], "nctool-roll-analysis")
        self.assertEqual(result["frames"][0]["source"], "rolls/R/a.tif")
        self.assertEqual(result["frames"][0]["output_stats"]["mean"], [.1, .2, .3])
        self.assertEqual(result["frames"][0]["clip_fraction"], 0.0)
        self.assertNotIn("input", result["frames"][0])
        self.assertNotIn("output", result["frames"][0])
        self.assertNotIn("memory", result["frames"][0])

        with contextlib.redirect_stderr(io.StringIO()):
            self.assertEqual(roll.cmd_analyze(args), 0)
        self.assertEqual(path.read_bytes(), first)

    def test_explicit_output_path_and_clipping_fraction(self):
        self.write_run("b", [.2, .2, .1], clipped=5)
        path = self.root / "result.json"
        args = argparse.Namespace(asset_root=str(self.root), roll="R", run="b",
                                  out=str(path))
        with contextlib.redirect_stderr(io.StringIO()):
            self.assertEqual(roll.cmd_analyze(args), 0)
        result = json.loads(path.read_text())
        self.assertEqual(result["frames"][0]["clip_fraction"], .05)


if __name__ == "__main__":
    unittest.main()
