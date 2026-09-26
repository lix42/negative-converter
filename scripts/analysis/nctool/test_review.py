"""Hermetic tests for the review-set generator.

Nothing here renders or measures: rendering needs the nc binary and the user's
own scans, and measuring needs the numpy venv. What is covered is everything that
decides *what* gets rendered and whether a stored measurement can be reused —
the parts a wrong answer in makes a whole page quietly wrong.
"""
from __future__ import annotations

import argparse
import contextlib
import io
import json
import os
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from nctool import metrics as _metrics  # noqa: E402
from nctool import review  # noqa: E402

MATRIX = {
    "schema_version": 1,
    "title": "Two presets",
    "output_preset": "gain-map-hdr",
    "common_args": ["--print-exposure", "0.5"],
    "metrics": {"inset": 0.1},
    "configs": [
        {"id": "generic", "label": "generic", "args": ["--density-gamma", "2.2"]},
        {"id": "stock", "label": "stock", "args": ["--film-base", "{dmin}"]},
    ],
}


def write(matrix: dict) -> Path:
    directory = Path(tempfile.mkdtemp(prefix="nc-review-matrix-"))
    path = directory / "matrix.json"
    path.write_text(json.dumps(matrix), encoding="utf-8")
    return path


def load(**overrides) -> dict:
    return review.load_matrix(write({**MATRIX, **overrides}))


class TestMatrix(unittest.TestCase):
    def test_reads_the_configs_in_order(self):
        matrix = load()
        self.assertEqual([c["id"] for c in matrix["configs"]], ["generic", "stock"])
        self.assertEqual(matrix["suffix"], "jpg")

    def test_refuses_an_unknown_output_preset(self):
        with self.assertRaisesRegex(review.ReviewError, "unknown output_preset"):
            load(output_preset="gain-map-hrd")

    def test_refuses_a_future_schema(self):
        with self.assertRaisesRegex(review.ReviewError, "schema_version"):
            load(schema_version=2)

    def test_refuses_two_configs_with_one_id(self):
        with self.assertRaisesRegex(review.ReviewError, "two entries with id"):
            load(configs=[{"id": "a", "args": []}, {"id": "a", "args": []}])

    def test_refuses_an_empty_matrix(self):
        with self.assertRaisesRegex(review.ReviewError, "at least one"):
            load(configs=[])

    # A typo in a placeholder is the failure this catches at the top rather than
    # 35 renders later — and `--film-stock {film_stok}` would otherwise reach nc
    # as a literal stock name.
    def test_refuses_an_unknown_placeholder(self):
        with self.assertRaisesRegex(review.ReviewError, r"unknown placeholder \{film_stok\}"):
            load(configs=[{"id": "a", "args": ["--film-stock", "{film_stok}"]}])

    def test_refuses_an_unterminated_placeholder(self):
        with self.assertRaisesRegex(review.ReviewError, "unterminated placeholder"):
            load(configs=[{"id": "a", "args": ["{dmin"]}])

    def test_refuses_an_inset_that_would_measure_nothing(self):
        with self.assertRaisesRegex(review.ReviewError, "metrics.inset"):
            load(metrics={"inset": 0.5})

    # Which configs need a per-frame value is **stated by the args**, never guessed
    # from the id.
    def test_only_the_configs_that_name_a_placeholder_need_its_value(self):
        configs = {c["id"]: c for c in load()["configs"]}
        self.assertEqual(configs["generic"]["needs"], set())
        self.assertEqual(configs["stock"]["needs"], {"dmin"})
        # A placeholder in `common_args` is every config's need.
        configs = {c["id"]: c for c in load(common_args=["--film-base", "{dmin}"])["configs"]}
        self.assertEqual(configs["generic"]["needs"], {"dmin"})

    # `film_stock` left with `--film-stock` (`nf-retire/characteristic`), and with it
    # the matrix's `rolls` block, which existed to state it.
    def test_refuses_the_retired_film_stock_placeholder_and_rolls_block(self):
        with self.assertRaisesRegex(review.ReviewError, "film_stock"):
            load(configs=[{"id": "a", "args": ["--film-stock", "{film_stock}"]}])
        with self.assertRaisesRegex(review.ReviewError, "unknown key rolls;"):
            load(rolls={"Ektar": {}})


class TestUnknownKeys(unittest.TestCase):
    """`deny_unknown_fields`, as every recipe struct in this repo has."""

    # `arg` for `args` loads as *no* arguments, so the cell renders the default
    # conversion under a label promising something else: five buttons, five
    # labels, identical pixels, exit 0.
    def test_refuses_a_mistyped_config_key(self):
        with self.assertRaisesRegex(review.ReviewError, "unknown key arg;"):
            load(configs=[{"id": "a", "arg": ["--density-gamma", "2.2"]}])

    # `insets` measures the whole frame — the film holder included, which is the
    # one thing the inset exists to keep out of the statistics.
    def test_refuses_a_mistyped_metrics_key(self):
        with self.assertRaisesRegex(review.ReviewError, "unknown key insets;"):
            load(metrics={"insets": 0.18})

    def test_refuses_a_mistyped_top_level_key(self):
        with self.assertRaisesRegex(review.ReviewError, "unknown key output_presets;"):
            load(output_presets="gain-map-hdr")


class TestCopiedStrings(unittest.TestCase):
    """Copied into `review.json`, where the app refuses a non-string."""

    def test_refuses_a_title_that_is_not_a_string(self):
        with self.assertRaisesRegex(review.ReviewError, "title must be a non-empty string"):
            load(title=2026)

    def test_refuses_a_note_that_is_not_a_string(self):
        with self.assertRaisesRegex(review.ReviewError, r"configs\[0\].note"):
            load(configs=[{"id": "a", "note": 123}])


class TestExpansion(unittest.TestCase):
    def test_substitutes_the_per_frame_values(self):
        self.assertEqual(
            review.expand_args(["--film-base", "{dmin}"], {"dmin": "0.5,0.2,0.1"}),
            ["--film-base", "0.5,0.2,0.1"])

    def test_leaves_an_argument_with_no_placeholder_alone(self):
        self.assertEqual(review.expand_args(["--density-gamma", "2.2"], {"dmin": "x"}),
                         ["--density-gamma", "2.2"])


class TestMetricsSpace(unittest.TestCase):
    def test_reads_the_gain_map_pair_as_its_sdr_base(self):
        space, _ = review.metrics_space("gain-map-hdr")
        self.assertEqual(space, "display-p3")

    # The preset name does not determine the space. `legacy` and `custom` accept
    # `--output-profile`, which a matrix is free to pass, and measuring ProPhoto
    # pixels as sRGB makes every tone and cast number wrong while every one of
    # them still looks reasonable.
    def test_a_cell_is_measured_in_the_space_its_own_recipe_resolved(self):
        space, _ = review.cell_space(
            {"output": {"preset": "legacy", "output_profile": "prophoto"}}, "legacy")
        self.assertEqual(space, "prophoto-gamma1.8")
        self.assertEqual(review.cell_space({"output": {"preset": "legacy"}}, "legacy")[0], "srgb")

    def test_a_cell_whose_space_is_under_determined_is_not_measured(self):
        space, why = review.cell_space({"output": {"preset": "custom", "depth": "f32"}}, "custom")
        self.assertIsNone(space)
        self.assertIn("f32", why)

    # `space_for_recipe` defaults an unstated preset to `gain-map-hdr`, so a
    # report that could not be read would resolve Display P3 for a `film-master`
    # render — linear ACEScg pixels measured as an encoded display space.
    def test_a_render_that_does_not_identify_itself_is_not_measured(self):
        for recipe in ({}, {"output": {}}, {"output": {"preset": "gain-map-hdr"}}):
            space, why = review.cell_space(recipe, "film-master")
            self.assertIsNone(space, recipe)
            self.assertIn("film-master", why)

    # A preset this toolkit cannot read is still perfectly reviewable by eye, so
    # the run produces a page without charts rather than refusing.
    def test_reports_why_an_unreadable_preset_has_no_metrics(self):
        space, why = review.metrics_space("hdr-pq")
        self.assertIsNone(space)
        self.assertIn("AVIF", why)

    def test_the_suffix_table_covers_every_preset_the_metrics_know(self):
        # The two tables are keyed by the same preset names; one gaining a preset
        # the other has never heard of is how a run renders `.tiff` from an AVIF
        # preset, or measures a container it cannot read.
        known = set(_metrics.PRESET_SPACES) | set(_metrics.PRESET_UNREADABLE)
        self.assertEqual(known - set(review.PRESET_SUFFIX), set())


class TestOwnedFlags(unittest.TestCase):
    """`nc` takes the last occurrence of a Set argument, so an override is silent."""

    def test_refuses_a_config_restating_the_output_preset(self):
        with self.assertRaisesRegex(review.ReviewError, "state it once as output_preset"):
            load(configs=[{"id": "a", "args": ["--output-preset", "film-master"]}])

    def test_refuses_the_flags_the_generator_supplies(self):
        # `--report-file` sends the report to a file *instead of* stdout, which is
        # where each cell's resolved recipe is read from — so a matrix passing it
        # would lose the measurement on every cell that rendered perfectly.
        for flag in ("-o", "--output", "--report", "--report-file"):
            with self.assertRaisesRegex(review.ReviewError, "set by the generator"):
                load(common_args=[flag, "x"])

    def test_sees_the_equals_form_too(self):
        with self.assertRaisesRegex(review.ReviewError, "set by the generator"):
            load(configs=[{"id": "a", "args": ["--output-preset=legacy"]}])


class TestUnsafeIds(unittest.TestCase):
    """Every cell writes `<frame>-<config>` into the output directory."""

    def test_refuses_a_config_id_that_would_escape_the_output_directory(self):
        for bad in ("../leak", "/tmp/leak", "a/b", ".."):
            with self.assertRaisesRegex(review.ReviewError, "filename-safe"):
                load(configs=[{"id": bad, "args": []}])

    def test_refuses_a_frame_id_that_would_escape(self):
        # Frame ids come from `--fixtures`, which is a flag: a custom fixture
        # file is as much an input as the matrix.
        fixtures = {"frames": {"../../negative-converter/leak": {}}}
        with self.assertRaisesRegex(review.ReviewError, "filename-safe"):
            review._frames_to_render(load(), fixtures, "../../negative-converter/leak")

    def test_accepts_the_ids_the_shipped_matrix_uses(self):
        self.assertEqual([c["id"] for c in load()["configs"]], ["generic", "stock"])


class TestCollisions(unittest.TestCase):
    """`<frame>-<config>` is not injective when either id may hold a hyphen."""

    def test_reports_two_cells_that_would_write_one_file(self):
        clashes = review.colliding_stems(["a-b", "a"], ["c", "b-c"])
        self.assertEqual(len(clashes), 1)
        self.assertIn("a-b-c", clashes[0])

    # The default macOS volume and Windows treat `A-c.jpg` and `a-c.jpg` as one
    # file, so an exact-string check passes while the second render replaces the
    # first — and this repo's primary machine is macOS.
    def test_reports_a_collision_that_only_a_case_insensitive_disk_sees(self):
        self.assertEqual(len(review.colliding_stems(["G2"], ["A", "a"])), 1)

    def test_says_nothing_about_the_shipped_shape(self):
        # Config ids routinely carry hyphens; frame ids do not, which is what
        # keeps the real matrix clear.
        self.assertEqual(review.colliding_stems(["G2", "E1"], ["chr-generic", "sig-flat"]), [])


class TestCastNote(unittest.TestCase):
    """The per-frame line the page shows beside the heading."""

    def note(self, mean):
        return review._cast_note({"output_stats": {"mean": mean}})

    def test_reports_the_ratios_against_red(self):
        self.assertEqual(self.note([1.0, 1.02, 0.94]), "G/R 1.020 B/R 0.940")

    # `> 0`, not "not zero": the unclamped-float presets can report a negative
    # mean, and dividing by it prints sign-flipped ratios rather than nothing.
    # The script this replaced had the guard; the first port lost it.
    def test_says_nothing_when_the_red_mean_is_not_positive(self):
        for mean in ([-0.2, 0.1, 0.1], [0.0, 1.0, 1.0]):
            self.assertIsNone(self.note(mean), mean)

    def test_says_nothing_about_a_report_it_cannot_read(self):
        for mean in (None, "x", [1, 2], ["a", "b", "c"]):
            self.assertIsNone(self.note(mean), mean)


class TestDimensions(unittest.TestCase):
    """`nc`'s report carries no image size, so the record is the only source."""

    def test_states_both_dimensions_or_neither(self):
        self.assertEqual(review._dimensions({"image": {"width": 5184, "height": 3600}}),
                         {"width": 5184, "height": 3600})
        for absent in ({}, {"image": {}}, {"image": {"width": 5184}},
                       {"image": {"width": 0, "height": 3600}}):
            self.assertEqual(review._dimensions(absent), {}, absent)


class TestOutputDirectory(unittest.TestCase):
    def test_refuses_to_render_into_the_repository(self):
        # The frames are the user's own photographs and are never committed.
        repo = Path(review.__file__).resolve().parents[3]
        for target in (repo, repo / "tools" / "review-app" / "public"):
            # A binary path that cannot exist, deliberately: the refusal must
            # come from *what was asked*, not depend on what happens to be built.
            # CI builds only `target/debug/hanten`, and an earlier version of this
            # check reported "build the binary first" there instead.
            args = argparse.Namespace(
                matrix=str(write(MATRIX)), fixtures="scripts/analysis/fixtures.json",
                frames=None, nc="/nonexistent/nc", build=None,
                asset_root="../nc-assets",
                out=str(target), no_metrics=True, force=False)
            err = io.StringIO()
            with contextlib.redirect_stderr(err):
                self.assertEqual(review.cmd_generate(args), 2)
            self.assertIn("inside the repository", err.getvalue())


class TestReuse(unittest.TestCase):
    RECORD = {"schema_version": _metrics.SCHEMA, "sha256": "abc",
              "space": {"declared": "display-p3"},
              "region": {"x": 10, "y": 10, "width": 80, "height": 60}}

    def reuse(self, record=None, digest="abc", region=None, space="display-p3", decoder=None):
        # `is None`, not truthiness: `{}` is a record — the one meaning "nothing
        # was stored" — and `or` would quietly substitute the good one.
        return review.is_measured(
            self.RECORD if record is None else record,
            digest,
            self.RECORD["region"] if region is None else region,
            space,
            decoder,
        )

    def test_reuses_a_record_describing_these_exact_bytes(self):
        self.assertTrue(self.reuse())

    def test_re_measures_when_the_render_changed(self):
        self.assertFalse(self.reuse(digest="def"))

    # The same file measured as sRGB and as Display P3 gives different tone and
    # cast numbers, and `nctool metrics image --space …` beside the same image is
    # a documented way to leave one of each lying about.
    def test_re_measures_when_the_record_was_read_in_another_space(self):
        self.assertFalse(self.reuse(space="srgb"))
        self.assertFalse(self.reuse(record={**self.RECORD, "space": {}}))

    # The region is part of the identity: the same file measured over a different
    # rectangle is a different measurement, and reusing one for the other would
    # silently chart the holder.
    # A JPEG's samples are whatever its decoder says they are, which is why the
    # record names it; reusing across an upgrade mixes two decoders in one set.
    def test_re_measures_when_the_jpeg_decoder_changed(self):
        record = {**self.RECORD, "image": {"decoder": "Pillow 11.0 / libjpeg 6.2"}}
        self.assertFalse(self.reuse(record=record, decoder="Pillow 12.3 / libjpeg 6.2"))
        self.assertTrue(self.reuse(record=record, decoder="Pillow 11.0 / libjpeg 6.2"))
        # A TIFF record names no decoder and is reused regardless.
        self.assertTrue(self.reuse(decoder="Pillow 12.3 / libjpeg 6.2"))

    def test_re_measures_when_the_region_moved(self):
        self.assertFalse(self.reuse(region={**self.RECORD["region"], "x": 0}))

    def test_re_measures_a_record_from_an_older_schema(self):
        self.assertFalse(
            self.reuse(record={**self.RECORD, "schema_version": _metrics.SCHEMA - 1}))

    def test_re_measures_when_there_is_no_record_at_all(self):
        self.assertFalse(self.reuse(record={}))


class TestReviewDocument(unittest.TestCase):
    IMAGES = [{"id": "E1", "label": "E1", "renditions": {"generic": {"src": "E1-generic.jpg"}}}]

    def test_emits_the_schema_the_app_parses(self):
        doc = review.build_review(load(), self.IMAGES)
        self.assertEqual(doc["schema_version"], review.REVIEW_SCHEMA)
        self.assertEqual([c["id"] for c in doc["configs"]], ["generic", "stock"])
        self.assertEqual(doc["title"], "Two presets")

    # `configs` order is the button order **and** the keyboard mapping in the
    # app, so it follows the matrix rather than whatever rendered first.
    def test_declares_every_config_even_when_none_of_its_cells_rendered(self):
        doc = review.build_review(load(), self.IMAGES)
        self.assertIn("stock", [c["id"] for c in doc["configs"]])
        self.assertNotIn("stock", doc["images"][0]["renditions"])

    def test_leaves_out_an_absent_description(self):
        self.assertNotIn("description", review.build_review(load(), self.IMAGES))


class TestFrameSelection(unittest.TestCase):
    FIXTURES = {"frames": {"E1": {}, "E2": {}, "G1": {}}}

    def test_renders_every_fixture_frame_when_neither_states_any(self):
        self.assertEqual(review._frames_to_render(load(), self.FIXTURES, None),
                         ["E1", "E2", "G1"])

    def test_the_matrix_can_name_a_subset(self):
        self.assertEqual(review._frames_to_render(load(frames=["G1"]), self.FIXTURES, None),
                         ["G1"])

    def test_the_command_line_wins_over_the_matrix(self):
        self.assertEqual(review._frames_to_render(load(frames=["G1"]), self.FIXTURES, "E1,E2"),
                         ["E1", "E2"])

    # Two entries for one frame render every cell twice and write two images with
    # the same id — which the app refuses, after the expensive part is done.
    def test_refuses_a_frame_named_twice(self):
        with self.assertRaisesRegex(review.ReviewError, "named more than once: E1"):
            review._frames_to_render(load(), self.FIXTURES, "E1,E1")
        with self.assertRaisesRegex(review.ReviewError, "named more than once: G1"):
            review._frames_to_render(load(frames=["G1", "G1"]), self.FIXTURES, None)

    def test_refuses_a_frame_the_fixtures_do_not_declare(self):
        with self.assertRaisesRegex(review.ReviewError, "no such frame"):
            review._frames_to_render(load(), self.FIXTURES, "E1,Z9")


if __name__ == "__main__":
    unittest.main()


BUILDS = [
    {"id": "before", "label": "shipped", "nc": "/bin/before"},
    {"id": "after", "label": "candidate", "nc": "/bin/after"},
]


class TestBuildDeclaration(unittest.TestCase):
    def test_reads_the_builds_in_order(self):
        matrix = load(builds=BUILDS)
        self.assertEqual([b["id"] for b in matrix["builds"]], ["before", "after"])
        self.assertEqual(matrix["builds"][0]["label"], "shipped")

    def test_a_build_falls_back_to_its_id_for_a_label(self):
        self.assertEqual(load(builds=[{"id": "b", "nc": "/x"}])["builds"][0]["label"], "b")

    def test_refuses_two_builds_with_one_id(self):
        with self.assertRaisesRegex(review.ReviewError, "two entries with id"):
            load(builds=[{"id": "b", "nc": "/x"}, {"id": "b", "nc": "/y"}])

    def test_refuses_an_empty_build_list(self):
        with self.assertRaisesRegex(review.ReviewError, "at least one build"):
            load(builds=[])

    # The path is the whole point of declaring a build; a build without one names
    # nothing, and defaulting it to `--nc` would make both arms the same binary.
    def test_refuses_a_build_that_names_no_binary(self):
        with self.assertRaisesRegex(review.ReviewError, r"builds\[0\].nc"):
            load(builds=[{"id": "b"}])

    def test_refuses_a_build_id_that_would_escape_the_output_directory(self):
        with self.assertRaisesRegex(review.ReviewError, "not filename-safe"):
            load(builds=[{"id": "../b", "nc": "/x"}])

    def test_refuses_a_mistyped_build_key(self):
        with self.assertRaisesRegex(review.ReviewError, "unknown key binary"):
            load(builds=[{"id": "b", "binary": "/x"}])

    def test_a_build_may_expect_a_commit(self):
        build = load(builds=[{"id": "b", "nc": "/x", "expect_commit": "0DA32D0"}])
        self.assertEqual(build["builds"][0]["expect_commit"], "0da32d0")
        self.assertIsNone(load(builds=BUILDS)["builds"][0]["expect_commit"])

    def test_refuses_an_expected_commit_that_is_not_one(self):
        for value in ("0da32d", "main", "0da32d0-dirty", 7):
            with self.subTest(value=value), self.assertRaisesRegex(
                    review.ReviewError, r"builds\[0\].expect_commit"):
                load(builds=[{"id": "b", "nc": "/x", "expect_commit": value}])

    def test_a_matrix_without_builds_declares_none(self):
        self.assertEqual(load()["builds"], [])


class TestConfigBuildSubset(unittest.TestCase):
    def one(self, builds):
        return load(builds=BUILDS,
                    configs=[{"id": "a", "args": [], "builds": builds}])

    def test_a_config_may_name_a_subset(self):
        self.assertEqual([c["id"] for c in self.one(["before"])["configs"]],
                         ["a@before"])

    def test_refuses_a_build_the_matrix_does_not_declare(self):
        with self.assertRaisesRegex(review.ReviewError, "no such build: durring"):
            self.one(["durring"])

    def test_refuses_an_empty_subset(self):
        with self.assertRaisesRegex(review.ReviewError, "at least one build"):
            self.one([])

    def test_refuses_a_build_named_twice(self):
        with self.assertRaisesRegex(review.ReviewError, "more than once"):
            self.one(["before", "before"])

    # Naming a build in a matrix that declares none is a half-finished edit: the
    # config would otherwise render once, under a name nothing defines.
    def test_refuses_a_subset_when_the_matrix_declares_no_builds(self):
        with self.assertRaisesRegex(review.ReviewError, "declares no `builds`"):
            load(configs=[{"id": "a", "args": [], "builds": ["before"]}])


class TestBuildAxis(unittest.TestCase):
    """The expansion of builds x configs into the flat list the app renders."""

    def test_a_matrix_with_no_builds_passes_through_untouched(self):
        configs = load()["configs"]
        self.assertEqual([c["id"] for c in configs], ["generic", "stock"])
        self.assertEqual([c["label"] for c in configs], ["generic", "stock"])
        self.assertEqual([c["build"] for c in configs], [None, None])

    # Builds nest *inside* configs so the two arms of a before/after are adjacent:
    # `h`/`l` in the app then steps between the cells being compared, which is the
    # gesture the comparison is made of.
    def test_builds_nest_inside_configs(self):
        self.assertEqual(
            [c["id"] for c in load(builds=BUILDS)["configs"]],
            ["generic@before", "generic@after", "stock@before", "stock@after"])

    def test_composes_the_label_from_both_names(self):
        self.assertEqual(load(builds=BUILDS)["configs"][0]["label"],
                         "generic · shipped")

    def test_carries_the_build_each_cell_renders_through(self):
        cell = load(builds=BUILDS)["configs"][1]
        self.assertEqual(cell["build"]["id"], "after")
        self.assertEqual(cell["build"]["nc"], "/bin/after")

    def test_the_args_come_from_the_config_not_the_build(self):
        for cell in load(builds=BUILDS)["configs"][:2]:
            self.assertEqual(cell["args"], ["--density-gamma", "2.2"])

    # The join is injective because `@` is outside `SAFE_ID`: an author's id can
    # never contain one, so a composed id splits exactly one way. That is what
    # keeps `<config>@<build>` from reproducing the `<frame>-<config>` ambiguity
    # `colliding_stems` exists to refuse.
    def test_an_author_id_can_never_contain_the_join(self):
        self.assertNotRegex(review.BUILD_JOIN, review.SAFE_ID.pattern)
        with self.assertRaisesRegex(review.ReviewError, "not filename-safe"):
            load(configs=[{"id": "a@before", "args": []}])

    # The join removes the config/build ambiguity but not the frame/config one, so
    # the check still has work to do on the composed ids.
    def test_the_collision_check_sees_the_composed_ids(self):
        self.assertEqual(
            review.colliding_stems(["F1"], [c["id"] for c in load(builds=BUILDS)["configs"]]),
            [])
        self.assertEqual(
            review.colliding_stems(["F1", "F1-generic"], ["generic-x@before", "x@before"]),
            ["F1/generic-x@before and F1-generic/x@before would both write "
             "F1-generic-x@before"])


class TestBuildOverrides(unittest.TestCase):
    def test_reads_an_id_and_a_path(self):
        self.assertEqual(review.parse_build_overrides(["after=/bin/x"]),
                         {"after": "/bin/x"})

    def test_no_overrides_at_all(self):
        self.assertEqual(review.parse_build_overrides(None), {})

    def test_refuses_a_form_with_no_path(self):
        for bad in ("after", "after=", "=/bin/x"):
            with self.assertRaisesRegex(review.ReviewError, "must be <build id>="):
                review.parse_build_overrides([bad])

    def test_refuses_one_id_named_twice(self):
        with self.assertRaisesRegex(review.ReviewError, "twice"):
            review.parse_build_overrides(["a=/x", "a=/y"])


class TestResolveBinaries(unittest.TestCase):
    """The pre-flight: every binary is checked before a single cell renders."""

    def fake(self, banner: str = "hanten 0.1.0", name: str = "fake") -> str:
        directory = Path(tempfile.mkdtemp(prefix="nc-review-bin-"))
        path = directory / name
        path.write_text(f"#!/bin/sh\necho '{banner}'\n", encoding="utf-8")
        path.chmod(0o755)
        return str(path)

    def test_resolves_one_unnamed_build_when_the_matrix_declares_none(self):
        binary = self.fake()
        resolved = review.resolve_binaries([], {}, binary)
        self.assertEqual(len(resolved), 1)
        self.assertIsNone(resolved[0]["id"])
        self.assertEqual(resolved[0]["nc"], str(Path(binary).resolve()))

    def test_falls_back_to_the_default_binary(self):
        with self.assertRaisesRegex(review.ReviewError, review.DEFAULT_NC):
            review.resolve_binaries([], {}, None)

    # The reference build this axis exists to compare against predates the rename
    # and prints `nc`. A pre-flight
    # that demanded `hanten` would reject exactly that binary.
    def test_accepts_the_pre_rename_banner(self):
        binary = self.fake(banner="nc 0.1.0")
        self.assertEqual(len(review.resolve_binaries(
            [{"id": "old", "label": "old", "note": None, "nc": binary}], {}, None)), 1)

    def test_refuses_something_that_is_not_this_projects_cli(self):
        binary = self.fake(banner="netcat")
        with self.assertRaisesRegex(review.ReviewError, "not this project's CLI"):
            review.resolve_binaries(
                [{"id": "old", "label": "old", "note": None, "nc": binary}], {}, None)

    def test_names_the_build_whose_binary_is_missing(self):
        with self.assertRaisesRegex(review.ReviewError, "build 'after'"):
            review.resolve_binaries(
                [{"id": "after", "label": "a", "note": None, "nc": "/nonexistent/x"}],
                {}, None)

    def test_an_override_repoints_one_build(self):
        binary, other = self.fake(name="a"), self.fake(name="b")
        resolved = review.resolve_binaries(
            [{"id": "after", "label": "a", "note": None, "nc": binary}],
            {"after": other}, None)
        self.assertEqual(resolved[0]["nc"], str(Path(other).resolve()))

    def test_records_each_binarys_digest(self):
        binary = self.fake()
        resolved = review.resolve_binaries([], {}, binary)
        self.assertRegex(resolved[0]["digest"], r"^[0-9a-f]{64}$")


class TestIndexedSources(unittest.TestCase):
    """Where an earlier set points, read back off a document this run did not write.

    That is the whole difficulty: every other consumer here reads `build_review`'s
    own output, so a compare written against *that* shape silently ignores half the
    schema. The cost of a miss is one-sided — it lands on the branch that spares the
    stale set and tells the user it still describes its own pixels.
    """

    def setUp(self):
        self.base = Path(tempfile.mkdtemp(prefix="nc-review-index-")).resolve()

    def index(self, doc) -> tuple[set[str], bool]:
        return review.indexed_sources(doc, self.base)

    def doc(self, renditions) -> dict:
        return {"images": [{"id": "F1", "renditions": renditions}]}

    def key(self, name: str) -> str:
        return review.dest_key((self.base / name).resolve())

    def test_reads_the_object_shape(self):
        found, complete = self.index(self.doc({"a": {"src": "F1-a.jpg"}}))
        self.assertEqual(found, {self.key("F1-a.jpg")})
        self.assertTrue(complete)

    # SCHEMA.md calls the bare string the common case. Reading only the object form
    # made the headline spelling invisible to the whole check.
    def test_reads_the_string_shorthand(self):
        found, complete = self.index(self.doc({"a": "F1-a.jpg"}))
        self.assertEqual(found, {self.key("F1-a.jpg")})
        self.assertTrue(complete)

    # `src` is relative to `review.json`, not a bare filename, and a merged set
    # legitimately points at a sibling folder.
    def test_a_path_qualified_src_resolves_to_the_same_destination(self):
        plain = self.index(self.doc({"a": "F1-a.jpg"}))[0]
        for spelling in ("./F1-a.jpg", "renders/../F1-a.jpg",
                         str(self.base / "F1-a.jpg")):
            with self.subTest(spelling=spelling):
                self.assertEqual(self.index(self.doc({"a": spelling}))[0], plain)

    def test_a_sibling_folder_is_not_the_same_destination(self):
        self.assertNotEqual(self.index(self.doc({"a": "renders/F1-a.jpg"}))[0],
                            self.index(self.doc({"a": "F1-a.jpg"}))[0])

    # `colliding_stems` already casefolds and says why; this is the second place two
    # spellings denote one file, and it reaches the plain rebuild loop rather than a
    # hand-authored set.
    def test_two_spellings_of_one_file_are_one_destination(self):
        self.assertEqual(self.index(self.doc({"a": "F1-Dflt.jpg"}))[0],
                         self.index(self.doc({"a": "F1-dflt.jpg"}))[0])

    def test_an_empty_but_well_formed_set_is_read_completely(self):
        self.assertEqual(self.index({"images": []}), (set(), True))
        self.assertEqual(self.index(self.doc({})), (set(), True))

    # Everything below says "I recognised nothing", which is not "there was
    # nothing" — the caller may not vouch for the set on any of them.
    def test_a_file_that_could_not_be_parsed_is_not_read_completely(self):
        self.assertEqual(self.index({}), (set(), False))

    def test_a_shape_the_schema_does_not_have_is_not_read_completely(self):
        self.assertFalse(self.index({"images": "F1"})[1])
        self.assertFalse(self.index({"images": [{"id": "F1", "renditions": []}]})[1])
        self.assertFalse(self.index(self.doc({"a": 7}))[1])
        self.assertFalse(self.index(self.doc({"a": {"metrics": "x.json"}}))[1])

    def test_one_unreadable_rendition_does_not_hide_the_readable_ones(self):
        found, complete = self.index(self.doc({"a": "F1-a.jpg", "b": 7}))
        self.assertEqual(found, {self.key("F1-a.jpg")})
        self.assertFalse(complete)


class TestNameSome(unittest.TestCase):
    def test_states_them_all_while_there_are_few(self):
        self.assertEqual(review.name_some(["a", "b"]), "a, b")

    # Five configs over fourteen frames is seventy destinations on one error line.
    def test_caps_a_long_list_and_says_how_many_it_left_out(self):
        self.assertEqual(review.name_some([str(n) for n in range(8)]),
                         "0, 1, 2, 3, 4, and 3 more")


class TestBuildArguments(unittest.TestCase):
    """The pure-argument half: faults in what was asked, before any filesystem.

    Exercised end to end as well (`TestBuildAxisEndToEnd`), because these rules
    only work if they run before the coarser gates — and a test calling them
    directly never sees the ordering.
    """

    BUILDS = [{"id": "after", "label": "a", "note": None, "nc": "/x"}]

    def test_refuses_an_override_for_a_build_that_does_not_exist(self):
        with self.assertRaisesRegex(review.ReviewError, "no such build: durring"):
            review.check_build_arguments(self.BUILDS, {"durring": "/x"}, None)

    def test_says_so_when_the_matrix_declares_no_builds_at_all(self):
        with self.assertRaisesRegex(review.ReviewError, "declares no `builds`"):
            review.check_build_arguments([], {"durring": "/x"}, None)

    # `--nc` names one binary for the whole run, which is exactly what a build axis
    # replaces; accepting both would leave it ambiguous which one a cell used.
    def test_refuses_nc_beside_a_declared_build_axis(self):
        with self.assertRaisesRegex(review.ReviewError, "--build <id>=<path>"):
            review.check_build_arguments(self.BUILDS, {}, "/some/hanten")

    # The contradiction is the more specific fault, so it must not be pre-empted by
    # an override that happens to be wrong on the same line.
    def test_the_nc_contradiction_is_diagnosed_before_a_bad_override(self):
        with self.assertRaisesRegex(review.ReviewError, "--build <id>=<path>"):
            review.check_build_arguments(self.BUILDS, {"durring": "/x"}, "/some/hanten")

    def test_says_nothing_about_a_command_line_that_asked_for_nothing(self):
        self.assertIsNone(review.check_build_arguments([], {}, "/some/hanten"))
        self.assertIsNone(review.check_build_arguments(self.BUILDS, {"after": "/y"}, None))


class TestIdentity(unittest.TestCase):
    IDENTITY = {"nc_version": "0.1.0", "git_commit": "abc123", "git_dirty": False,
                "pipeline_version": 5, "target": "aarch64-apple-darwin",
                "params_hash": "deadbeef"}

    # `params_hash` identifies the recipe, which differs from config to config by
    # design — keeping it would make every second cell of a build look like a
    # different binary.
    def test_the_recipe_hash_is_not_part_of_the_build(self):
        self.assertNotIn("params_hash", review.build_identity(self.IDENTITY))
        self.assertEqual(review.build_identity(self.IDENTITY)["git_commit"], "abc123")

    def test_a_report_that_identifies_nothing(self):
        self.assertIsNone(review.build_identity(None))
        self.assertIsNone(review.build_identity({}))
        self.assertIsNone(review.build_identity("2664a0d"))

    def test_reads_the_report_first(self):
        directory = Path(tempfile.mkdtemp(prefix="nc-review-cell-"))
        dest = directory / "F1-a.tiff"
        dest.with_name(dest.name + ".json").write_text(
            json.dumps({"meta": {"git_commit": "sidecar"}}), encoding="utf-8")
        self.assertEqual(
            review.cell_identity({"identity": {"git_commit": "report"}}, dest),
            {"git_commit": "report"})

    # The two are the same value by construction, and no binary is known to write
    # one without the other — the fallback is kept because it costs a line, not
    # because a version needing it was established (`cell_identity`).
    def test_falls_back_to_the_sidecar(self):
        directory = Path(tempfile.mkdtemp(prefix="nc-review-cell-"))
        dest = directory / "F1-a.tiff"
        dest.with_name(dest.name + ".json").write_text(
            json.dumps({"meta": {"git_commit": "sidecar"}, "params": {}}),
            encoding="utf-8")
        self.assertEqual(review.cell_identity({}, dest), {"git_commit": "sidecar"})

    def test_a_cell_that_identifies_itself_nowhere(self):
        directory = Path(tempfile.mkdtemp(prefix="nc-review-cell-"))
        self.assertIsNone(review.cell_identity({}, directory / "F1-a.tiff"))


class TestRecordIdentity(unittest.TestCase):
    BUILD = {"id": "after", "label": "a", "nc": "/bin/after"}

    def record(self, identities, identity):
        directory = Path(tempfile.mkdtemp(prefix="nc-review-cell-"))
        return review.record_identity(identities, self.BUILD, "F1/a",
                                      {"identity": identity},
                                      directory / "F1-a.tiff")

    def test_the_first_cell_establishes_the_build(self):
        identities: dict = {}
        self.assertIsNone(self.record(identities, {"git_commit": "aaa"}))
        self.assertEqual(identities["after"], {"git_commit": "aaa"})

    def test_a_later_cell_that_agrees_says_nothing(self):
        identities = {"after": {"git_commit": "aaa"}}
        self.assertIsNone(self.record(identities, {"git_commit": "aaa"}))

    # A run fault, not a cell fault: the binary changed under the run, so every
    # cell already rendered under that name is suspect too.
    def test_a_later_cell_that_disagrees_is_reported(self):
        identities = {"after": {"git_commit": "aaa"}}
        message = self.record(identities, {"git_commit": "bbb"})
        self.assertIn("'after'", message)
        self.assertIn("F1/a", message)
        self.assertIn("aaa", message)
        self.assertIn("bbb", message)

    def test_a_cell_that_identifies_itself_nowhere_is_not_drift(self):
        identities = {"after": {"git_commit": "aaa"}}
        self.assertIsNone(self.record(identities, None))


class TestExpectedCommit(unittest.TestCase):
    CLEAN = {"git_commit": "0da32d063211", "git_dirty": False}

    # nc prints 12 digits; a matrix may state the full hash or a short one.
    def test_either_side_may_be_the_shorter(self):
        for expected in ("0da32d0", "0da32d063211",
                         "0da32d0632111196cfd8feb6f70d7e08b8b22a51"):
            with self.subTest(expected=expected):
                self.assertIsNone(review.commit_mismatch(expected, self.CLEAN))

    def test_another_commit(self):
        self.assertIn("9a61136bfe6e", review.commit_mismatch(
            "0da32d0", {"git_commit": "9a61136bfe6e", "git_dirty": False}))

    # A dirty tree's commit does not identify its source, and "unknown" is not clean.
    def test_a_dirty_or_unknown_tree_is_not_the_commit(self):
        self.assertIn("dirty", review.commit_mismatch(
            "0da32d0", {**self.CLEAN, "git_dirty": True}))
        self.assertIn("possibly dirty", review.commit_mismatch(
            "0da32d0", {"git_commit": "0da32d063211"}))

    def test_reads_the_version_banner(self):
        head = "hanten 0.1.0\npipeline_version: 5 (x)\n"
        self.assertEqual(review.banner_identity(head + "commit: 0da32d063211\ntarget: t"),
                         {"git_commit": "0da32d063211", "git_dirty": False})
        self.assertEqual(review.banner_identity(head + "commit: 0da32d063211-dirty"),
                         {"git_commit": "0da32d063211", "git_dirty": True})
        self.assertEqual(
            review.banner_identity(head + "commit: 0da32d063211 (dirty unknown)"),
            {"git_commit": "0da32d063211", "git_dirty": None})
        self.assertIsNone(review.banner_identity(head + "commit: unknown"))
        self.assertIsNone(review.banner_identity("hanten 0.1.0"))

    def test_a_cell_that_identifies_itself_nowhere_cannot_meet_it(self):
        self.assertEqual(review.commit_mismatch("0da32d0", None), "no commit")

    # Checked on the first cell too, which the drift rule alone lets through.
    def test_the_first_cell_is_held_to_it(self):
        build = {"id": "ref", "label": "r", "nc": "/bin/ref",
                 "expect_commit": "0da32d0"}
        directory = Path(tempfile.mkdtemp(prefix="nc-review-cell-"))
        message = review.record_identity(
            {}, build, "F1/a",
            {"identity": {"git_commit": "9a61136bfe6e", "git_dirty": False}},
            directory / "F1-a.tiff")
        self.assertIn("'ref'", message)
        self.assertIn("0da32d0", message)
        self.assertIn("9a61136bfe6e", message)


class TestDistinctnessWarnings(unittest.TestCase):
    # A warning and not an error: the task's own acceptance check is that the same
    # binary declared twice yields byte-identical cells, which a generator that
    # refused it could not perform.
    def test_two_builds_that_are_one_file(self):
        warnings = review.duplicate_binary_warnings(
            [{"id": "before", "digest": "a" * 64}, {"id": "after", "digest": "a" * 64}])
        self.assertEqual(len(warnings), 1)
        self.assertIn("byte-identical", warnings[0])

    def test_two_builds_that_are_two_files(self):
        self.assertEqual(review.duplicate_binary_warnings(
            [{"id": "before", "digest": "a" * 64}, {"id": "after", "digest": "b" * 64}]), [])

    # The realistic case is two dirty builds of one commit — the shape a
    # patched-versus-shipped spike takes.
    def test_different_binaries_describing_themselves_identically(self):
        warnings = review.indistinguishable_identity_warnings([
            {"id": "before", "digest": "a" * 64, "identity": {"git_commit": "aaa"}},
            {"id": "after", "digest": "b" * 64, "identity": {"git_commit": "aaa"}}])
        self.assertEqual(len(warnings), 1)
        self.assertIn("cannot tell them apart", warnings[0])

    # Already reported as one file by `duplicate_binary_warnings`; saying they are
    # "different binaries" here too would be both noise and false.
    def test_says_nothing_about_two_builds_that_are_one_file(self):
        self.assertEqual(review.indistinguishable_identity_warnings([
            {"id": "before", "digest": "a" * 64, "identity": {"git_commit": "aaa"}},
            {"id": "after", "digest": "a" * 64, "identity": {"git_commit": "aaa"}}]), [])

    def test_says_nothing_when_the_identities_differ(self):
        self.assertEqual(review.indistinguishable_identity_warnings([
            {"id": "before", "digest": "a" * 64, "identity": {"git_commit": "aaa"}},
            {"id": "after", "digest": "b" * 64, "identity": {"git_commit": "bbb"}}]), [])

    def test_says_nothing_about_a_build_that_identified_itself_nowhere(self):
        self.assertEqual(review.indistinguishable_identity_warnings([
            {"id": "before", "digest": "a" * 64, "identity": None},
            {"id": "after", "digest": "b" * 64, "identity": None}]), [])


class TestProducer(unittest.TestCase):
    IDENTITY = {"nc_version": "0.1.0", "git_commit": "abc123", "pipeline_version": 5}

    def test_a_cell_with_no_build_carries_no_producer(self):
        self.assertIsNone(review.producer_block(None, self.IDENTITY))
        self.assertIsNone(review.producer_block(
            {"id": None, "label": None, "note": None}, self.IDENTITY))

    # Tagged so `analysis/review-reference-cells` adds a variant rather than a
    # second block.
    def test_states_its_kind(self):
        block = review.producer_block(
            {"id": "after", "label": "candidate", "note": None}, self.IDENTITY)
        self.assertEqual(block["kind"], "hanten")
        self.assertEqual(block["label"], "candidate")
        self.assertNotIn("note", block)

    # Derived from what the binary reported. The matrix names a build; it does not
    # get to say what that build is.
    def test_carries_the_identity_the_binary_reported(self):
        block = review.producer_block(
            {"id": "after", "label": "c", "note": "n"}, self.IDENTITY)
        self.assertEqual(block["git_commit"], "abc123")
        self.assertEqual(block["note"], "n")

    def test_a_build_that_rendered_nothing_still_declares_itself(self):
        block = review.producer_block({"id": "after", "label": "c", "note": None}, None)
        self.assertEqual(block, {"kind": "hanten", "label": "c"})

    # The block spreads the identity into `review.json`, so the key set is filtered
    # *here* rather than only by whoever calls it. `params_hash` is the realistic
    # one — it differs per cell, so a leak would make every second cell of a build
    # look like a different binary.
    def test_nothing_the_key_set_does_not_name_reaches_review_json(self):
        block = review.producer_block(
            {"id": "after", "label": "c", "note": None},
            {**self.IDENTITY, "params_hash": "h0", "surprise": "leak"})
        self.assertNotIn("params_hash", block)
        self.assertNotIn("surprise", block)
        self.assertEqual(block["git_commit"], "abc123")

    def test_filtering_twice_is_the_same_as_filtering_once(self):
        raw = {**self.IDENTITY, "params_hash": "h0"}
        build = {"id": "after", "label": "c", "note": None}
        self.assertEqual(review.producer_block(build, raw),
                         review.producer_block(build, review.build_identity(raw)))


class TestReviewDocumentBuilds(unittest.TestCase):
    IMAGES = [{"id": "E1", "label": "E1", "renditions": {}}]

    # The whole point of the additive shape: every set rendered before this existed
    # re-renders byte-identically, which is why `REVIEW_SCHEMA` stays 1.
    def test_a_matrix_with_no_builds_produces_the_document_it_always_did(self):
        doc = review.build_review(load(), self.IMAGES)
        self.assertEqual(doc["configs"],
                         [{"id": "generic", "label": "generic"},
                          {"id": "stock", "label": "stock"}])

    def test_each_config_carries_its_builds_provenance(self):
        doc = review.build_review(load(builds=BUILDS), self.IMAGES,
                                  {"before": {"git_commit": "aaa"},
                                   "after": {"git_commit": "bbb"}})
        self.assertEqual([c["id"] for c in doc["configs"]],
                         ["generic@before", "generic@after",
                          "stock@before", "stock@after"])
        self.assertEqual(doc["configs"][0]["producer"]["git_commit"], "aaa")
        self.assertEqual(doc["configs"][1]["producer"]["git_commit"], "bbb")


#: A stand-in for `hanten` that identifies itself however the test asks.
#:
#: Driven through `cmd_generate` rather than through the helpers, because the
#: ordering is half of what is being tested: the pre-flight runs before any render,
#: and drift is detected between two renders of one build.
FAKE_NC = '''#!{python}
import json, pathlib, sys
here = pathlib.Path(__file__).resolve().parent
if "--version" in sys.argv:
    print("hanten 0.1.0\\ncommit: {banner}")
    raise SystemExit(0)
if "--strict" in sys.argv:
    # The shape measured off the release binary: `--strict` gates *after*
    # encoding, so the image and its sidecar are on disk and the exit is still 1.
    # The counter is deliberately untouched — this call renders no identity.
    failed = pathlib.Path(sys.argv[sys.argv.index("-o") + 1])
    failed.write_bytes(b"II*\\x00")
    failed.with_name(failed.name + ".json").write_text(
        json.dumps({{"meta": {{}}, "params": {{}}}}))
    print("error: --strict: 1 warning(s) present (see report)", file=sys.stderr)
    raise SystemExit(1)
commits = {commits!r}
counter = here / "{name}.count"
index = int(counter.read_text()) if counter.exists() else 0
counter.write_text(str(index + 1))
dest = pathlib.Path(sys.argv[sys.argv.index("-o") + 1])
dest.write_bytes(b"II*\\x00")
identity = {{"nc_version": "0.1.0", "git_dirty": False, "pipeline_version": 5,
            "target": "t", "git_commit": commits[min(index, len(commits) - 1)],
            "params_hash": "h%d" % index}}
dest.with_name(dest.name + ".json").write_text(
    json.dumps({{"meta": identity, "params": {{}}}}))
print(json.dumps({{"identity": identity, "output_stats": {{"mean": [1.0, 1.0, 1.0]}}}}))
'''


class TestBuildAxisEndToEnd(unittest.TestCase):
    """`cmd_generate` over fake binaries: no assets, no real nc, no venv."""

    def setUp(self):
        self.root = Path(tempfile.mkdtemp(prefix="nc-review-e2e-"))
        (self.root / "assets" / "rolls" / "R").mkdir(parents=True)
        (self.root / "assets" / "manifest.json").write_text("{}", encoding="utf-8")
        (self.root / "assets" / "rolls" / "R" / "f1.tif").write_bytes(b"II*\x00")
        (self.root / "fixtures.json").write_text(json.dumps(
            {"rolls": {"R": {"dmin": [0.9, 0.9, 0.9]}},
             "frames": {"F1": {"roll": "R", "file": "f1.tif"},
                        "F2": {"roll": "R", "file": "f1.tif"}}}), encoding="utf-8")

    def binary(self, name: str, *commits: str, banner: str | None = None) -> str:
        """A fake nc whose renders report `commits` in turn and whose `--version`
        states `banner` — the first commit unless a test needs the two to disagree."""
        path = self.root / name
        path.write_text(FAKE_NC.format(python=sys.executable, commits=list(commits),
                                       banner=banner or commits[0],
                                       name=name), encoding="utf-8")
        path.chmod(0o755)
        return str(path)

    def run_generate(self, builds, nc=None, build=None, frames=None, out=None,
                     **matrix) -> tuple[int, str]:
        """One `cmd_generate` run. `builds=None` is the matrix with no build axis."""
        path = write({**MATRIX, "output_preset": "legacy", "common_args": [],
                      "configs": [{"id": "dflt", "args": []}],
                      "builds": builds, **matrix})
        args = argparse.Namespace(
            matrix=str(path), fixtures=str(self.root / "fixtures.json"),
            frames=frames, nc=nc, build=build,
            asset_root=str(self.root / "assets"),
            out=out or str(self.root / "out"),
            no_metrics=True, force=False)
        err = io.StringIO()
        with contextlib.redirect_stderr(err):
            code = review.cmd_generate(args)
        return code, err.getvalue()

    def review_json(self) -> dict:
        return json.loads((self.root / "out" / "review.json").read_text(encoding="utf-8"))

    def test_two_builds_yield_two_cells_labelled_from_their_own_identity(self):
        code, _err = self.run_generate([
            {"id": "before", "label": "shipped", "nc": self.binary("a", "aaa")},
            {"id": "after", "label": "candidate", "nc": self.binary("b", "bbb")}])
        self.assertEqual(code, 0)
        doc = self.review_json()
        self.assertEqual([c["id"] for c in doc["configs"]],
                         ["dflt@before", "dflt@after"])
        self.assertEqual([c["producer"]["git_commit"] for c in doc["configs"]],
                         ["aaa", "bbb"])
        self.assertEqual([c["label"] for c in doc["configs"]],
                         ["dflt · shipped", "dflt · candidate"])
        self.assertEqual(sorted(doc["images"][0]["renditions"]),
                         ["dflt@after", "dflt@before"])

    # The recipe hash differs per cell by design; treating it as build identity
    # would make the second frame of every build look like a different binary.
    def test_a_build_rendering_many_frames_is_one_build(self):
        code, err = self.run_generate(
            [{"id": "only", "label": "only", "nc": self.binary("a", "aaa")}])
        self.assertEqual(code, 0, err)
        self.assertEqual(len(self.review_json()["images"]), 2)

    # The task's flag: the matrix claims no identity, so the only thing a cell can
    # disagree with is the rest of its own build.
    def test_a_build_that_changes_identity_mid_run_writes_nothing(self):
        code, err = self.run_generate(
            [{"id": "only", "label": "only", "nc": self.binary("a", "aaa", "bbb")}])
        self.assertEqual(code, 1)
        self.assertIn("reported one identity and then another", err)
        self.assertIn("aaa", err)
        self.assertIn("bbb", err)
        self.assertIn("No review.json was written", err)
        self.assertFalse((self.root / "out" / "review.json").exists())

    # The reference arm pointed at the wrong binary: without an expectation it
    # renders and is labelled with its real commit, but nothing refuses it.
    def test_a_build_that_is_not_its_expected_commit_writes_nothing(self):
        wrong = self.binary("a", "bbb000000000")
        code, err = self.run_generate(
            [{"id": "ref", "label": "reference", "nc": wrong}])
        self.assertEqual(code, 0, err)
        renders = (self.root / "a.count").read_text()
        before = self.review_json()
        code, err = self.run_generate(
            [{"id": "ref", "label": "reference", "nc": wrong,
              "expect_commit": "aaa0000"}])
        self.assertEqual(code, 2)
        self.assertIn("is not a clean build of aaa0000", err)
        self.assertIn("bbb000000000", err)
        # Refused in the pre-flight: no cell rendered over, the earlier set untouched.
        self.assertEqual((self.root / "a.count").read_text(), renders)
        self.assertEqual(self.review_json(), before)

    # The render-time check is the backstop: a binary whose banner agrees but whose
    # renders do not (it changed after the pre-flight) still aborts the run.
    def test_a_render_that_is_not_the_expected_commit_aborts(self):
        code, err = self.run_generate(
            [{"id": "ref", "label": "reference", "expect_commit": "aaa0000",
              "nc": self.binary("a", "bbb000000000", banner="aaa000000000")}])
        self.assertEqual(code, 1)
        self.assertIn("expects a clean build of aaa0000", err)
        self.assertIn("No review.json was written", err)
        self.assertFalse((self.root / "out" / "review.json").exists())

    def test_a_build_that_is_its_expected_commit_renders(self):
        code, err = self.run_generate(
            [{"id": "ref", "label": "reference", "nc": self.binary("a", "aaa000000000"),
              "expect_commit": "aaa0000"}])
        self.assertEqual(code, 0, err)
        self.assertEqual(self.review_json()["configs"][0]["producer"]["git_commit"],
                         "aaa000000000")

    # The cells this run had already re-rendered were overwritten in place, so a
    # `review.json` left by an earlier run now names new pixels under the old
    # build's identity — and the app watches the set, so a page already open
    # refreshes straight onto it.
    def test_drift_takes_an_earlier_runs_review_set_with_it(self):
        stable = self.binary("a", "aaa")
        code, err = self.run_generate([{"id": "only", "label": "only", "nc": stable}])
        self.assertEqual(code, 0, err)
        self.assertEqual(self.review_json()["configs"][0]["producer"]["git_commit"],
                         "aaa")

        drifting = self.binary("b", "bbb", "ccc")
        code, err = self.run_generate([{"id": "only", "label": "only", "nc": drifting}])
        self.assertEqual(code, 1)
        self.assertIn("No review.json was written", err)
        self.assertIn("removed the stale", err)
        # Named, not asserted in the abstract: the deletion is justified only by the
        # cells this run actually put new bytes into.
        self.assertIn("F1-dflt@only.tiff", err)
        self.assertFalse((self.root / "out" / "review.json").exists())

    # **The drift rule is not build-axis-only.** A matrix with no `builds` renders
    # through one *unnamed* build — the `--nc` binary — and a binary that changes
    # underneath it is the same fault, with the same abort. This is the path most
    # runs take, and nothing covered it.
    def test_a_run_with_no_build_axis_aborts_on_drift_too(self):
        drifting = self.binary("solo", "aaa", "bbb")
        code, err = self.run_generate(None, nc=drifting)
        self.assertEqual(code, 1)
        self.assertIn("reported one identity and then another", err)
        self.assertIn("No review.json was written", err)
        self.assertFalse((self.root / "out" / "review.json").exists())
        # There is no build id to print, so it must name the binary instead.
        self.assertIn(drifting, err)
        self.assertNotIn("build None", err)

    # A run that landed on none of the cells the earlier set names told it no lie,
    # so the set is still true and is left where it is. Deleting it — and saying it
    # held cells this run overwrote — was both destructive and false.
    def test_drift_leaves_an_earlier_set_alone_when_it_overwrote_none_of_it(self):
        code, err = self.run_generate(None, nc=self.binary("stable", "aaa"))
        self.assertEqual(code, 0, err)
        before = self.review_json()
        self.assertEqual(sorted(i["id"] for i in before["images"]), ["F1", "F2"])

        code, err = self.run_generate(
            None, nc=self.binary("wobble", "bbb", "ccc"), frames="F2",
            configs=[{"id": "a", "args": []}, {"id": "b", "args": []}])
        self.assertEqual(code, 1)
        self.assertIn("reported one identity and then another", err)
        self.assertIn("left the earlier", err)
        self.assertNotIn("removed the stale", err)
        self.assertTrue((self.root / "out" / "review.json").exists())
        self.assertEqual(self.review_json(), before)

    def rewrite_renditions(self, transform) -> None:
        """Re-author the set on disk the way a merge or another tool would."""
        path = self.root / "out" / "review.json"
        doc = json.loads(path.read_text(encoding="utf-8"))
        for image in doc["images"]:
            image["renditions"] = {config: transform(rendition)
                                   for config, rendition in image["renditions"].items()}
        path.write_text(json.dumps(doc), encoding="utf-8")

    def drift_over_the_same_cells(self, name: str) -> str:
        """A second run of the same matrix, on a binary that changes its mind."""
        code, err = self.run_generate(None, nc=self.binary(name, "bbb", "ccc"))
        self.assertEqual(code, 1, err)
        return err

    # Every axis below is one spelling this run does not itself produce, landing on
    # the branch that *spares* the set — which then asserts it still describes its
    # own pixels. Over-deleting was the old behaviour and was never wrong in this
    # direction; a spared lie is.
    def test_drift_sees_a_rendition_written_in_the_string_shorthand(self):
        code, err = self.run_generate(None, nc=self.binary("stable", "aaa"))
        self.assertEqual(code, 0, err)
        # SCHEMA.md's headline form, and nothing the app would refuse.
        self.rewrite_renditions(lambda rendition: rendition["src"])

        err = self.drift_over_the_same_cells("wobble")
        self.assertIn("removed the stale", err)
        self.assertNotIn("still describes its own pixels", err)
        self.assertFalse((self.root / "out" / "review.json").exists())

    # `src` is a path relative to `review.json`, not a bare filename. The spelling
    # here keeps a `..` on purpose: `pathlib` folds a leading `./` away by itself,
    # so only a segment it will not normalise proves the resolution is happening.
    def test_drift_sees_a_path_qualified_src(self):
        code, err = self.run_generate(None, nc=self.binary("stable", "aaa"))
        self.assertEqual(code, 0, err)
        self.rewrite_renditions(
            lambda rendition: {**rendition, "src": "renders/../" + rendition["src"]})

        err = self.drift_over_the_same_cells("wobble")
        self.assertIn("removed the stale", err)
        self.assertNotIn("still describes its own pixels", err)
        self.assertFalse((self.root / "out" / "review.json").exists())

    # The axis that needs no hand-authored set: two ordinary runs in the
    # rebuild-into-the-same-directory loop `--build` was added to invite. On this
    # volume `F1-Dflt.tiff` and `F1-dflt.tiff` are one file, so run 2 really does
    # sit on run 1's pixels. `colliding_stems` only ever sees one matrix, so it
    # cannot catch a spelling that changed *between* runs.
    def test_drift_sees_a_cell_respelled_in_another_case(self):
        code, err = self.run_generate(None, nc=self.binary("stable", "aaa"),
                                      configs=[{"id": "Dflt", "args": []}])
        self.assertEqual(code, 0, err)

        code, err = self.run_generate(None, nc=self.binary("wobble", "bbb", "ccc"),
                                      configs=[{"id": "dflt", "args": []}])
        self.assertEqual(code, 1)
        self.assertIn("removed the stale", err)
        self.assertNotIn("still describes its own pixels", err)
        self.assertFalse((self.root / "out" / "review.json").exists())

    # **A render that fails can still have written its output.** `--strict` gates
    # after encoding, so the real binary writes the image and the sidecar and then
    # exits 1 — checked against it, not assumed — and `--strict` is not an owned
    # flag, so an ordinary config may pass it. Booking the destination only on
    # success left exactly those cells unaccounted for, and the stale set naming
    # only them then got the vouching sentence over pixels this run replaced.
    def test_drift_accounts_for_a_cell_whose_render_failed_after_writing(self):
        code, err = self.run_generate(None, nc=self.binary("stable", "aaa"))
        self.assertEqual(code, 0, err)
        # The earlier set names the `dflt` cells and nothing else — and those are
        # precisely the ones run 2 writes through its *failing* path.
        indexed = sorted(r["src"] for image in self.review_json()["images"]
                         for r in image["renditions"].values())
        self.assertEqual(indexed, ["F1-dflt.tiff", "F2-dflt.tiff"])

        code, err = self.run_generate(
            None, nc=self.binary("wobble", "bbb", "ccc"),
            configs=[{"id": "dflt", "args": ["--strict"]}, {"id": "x", "args": []}])
        self.assertEqual(code, 1)
        self.assertIn("reported one identity and then another", err)
        # The failing cells were reported as failures, so the run really did take
        # the `except` path for them.
        self.assertIn("--strict: 1 warning(s) present", err)
        self.assertIn("removed the stale", err)
        self.assertIn("F1-dflt.tiff", err)
        self.assertNotIn("still describes its own pixels", err)
        self.assertFalse((self.root / "out" / "review.json").exists())

    # A set this cannot parse indexes nothing *as far as the compare knows*, which
    # is not the same fact. Leaving it is right — the app refuses it loudly — but
    # the run must not vouch for it.
    def test_drift_does_not_vouch_for_a_set_it_could_not_read(self):
        code, err = self.run_generate(None, nc=self.binary("stable", "aaa"))
        self.assertEqual(code, 0, err)
        stale = self.root / "out" / "review.json"
        stale.write_text("{ this is not json", encoding="utf-8")

        err = self.drift_over_the_same_cells("wobble")
        self.assertIn("could not read every rendition it names", err)
        self.assertNotIn("still describes its own pixels", err)
        self.assertTrue(stale.is_file())

    def test_the_same_binary_under_two_names_is_a_note_not_a_refusal(self):
        binary = self.binary("a", "aaa")
        code, err = self.run_generate([
            {"id": "before", "label": "before", "nc": binary},
            {"id": "after", "label": "after", "nc": binary}])
        self.assertEqual(code, 0, err)
        self.assertIn("are the same binary", err)
        out = self.root / "out"
        self.assertEqual((out / "F1-dflt@before.tiff").read_bytes(),
                         (out / "F1-dflt@after.tiff").read_bytes())

    def test_a_config_may_opt_out_of_a_build(self):
        code, err = self.run_generate(
            [{"id": "before", "label": "b", "nc": self.binary("a", "aaa")},
             {"id": "after", "label": "a", "nc": self.binary("b", "bbb")}],
            configs=[{"id": "both", "args": []},
                     {"id": "new", "args": [], "builds": ["after"]}])
        self.assertEqual(code, 0, err)
        self.assertEqual([c["id"] for c in self.review_json()["configs"]],
                         ["both@before", "both@after", "new@after"])

    # Driven through `cmd_generate`, because the rule is only useful if it runs
    # before the coarser gates — and a test calling it directly never sees that.
    def test_refuses_nc_beside_a_build_axis(self):
        code, err = self.run_generate(
            [{"id": "after", "label": "a", "nc": self.binary("b", "bbb")}],
            nc=self.binary("a", "aaa"))
        self.assertEqual(code, 2)
        self.assertIn("--build <id>=<path>", err)

    # The ordering case: a frame fault on the same command line used to hide it.
    # Asserting the losing rule's wording is *absent* is the only way to tell the
    # two apart — both mention the matrix.
    def test_the_nc_contradiction_beats_an_unknown_frame(self):
        code, err = self.run_generate(
            [{"id": "after", "label": "a", "nc": self.binary("b", "bbb")}],
            nc=self.binary("a", "aaa"), frames="NOPE")
        self.assertEqual(code, 2)
        self.assertIn("--build <id>=<path>", err)
        self.assertNotIn("no such frame", err)

    def test_refuses_an_override_naming_a_build_the_matrix_does_not_declare(self):
        code, err = self.run_generate(
            [{"id": "after", "label": "a", "nc": self.binary("b", "bbb")}],
            build=["durring=/x"])
        self.assertEqual(code, 2)
        self.assertIn("no such build: durring", err)
        self.assertNotIn("no hanten binary", err)

    # Deliberate priority, kept: where the pictures land outranks which binary
    # makes them, so a bad `--build` must not pre-empt the repo refusal.
    def test_rendering_into_the_repository_is_refused_before_a_bad_override(self):
        repo = Path(review.__file__).resolve().parents[3]
        code, err = self.run_generate(
            [{"id": "after", "label": "a", "nc": self.binary("b", "bbb")}],
            build=["durring=/x"], out=str(repo / "tools"))
        self.assertEqual(code, 2)
        self.assertIn("inside the repository", err)
        self.assertNotIn("no such build", err)

    def test_an_override_repoints_a_build_without_editing_the_matrix(self):
        path = write({**MATRIX, "output_preset": "legacy", "common_args": [],
                      "configs": [{"id": "dflt", "args": []}],
                      "builds": [{"id": "after", "label": "a",
                                  "nc": "/nonexistent/x"}]})
        args = argparse.Namespace(
            matrix=str(path), fixtures=str(self.root / "fixtures.json"),
            frames="F1", nc=None, build=[f"after={self.binary('b', 'bbb')}"],
            asset_root=str(self.root / "assets"), out=str(self.root / "out"),
            no_metrics=True, force=False)
        err = io.StringIO()
        with contextlib.redirect_stderr(err):
            self.assertEqual(review.cmd_generate(args), 0, err.getvalue())
        self.assertEqual(self.review_json()["configs"][0]["producer"]["git_commit"],
                         "bbb")
