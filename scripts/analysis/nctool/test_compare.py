"""Stdlib unit tests for `nctool.compare` (run: `python3 -m unittest`).

Hermetic: no real assets, no `nc` binary. `convert_case`'s subprocess call is faked
so the report/telemetry contract is exercised without a conversion, and the diff
logic is driven from hand-written run records. Focuses on the parts a wrong answer
would be *silent* in: the zero-diff verdict for an identical build, timings being
excluded from that verdict, a dropped frame not reading as "no change", the loud
rejection of a pre-versioning or malformed report, records that cannot be compared at
all, a dirty tree not being mistaken for one build, and checksum resolution through
the asset manifest (never a second inventory).
"""
from __future__ import annotations

import io
import json
import os
import stat
import sys
import tempfile
import unittest
from unittest import mock

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from nctool import compare  # noqa: E402


def frame(name, mean, clipped=0, total=300, timing=None, phash="aaaa",
          sha="00ff", checksums="computed", depth="u16"):
    """A run-record frame entry, with the clip fraction kept consistent with the
    counts (the record writer derives it, so a test fixture must too)."""
    return dict(name=name, input=f"{name}.tif", input_sha256=sha, checksums=checksums,
                params_hash=phash, output_depth=depth, mean=list(mean),
                clipped=clipped, non_finite=0, total_samples=total,
                clip_fraction=(clipped / total if total else 0.0),
                timing_ms=timing or {"total": 10.0, "encode": 1.0})


def record(frames, pipeline_version=1, commit="abc123", dirty=False,
           target="aarch64-apple-darwin"):
    return dict(schema_version=compare.RECORD_SCHEMA, benchmark_set="fixtures",
                identity=dict(nc_version="0.1.0", git_commit=commit, git_dirty=dirty,
                              pipeline_version=pipeline_version, target=target),
                frames=frames)


def nc_report(params_hash="feed", mean=(0.25, 0.5, 0.75), depth="u16",
              encoding="rendered-u16-tiff",
              total=400, low=10, high=30, non_finite=0):
    """A complete `nc convert` report, i.e. every block+field `run` reads.

    `depth` is the `output.depth` **knob** and `encoding` the container the preset
    actually resolved. They are separate arguments because the whole point of the
    marker is that an atomic preset makes them disagree."""
    return dict(identity=dict(nc_version="0.1.0", git_commit="abc", git_dirty=False,
                              pipeline_version=1, target="t", params_hash=params_hash),
                output_stats=dict(mean=list(mean)),
                recipe=dict(output=dict(depth=depth)),
                output_render=dict(preset="legacy", encoding=encoding),
                loss=dict(total_samples=total, clipped_low=low, clipped_high=high,
                          non_finite=non_finite))


class TestDiff(unittest.TestCase):
    def test_same_record_is_a_zero_diff(self):
        # The headline determinism contract: re-running the same build yields
        # `identical: true` and all-zero deltas.
        r = record([frame("a", [0.1, 0.2, 0.3]), frame("b", [0.4, 0.5, 0.6])])
        rows, identical = compare.diff_frames(r, json.loads(json.dumps(r)))
        self.assertTrue(identical)
        for row in rows:
            self.assertEqual(row["mean_delta_rgb"], [0.0, 0.0, 0.0])
            self.assertEqual(row["clip_fraction_delta"], 0.0)
            self.assertFalse(row["params_hash_changed"])
            self.assertFalse(row["input_sha256_changed"])

    def test_timings_alone_never_break_the_verdict(self):
        # Wall clocks differ between any two runs. If they counted, a zero diff would
        # be impossible and the whole verdict would be worthless.
        a = record([frame("a", [0.1, 0.2, 0.3], timing={"total": 10.0})])
        b = record([frame("a", [0.1, 0.2, 0.3], timing={"total": 31.5})])
        rows, identical = compare.diff_frames(a, b)
        self.assertTrue(identical, "a timing difference must not break `identical`")
        self.assertEqual(rows[0]["timing_ms_delta"]["total"], 21.5,
                         "but it must still be reported")

    def test_a_changed_mean_is_detected_with_a_signed_delta(self):
        a = record([frame("a", [0.10, 0.20, 0.30])])
        b = record([frame("a", [0.15, 0.20, 0.25])])
        rows, identical = compare.diff_frames(a, b)
        self.assertFalse(identical)
        self.assertEqual(rows[0]["mean_delta_rgb"], [0.05, 0.0, -0.05])

    def test_a_changed_clip_fraction_is_detected(self):
        a = record([frame("a", [0.1, 0.2, 0.3], clipped=0)])
        b = record([frame("a", [0.1, 0.2, 0.3], clipped=30)])
        rows, identical = compare.diff_frames(a, b)
        self.assertFalse(identical)
        self.assertEqual(rows[0]["clipped_delta"], 30)
        self.assertAlmostEqual(rows[0]["clip_fraction_delta"], 0.1)

    def test_a_dropped_frame_is_not_no_change(self):
        # A benchmark set that silently shrank must never read as "identical".
        a = record([frame("a", [0.1, 0.2, 0.3]), frame("b", [0.1, 0.2, 0.3])])
        b = record([frame("a", [0.1, 0.2, 0.3])])
        rows, identical = compare.diff_frames(a, b)
        self.assertFalse(identical)
        missing = [r for r in rows if r["status"] == "missing"]
        self.assertEqual([r["name"] for r in missing], ["b"])
        self.assertEqual(missing[0]["present_in"], "a")

    def test_a_changed_params_hash_is_flagged(self):
        a = record([frame("a", [0.1, 0.2, 0.3], phash="1111")])
        b = record([frame("a", [0.1, 0.2, 0.3], phash="2222")])
        rows, identical = compare.diff_frames(a, b)
        self.assertFalse(identical, "a different effective recipe is a difference")
        self.assertTrue(rows[0]["params_hash_changed"])

    def test_a_depth_change_withholds_the_mean_delta_instead_of_reporting_units(self):
        # u16 means are quantized into [0,1]; f32 means are verbatim and unclamped.
        # Subtracting one from the other reports a UNIT change as a rendering change.
        a = record([frame("a", [0.5, 0.5, 0.5], depth="u16")])
        b = record([frame("a", [2.0, 2.0, 2.0], depth="f32")])
        rows, identical = compare.diff_frames(a, b)
        self.assertFalse(identical)
        self.assertTrue(rows[0]["output_depth_changed"])
        self.assertIsNone(rows[0]["mean_delta_rgb"],
                          "an incomparable delta must be withheld, not computed")


class TestDiffCommand(unittest.TestCase):
    def run_diff(self, a, b):
        with tempfile.TemporaryDirectory() as d:
            pa, pb = os.path.join(d, "a.json"), os.path.join(d, "b.json")
            for p, r in ((pa, a), (pb, b)):
                with open(p, "w", encoding="utf-8") as fh:
                    json.dump(r, fh)
            return self.run_diff_paths(pa, pb)

    def run_diff_paths(self, pa, pb):
        args = mock.Mock(before=pa, after=pb)
        out, err = io.StringIO(), io.StringIO()
        with mock.patch.object(sys, "stdout", out), \
             mock.patch.object(sys, "stderr", err):
            code = compare.cmd_diff(args)
        return code, out.getvalue(), err.getvalue()

    def test_report_is_keyed_on_both_identities(self):
        # A diff is only interpretable when attributed to a pipeline_version+commit
        # pair on each side.
        a = record([frame("a", [0.1, 0.2, 0.3])], pipeline_version=1, commit="aaa")
        b = record([frame("a", [0.2, 0.2, 0.3])], pipeline_version=2, commit="bbb")
        code, out, _ = self.run_diff(a, b)
        self.assertEqual(code, 0)
        doc = json.loads(out)
        self.assertEqual(doc["before"]["pipeline_version"], 1)
        self.assertEqual(doc["after"]["pipeline_version"], 2)
        self.assertEqual(doc["before"]["git_commit"], "aaa")
        self.assertFalse(doc["identical"])
        self.assertTrue(doc["pipeline_version_changed"])

    def test_same_clean_build_with_a_difference_is_a_hard_failure(self):
        # One CLEAN build producing two different results breaks determinism — the
        # loudest thing this harness can find, so it exits non-zero.
        a = record([frame("a", [0.1, 0.2, 0.3])], dirty=False)
        b = record([frame("a", [0.9, 0.2, 0.3])], dirty=False)
        code, out, err = self.run_diff(a, b)
        self.assertEqual(code, 1)
        self.assertIn("deterministic", err)
        # The report still lands on stdout — the non-zero exit is the signal, the
        # document is the evidence (the same contract `nc`'s own reports follow).
        self.assertFalse(json.loads(out)["identical"])

    def test_two_dirty_records_at_one_commit_are_not_a_determinism_failure(self):
        # Two builds from DIFFERENT uncommitted trees at the same commit produce
        # identical identity dicts. Calling that "the pipeline is not deterministic"
        # would fire on the most ordinary workflow there is: iterating on a change.
        a = record([frame("a", [0.1, 0.2, 0.3])], dirty=True)
        b = record([frame("a", [0.9, 0.2, 0.3])], dirty=True)
        code, out, err = self.run_diff(a, b)
        self.assertEqual(code, 0, err)
        self.assertNotIn("not deterministic", err)
        self.assertIn("does not pin the source", err)
        self.assertFalse(json.loads(out)["identical"])

    def test_a_missing_commit_also_fails_to_pin_the_source(self):
        a = record([frame("a", [0.1, 0.2, 0.3])], commit=None, dirty=None)
        b = record([frame("a", [0.9, 0.2, 0.3])], commit=None, dirty=None)
        code, _, err = self.run_diff(a, b)
        self.assertEqual(code, 0, err)
        self.assertIn("does not pin the source", err)
        self.assertFalse(compare.pins_source(a["identity"]))

    def blocked(self, a, b, expect):
        """Assert a non-zero diff whose determinism check was BLOCKED: rc 0 (verdict
        delivered), `identical: false`, and a note naming the precondition to fix."""
        code, out, err = self.run_diff(a, b)
        self.assertEqual(code, 0, err)
        self.assertNotIn("not deterministic", err)
        doc = json.loads(out)
        self.assertFalse(doc["identical"])
        blocked = [n for n in doc["notes"] if n.startswith("determinism_check_blocked")]
        self.assertTrue(blocked, f"expected a blocked note, got {doc['notes']}")
        self.assertTrue(any(expect in n for n in blocked),
                        f"no blocked note mentions {expect!r}: {blocked}")

    def test_the_determinism_claim_requires_every_other_cause_ruled_out(self):
        # The invariant: `compare` may blame the pipeline only when it has eliminated
        # every OTHER explanation for a difference. Guarding on the build identity alone
        # left five routes to the same false accusation — each measured at rc 1 "not
        # deterministic" with the SAME CLEAN identity on both sides.
        base, differing = [0.1, 0.2, 0.3], [0.9, 0.2, 0.3]

        # (a) unverified input bytes: `diff` used to print a note conceding the bytes
        #     were never verified and then contradict itself on the very next line.
        self.blocked(record([frame("a", base, sha=None, checksums="skipped")]),
                     record([frame("a", differing, sha=None, checksums="skipped")]),
                     "unverified input bytes")

        # (b) a different recipe — the plainest error of the set: differing
        #     `params_hash` means the two runs used different recipes, so of course the
        #     output differs.
        self.blocked(record([frame("a", base, phash="p1")]),
                     record([frame("a", differing, phash="p2")]),
                     "DIFFERENT RECIPE")

        # (c) a different output depth — the two means are in different units.
        self.blocked(record([frame("a", base, depth="u16")]),
                     record([frame("a", differing, depth="f32")]),
                     "different output depth")

        # (d) a different frame set — the two runs did not convert the same work.
        self.blocked(record([frame("a", base), frame("b", base)]),
                     record([frame("a", differing)]),
                     "frame sets differ")

        # (e) a dirty tree — the original route (round 1), now one precondition of many.
        self.blocked(record([frame("a", base)], dirty=True),
                     record([frame("a", differing)], dirty=True),
                     "does not pin the source")

    def test_the_determinism_claim_still_fires_when_nothing_else_explains_it(self):
        # The important half of the tightening: it must not make the determinism check
        # UNREACHABLE, which would silently remove the guarantee entirely. Everything
        # matches — same clean commit, same inputs by digest, same recipe, same depth,
        # same frame set — so the pipeline is the only remaining explanation.
        a = record([frame("a", [0.1, 0.2, 0.3])])
        b = record([frame("a", [0.9, 0.2, 0.3])])
        self.assertEqual(compare.determinism_blockers(a, b), [])
        code, out, err = self.run_diff(a, b)
        self.assertEqual(code, 1)
        self.assertIn("not deterministic", err)
        self.assertFalse(json.loads(out)["identical"])
        # And it says WHY it is confident — an accusation this serious has to be able
        # to name what it ruled out.
        for ruled_out in ("same commit", "clean tree", "identical input bytes by sha256",
                          "identical params_hash", "identical output depth",
                          "same frame set"):
            self.assertIn(ruled_out, err)

    def test_every_offending_frame_is_named_not_just_the_first(self):
        # A multi-frame set must name each frame that blocks the check: the operator
        # needs the whole list to know what to fix.
        a = record([frame("a", [0.1] * 3, phash="p1"), frame("b", [0.1] * 3, depth="u16")])
        b = record([frame("a", [0.9] * 3, phash="p2"), frame("b", [0.9] * 3, depth="f32")])
        blockers = compare.determinism_blockers(a, b)
        self.assertTrue(any("'a'" in r and "DIFFERENT RECIPE" in r for r in blockers),
                        blockers)
        self.assertTrue(any("'b'" in r and "output depth" in r for r in blockers),
                        blockers)

    def test_mismatched_benchmark_sets_are_refused(self):
        a = record([frame("a", [0.1, 0.2, 0.3])])
        b = record([frame("a", [0.1, 0.2, 0.3])])
        b["benchmark_set"] = "rolls"
        code, _, err = self.run_diff(a, b)
        self.assertEqual(code, 2)
        self.assertIn("different benchmark sets", err)

    def test_empty_documents_are_refused_instead_of_identical(self):
        # Two `{}` files used to diff to `identical: true` at exit 0 — a CI gate
        # reading a path typo would go green having compared nothing.
        code, _, err = self.run_diff({}, {})
        self.assertEqual(code, 2)
        self.assertIn("schema_version", err)

    def test_a_frameless_record_is_refused(self):
        a, b = record([]), record([])
        code, _, err = self.run_diff(a, b)
        self.assertEqual(code, 2)
        self.assertIn("no frames", err)

    def test_a_mismatched_schema_version_is_refused(self):
        a = record([frame("a", [0.1, 0.2, 0.3])])
        b = record([frame("a", [0.1, 0.2, 0.3])])
        b["schema_version"] = 99
        code, _, err = self.run_diff(a, b)
        self.assertEqual(code, 2)
        self.assertIn("schema_version", err)

    def test_a_frame_missing_a_verdict_field_is_refused(self):
        # `None != None` is not agreement. Defaulting an absent field made the clip
        # half of the verdict trivially equal instead of loudly missing.
        for field in ("mean", "clipped", "params_hash", "total_samples"):
            a = record([frame("a", [0.1, 0.2, 0.3])])
            b = record([frame("a", [0.1, 0.2, 0.3])])
            for r in (a, b):
                del r["frames"][0][field]
            with self.subTest(field=field):
                code, _, err = self.run_diff(a, b)
                self.assertEqual(code, 2)
                self.assertIn(field, err)

    def test_an_explicit_null_count_is_a_loud_refusal_not_a_traceback(self):
        # A `null` clipped used to raise TypeError and exit 1 — the same code as "not
        # deterministic", so a malformed record read as a determinism failure.
        a = record([frame("a", [0.1, 0.2, 0.3])])
        b = record([frame("a", [0.1, 0.2, 0.3])])
        b["frames"][0]["clipped"] = None
        code, _, err = self.run_diff(a, b)
        self.assertEqual(code, 2)
        self.assertIn("clipped", err)

    def test_rediffing_a_diff_report_is_refused(self):
        # The realistic path-typo accident: a CI step writes `compare diff > x.json`
        # and a later step diffs that. Its rows carry deltas, not measurements.
        a = record([frame("a", [0.1, 0.2, 0.3])], commit="aaa")
        b = record([frame("a", [0.2, 0.2, 0.3])], commit="bbb")
        code, out, _ = self.run_diff(a, b)
        self.assertEqual(code, 0)
        with tempfile.TemporaryDirectory() as d:
            p = os.path.join(d, "diff.json")
            with open(p, "w", encoding="utf-8") as fh:
                fh.write(out)
            code, _, err = self.run_diff_paths(p, p)
        self.assertEqual(code, 2, err)

    def test_a_non_json_input_is_a_message_not_a_traceback(self):
        # Diffing a TIFF (an easy before/after mix-up) must not raise
        # UnicodeDecodeError out of the harness.
        with tempfile.TemporaryDirectory() as d:
            p = os.path.join(d, "scan.tif")
            with open(p, "wb") as fh:
                fh.write(b"II*\0\xff\xfe\xfd")
            code, _, err = self.run_diff_paths(p, p)
        self.assertEqual(code, 2)
        self.assertIn("cannot read", err)

    def test_a_changed_input_digest_refuses_the_comparison(self):
        # Different input bytes make this not a build comparison at all — the exact
        # misattribution the checksum exists to prevent.
        a = record([frame("a", [0.1, 0.2, 0.3], sha="1111")])
        b = record([frame("a", [0.9, 0.2, 0.3], sha="2222")])
        code, _, err = self.run_diff(a, b)
        self.assertEqual(code, 2)
        self.assertIn("DIFFERENT input bytes", err)

    def test_skipped_checksums_are_surfaced_in_the_diff(self):
        a = record([frame("a", [0.1, 0.2, 0.3], sha=None, checksums="skipped")])
        b = record([frame("a", [0.1, 0.2, 0.3], sha=None, checksums="skipped")])
        code, out, err = self.run_diff(a, b)
        self.assertEqual(code, 0, err)
        doc = json.loads(out)
        self.assertTrue(doc["checksums_skipped"])
        self.assertIn("checksums_skipped", err)
        # Unverifiable bytes must not be reported as an input change either.
        self.assertIsNone(doc["frames"][0]["input_sha256_changed"])

    def test_a_record_with_no_checksum_evidence_cannot_claim_verification(self):
        # `checksums_skipped: false` is an AFFIRMATIVE claim derived from a field that
        # used to be optional. Measured before the fix, with `checksums` and
        # `input_sha256` stripped and DIFFERENT commits on each side:
        #     rc=0  "identical": true  "checksums_skipped": false  notes=[]
        # — two builds over unverified (or genuinely different) input bytes reported
        # identical, with a positive attestation that verification happened. In the
        # artifact whose whole purpose is attribution, that is worse than an omission.
        a = record([frame("a", [0.1, 0.2, 0.3])], commit="aaa")
        b = record([frame("a", [0.1, 0.2, 0.3])], commit="bbb")
        for r in (a, b):
            del r["frames"][0]["checksums"]
            del r["frames"][0]["input_sha256"]
        code, out, err = self.run_diff(a, b)
        self.assertEqual(code, 2)
        self.assertIn("checksums", err)
        self.assertEqual(out, "", "a refused comparison must not emit an attestation")

        # An unreadable mode is refused for the same reason.
        a, b = record([frame("a", [0.1] * 3)]), record([frame("a", [0.1] * 3)])
        for r in (a, b):
            r["frames"][0]["checksums"] = "maybe"
        code, _, err = self.run_diff(a, b)
        self.assertEqual(code, 2)
        self.assertIn("expected one of", err)

        # And a mode that CLAIMS a digest must carry one — an unsubstantiated
        # "verified" is the same false attestation by another route.
        for mode in ("verified", "computed"):
            a = record([frame("a", [0.1] * 3, checksums=mode, sha=None)], commit="aaa")
            b = record([frame("a", [0.1] * 3, checksums=mode, sha=None)], commit="bbb")
            with self.subTest(mode=mode):
                code, _, err = self.run_diff(a, b)
                self.assertEqual(code, 2)
                self.assertIn("unsubstantiated", err)

        # `skipped` with no digest stays legal — that is the honest state, and the
        # diff surfaces it as a caveat (covered by the test above).
        ok = record([frame("a", [0.1] * 3, checksums="skipped", sha=None)])
        self.assertIsNone(compare.validate_record(ok, "ok"))

    def test_duplicate_frame_names_are_refused(self):
        # Frames are matched BY NAME, so a duplicate silently keeps only the last
        # entry. Measured before the fix: each record held two frames, ONE was
        # compared, and because the surviving pair agreed the verdict was
        # `identical: true` at rc 0 — the first frame's disagreement vanished.
        a = record([frame("a", [0.1, 0.2, 0.3]), frame("a", [0.9, 0.9, 0.9])],
                   commit="aaa")
        b = record([frame("a", [0.5, 0.5, 0.5]), frame("a", [0.9, 0.9, 0.9])],
                   commit="bbb")
        code, _, err = self.run_diff(a, b)
        self.assertEqual(code, 2)
        self.assertIn("duplicate frame name", err)

    def test_a_record_without_the_depth_marker_is_refused(self):
        # A *missing* `output_depth` equals a missing `output_depth`, so two older records
        # got `output_depth_changed=False` and their u16 and f32 means were compared —
        # or, when the numbers coincided, declared identical. Refusing is the only safe
        # reading of "the units are unknown".
        a = record([frame("a", [0.5, 0.5, 0.5])])
        b = record([frame("a", [0.5, 0.5, 0.5])])
        for r in (a, b):
            del r["frames"][0]["output_depth"]
        code, _, err = self.run_diff(a, b)
        self.assertEqual(code, 2)
        self.assertIn("output_depth", err)

        # An unrecognized marker is just as unusable as an absent one.
        a, b = record([frame("a", [0.5] * 3)]), record([frame("a", [0.5] * 3)])
        for r in (a, b):
            r["frames"][0]["output_depth"] = "u12"
        code, _, err = self.run_diff(a, b)
        self.assertEqual(code, 2)
        self.assertIn("expected one of u8, u10, u16, f32", err)

    def test_non_numeric_measurements_are_refused_not_tracebacked(self):
        # `mean: ["x", 0, 0]` used to reach `round(y - x, 12)` and raise TypeError —
        # an uncaught traceback exiting 1, this tool's "comparison failed / invariant
        # broken" code, which reads to a caller as a determinism failure. A malformed
        # count was quieter and worse: `_number` coerced it to 0, making the clip half
        # of the verdict trivially equal.
        cases = {
            "mean member": ("mean", ["x", 0, 0], "three finite per-channel numbers"),
            "mean nan": ("mean", [float("nan"), 0, 0], "three finite per-channel numbers"),
            "clipped": ("clipped", "x", "non-numeric"),
            "total_samples": ("total_samples", None, "missing"),
            "clip_fraction bool": ("clip_fraction", True, "non-numeric"),
        }
        for label, (field, value, expect) in cases.items():
            a = record([frame("a", [0.1, 0.2, 0.3])])
            b = record([frame("a", [0.1, 0.2, 0.3])])
            for r in (a, b):
                r["frames"][0][field] = value
            with self.subTest(label=label):
                code, out, err = self.run_diff(a, b)
                self.assertEqual(code, 2, err)
                self.assertIn(expect, err)
                self.assertEqual(out, "", "a refused comparison writes no report")

    def test_a_record_without_a_usable_identity_is_refused(self):
        # `{}` == `{}`, so two unattributable records compared as "one build" and
        # returned success for a diff attributable to neither — contradicting this
        # format's own pipeline_version + commit + target premise.
        for label, mutate in (
            ("empty", lambda r: r.update(identity={})),
            ("absent", lambda r: r.pop("identity")),
            ("no target", lambda r: r["identity"].pop("target")),
            ("no pipeline_version", lambda r: r["identity"].pop("pipeline_version")),
            ("no nc_version", lambda r: r["identity"].pop("nc_version")),
        ):
            a = record([frame("a", [0.1, 0.2, 0.3])])
            b = record([frame("a", [0.1, 0.2, 0.3])])
            for r in (a, b):
                mutate(r)
            with self.subTest(label=label):
                code, _, err = self.run_diff(a, b)
                self.assertEqual(code, 2)
                self.assertIn("identity", err)

    def test_a_no_git_build_is_still_a_valid_record(self):
        # The other half of the identity check, and the reason it is NOT
        # `REQUIRED_REPORT["identity"]`: `git_commit`/`git_dirty` are legitimately
        # absent for a source-tarball build, and `params_hash` is per-frame and never
        # part of a record's identity at all. Requiring either would reject records
        # `compare run` itself writes.
        a = record([frame("a", [0.1, 0.2, 0.3])])
        b = record([frame("a", [0.1, 0.2, 0.3])])
        for r in (a, b):
            del r["identity"]["git_commit"]
            del r["identity"]["git_dirty"]
        self.assertIsNone(compare.validate_record(a, "a"))
        self.assertNotIn("params_hash", a["identity"])
        code, out, err = self.run_diff(a, b)
        self.assertEqual(code, 0, err)
        self.assertTrue(json.loads(out)["identical"])

    def test_a_cross_target_comparison_is_annotated(self):
        a = record([frame("a", [0.1, 0.2, 0.3])], target="aarch64-apple-darwin")
        b = record([frame("a", [0.1, 0.2, 0.3])], target="x86_64-unknown-linux-gnu")
        code, out, err = self.run_diff(a, b)
        self.assertEqual(code, 0, err)
        self.assertTrue(json.loads(out)["target_changed"])
        self.assertIn("target_changed", err)


class TestConvertCase(unittest.TestCase):
    """`convert_case` with a faked `nc`: proves the harness reads its numbers from
    the JSON report + telemetry record and never from pixels."""

    def fake_run(self, report: dict, telemetry: dict | None = None):
        def _run(argv, capture_output=True, text=True):
            if telemetry is not None:
                tel = argv[argv.index("--telemetry-file") + 1]
                with open(tel, "w", encoding="utf-8") as fh:
                    json.dump(telemetry, fh)
            return mock.Mock(returncode=0, stdout=json.dumps(report), stderr="")
        return _run

    def case(self, workdir, **kw):
        src = os.path.join(workdir, "in.tif")
        with open(src, "wb") as fh:
            fh.write(b"II*\0")
        base = dict(name="c1", input=src, recipe=None, args=[], expect_sha256=None,
                    input_sha256="beef", checksums="computed")
        base.update(kw)
        return base

    def test_reads_stats_loss_and_timings(self):
        with tempfile.TemporaryDirectory() as d:
            with mock.patch("subprocess.run",
                            self.fake_run(nc_report(), {"timing_ms": {"total": 12.5}})):
                entry, identity, err = compare.convert_case("nc", self.case(d), d)
        self.assertIsNone(err)
        self.assertEqual(entry["mean"], [0.25, 0.5, 0.75])
        self.assertEqual(entry["clipped"], 40)
        self.assertAlmostEqual(entry["clip_fraction"], 0.1)
        self.assertEqual(entry["timing_ms"], {"total": 12.5})
        self.assertEqual(entry["params_hash"], "feed")
        # The digest of the bytes actually converted rides the frame, so a later
        # `diff` can prove both builds read the same input.
        self.assertEqual(entry["input_sha256"], "beef")
        self.assertEqual(entry["checksums"], "computed")
        # The mean's units depend on the depth, so the depth is recorded with it.
        self.assertEqual(entry["output_depth"], "u16")
        # The build identity carries the binary's labels, NOT the per-frame hash.
        self.assertEqual(identity["pipeline_version"], 1)
        self.assertNotIn("params_hash", identity)

    def test_the_depth_marker_follows_the_container_not_the_recipe_knob(self):
        # The bug this pins: `output.depth` is the *knob*, and an atomic preset
        # ignores it. A `film-master` run reports `depth: "u16"` while writing
        # unclamped f32 and a mean in the f32 domain, so recording the knob labelled
        # the mean with units it is not in — defeating the one field whose job is to
        # stop `diff` subtracting incomparable means. The JPEG and AVIF presets are
        # further out still: their primaries are 8- and 10-bit, depths the knob
        # cannot even spell.
        for encoding, want in (("unclamped-linear-acescg-float-tiff", "f32"),
                               ("dual-dialect-gain-map-jpeg", "u8"),
                               ("rec2100-pq-10bit-444-avif", "u10"),
                               ("display-p3-u16-tiff", "u16")):
            with self.subTest(encoding=encoding), tempfile.TemporaryDirectory() as d:
                # `depth="u16"` throughout — the knob's default, which is exactly what
                # every one of these presets reports while resolving its own container.
                report = nc_report(depth="u16", encoding=encoding)
                with mock.patch("subprocess.run", self.fake_run(report)):
                    entry, _, err = compare.convert_case("nc", self.case(d), d)
                self.assertIsNone(err)
                self.assertEqual(entry["output_depth"], want)

    def test_a_pre_versioning_build_is_refused_loudly(self):
        # A build with no `identity`/`output_stats` would otherwise produce a record
        # full of nulls that diffs to "no change" — a quietly wrong comparison.
        for report in (dict(output_stats=dict(mean=[0.1, 0.2, 0.3]), loss={}),
                       dict(identity=dict(params_hash="x"), loss={}),
                       dict(loss={}),
                       {}):
            with tempfile.TemporaryDirectory() as d:
                with mock.patch("subprocess.run", self.fake_run(report)):
                    entry, _, err = compare.convert_case("nc", self.case(d), d)
            self.assertIsNone(entry)
            self.assertIn("predating core/conversion-versioning", err or "")

    def test_a_present_but_hollow_report_is_refused_field_by_field(self):
        # The blocks-only check passed `{"output_stats": {"mean": null}}` and a report
        # with NO `loss` block at all, then defaulted three of five verdict fields to
        # 0 — making the clip half of the verdict trivially equal.
        hollow = [
            ("null mean", {**nc_report(), "output_stats": {"mean": None}}),
            ("non-triple mean", {**nc_report(), "output_stats": {"mean": [0.1]}}),
            ("no loss", {k: v for k, v in nc_report().items() if k != "loss"}),
            ("partial loss", {**nc_report(), "loss": {"total_samples": 3}}),
            ("identity only", {"identity": {"nc_version": "0.1.0"}}),
            ("no params_hash", {**nc_report(),
                                "identity": {"nc_version": "0.1.0", "pipeline_version": 1,
                                             "target": "t"}}),
            ("no depth", {k: v for k, v in nc_report().items() if k != "output_render"}),
            ("unknown encoding", {**nc_report(), "output_render": {"encoding": "who-knows"}}),
        ]
        for label, report in hollow:
            with self.subTest(label=label), tempfile.TemporaryDirectory() as d:
                with mock.patch("subprocess.run", self.fake_run(report)):
                    entry, _, err = compare.convert_case("nc", self.case(d), d)
                self.assertIsNone(entry, label)
                self.assertIn("missing", err or "", label)

    def test_missing_telemetry_warns_but_still_records(self):
        # Timings are informational, so losing them must not sink the comparison.
        with tempfile.TemporaryDirectory() as d:
            err_buf = io.StringIO()
            with mock.patch("subprocess.run", self.fake_run(nc_report())), \
                 mock.patch.object(sys, "stderr", err_buf):
                entry, _, err = compare.convert_case("nc", self.case(d), d)
        self.assertIsNone(err)
        self.assertEqual(entry["timing_ms"], {})
        self.assertIn("no telemetry record", err_buf.getvalue())


class TestRunCommand(unittest.TestCase):
    """`cmd_run` with a faked `nc`: the abort paths, which are the ones whose failure
    would silently produce an unattributable or mis-attributed record."""

    def args(self, tmp, **kw):
        base = dict(benchmark=compare.BENCHMARK, set_name="fixtures",
                    asset_root=os.path.join(tmp, "assets"), nc=sys.executable,
                    out=None, skip_checksums=False)
        base.update(kw)
        return mock.Mock(**base)

    def call(self, args, report=None, per_call=None):
        """Run `cmd_run` with `subprocess.run` faked, returning `(code, stdout, stderr)`."""
        calls = {"n": 0}

        def _run(argv, capture_output=True, text=True):
            i = calls["n"]
            calls["n"] += 1
            doc = per_call[i] if per_call else report
            return mock.Mock(returncode=0, stdout=json.dumps(doc), stderr="")

        out, err = io.StringIO(), io.StringIO()
        with mock.patch("subprocess.run", _run), \
             mock.patch.object(sys, "stdout", out), \
             mock.patch.object(sys, "stderr", err):
            code = compare.cmd_run(args)
        return code, out.getvalue(), err.getvalue()

    def test_the_committed_fixtures_set_runs_from_the_repo_root(self):
        # The `repo`-rooted branch: no assets, no manifest, so `compare` is runnable
        # and testable on any checkout. Also the only coverage of `cmd_run`'s record
        # assembly and its computed (unverifiable) checksums.
        with tempfile.TemporaryDirectory() as tmp:
            out_path = os.path.join(tmp, "rec.json")
            code, _, err = self.call(self.args(tmp, out=out_path), report=nc_report())
            self.assertEqual(code, 0, err)
            with open(out_path, encoding="utf-8") as fh:
                rec = json.load(fh)
        self.assertEqual(rec["schema_version"], compare.RECORD_SCHEMA)
        self.assertEqual(rec["benchmark_set"], "fixtures")
        self.assertTrue(rec["frames"])
        for fr in rec["frames"]:
            # A committed fixture has no manifest digest to verify against, so the
            # digest is *computed* — recorded either way, so `diff` can compare it.
            self.assertEqual(fr["checksums"], "computed")
            self.assertEqual(len(fr["input_sha256"]), 64)
        # And the record it wrote is one `diff` accepts.
        self.assertIsNone(compare.validate_record(rec, "rec"))

    def test_skip_checksums_is_recorded_and_warned_about(self):
        with tempfile.TemporaryDirectory() as tmp:
            out_path = os.path.join(tmp, "rec.json")
            code, _, err = self.call(self.args(tmp, out=out_path, skip_checksums=True),
                                     report=nc_report())
            self.assertEqual(code, 0, err)
            with open(out_path, encoding="utf-8") as fh:
                rec = json.load(fh)
        self.assertIn("--skip-checksums", err)
        for fr in rec["frames"]:
            self.assertEqual(fr["checksums"], "skipped")
            self.assertIsNone(fr["input_sha256"])

    def test_a_non_executable_nc_is_refused_before_any_work(self):
        with tempfile.TemporaryDirectory() as tmp:
            fake = os.path.join(tmp, "nc")
            with open(fake, "w", encoding="utf-8") as fh:
                fh.write("not a binary")
            os.chmod(fake, stat.S_IRUSR | stat.S_IWUSR)
            code, _, err = self.call(self.args(tmp, nc=fake), report=nc_report())
        self.assertEqual(code, 2)
        self.assertIn("not an executable nc binary", err)

    def test_checksum_drift_aborts_before_converting(self):
        # The record must never describe a benchmark over changed bytes.
        with tempfile.TemporaryDirectory() as tmp:
            root = os.path.join(tmp, "assets")
            os.makedirs(os.path.join(root, "rolls", "Ektar"))
            target = os.path.join(root, "rolls", "Ektar", "f1.tif")
            with open(target, "wb") as fh:
                fh.write(b"II*\0")
            with open(os.path.join(root, "manifest.json"), "w", encoding="utf-8") as fh:
                json.dump({"rolls": {"Ektar": {"frames": [
                    {"file": "rolls/Ektar/f1.tif", "sha256": "deadbeef"}]}}}, fh)
            bench = os.path.join(tmp, "bench.json")
            with open(bench, "w", encoding="utf-8") as fh:
                json.dump({"schema_version": 1, "sets": {"rolls": {"root": "assets", "cases": [
                    {"name": "r1", "roll": "Ektar", "frame": "f1"}]}}}, fh)
            code, _, err = self.call(
                self.args(tmp, benchmark=bench, set_name="rolls"), report=nc_report())
        self.assertEqual(code, 1)
        self.assertIn("input checksum drift", err)

    def test_cases_disagreeing_about_the_build_abort_the_record(self):
        # A record must describe exactly ONE build, or the comparison axis is
        # meaningless. Two cases reporting different identities is that failure.
        bench_cases = [{"name": "f1", "input": "tests/fixtures/hdri-64bit.tif"},
                       {"name": "f2", "input": "tests/fixtures/hdr-48bit.tif"}]
        with tempfile.TemporaryDirectory() as tmp:
            bench = os.path.join(tmp, "bench.json")
            with open(bench, "w", encoding="utf-8") as fh:
                json.dump({"schema_version": 1,
                           "sets": {"two": {"root": "repo", "cases": bench_cases}}}, fh)
            second = nc_report()
            second["identity"]["git_commit"] = "different"
            code, _, err = self.call(self.args(tmp, benchmark=bench, set_name="two"),
                                     per_call=[nc_report(), second])
        self.assertEqual(code, 1)
        self.assertIn("different build identity", err)

    def test_an_unwritable_out_path_is_a_message_not_a_traceback(self):
        with tempfile.TemporaryDirectory() as tmp:
            bad = os.path.join(tmp, "no-such-dir", "rec.json")
            code, _, err = self.call(self.args(tmp, out=bad), report=nc_report())
        self.assertEqual(code, 2)
        self.assertIn("cannot write", err)

    def test_a_failed_run_leaves_no_partial_out_file(self):
        # The record is written atomically, so a reader never sees a half-written
        # document that would parse (or fail) unpredictably.
        with tempfile.TemporaryDirectory() as tmp:
            out_path = os.path.join(tmp, "rec.json")
            hollow = {k: v for k, v in nc_report().items() if k != "loss"}
            code, _, err = self.call(self.args(tmp, out=out_path), report=hollow)
            self.assertEqual(code, 1, err)
            self.assertFalse(os.path.exists(out_path),
                             "a failed run must not leave a record behind")
            self.assertFalse([p for p in os.listdir(tmp) if ".tmp." in p])


class TestResolveCases(unittest.TestCase):
    BENCH = {
        "sets": {
            "fixtures": {"root": "repo",
                         "cases": [{"name": "f1", "input": "tests/fixtures/x.tif"}]},
            "rolls": {"root": "assets",
                      "cases": [{"name": "r1", "roll": "Ektar",
                                 "frame": "20260713-nikon-971"}]},
        }
    }

    def test_unknown_set_lists_what_exists(self):
        cases, err = compare.resolve_cases(self.BENCH, "nope", "/nonexistent")
        self.assertEqual(cases, [])
        self.assertIn("fixtures", err)
        self.assertIn("rolls", err)

    def test_asset_case_resolves_path_and_checksum_through_the_manifest(self):
        # The asset manifest is the ONE inventory: the benchmark names a frame stem
        # and gets back the path + sha256 recorded there.
        with tempfile.TemporaryDirectory() as root:
            os.makedirs(os.path.join(root, "rolls", "Ektar"))
            target = os.path.join(root, "rolls", "Ektar", "20260713-nikon-971.tif")
            with open(target, "wb") as fh:
                fh.write(b"II*\0")
            with open(os.path.join(root, "manifest.json"), "w", encoding="utf-8") as fh:
                json.dump({"rolls": {"Ektar": {"frames": [
                    {"file": "rolls/Ektar/20260713-nikon-971.tif", "sha256": "deadbeef"}
                ]}}}, fh)
            cases, err = compare.resolve_cases(self.BENCH, "rolls", root)
        self.assertIsNone(err)
        self.assertEqual(cases[0]["input"], target)
        self.assertEqual(cases[0]["expect_sha256"], "deadbeef")

    def test_missing_asset_manifest_says_how_to_fix_it(self):
        cases, err = compare.resolve_cases(self.BENCH, "rolls", "/nonexistent-root")
        self.assertEqual(cases, [])
        self.assertIn("manifest generate", err)

    def test_a_frame_absent_from_the_manifest_is_a_loud_error(self):
        with tempfile.TemporaryDirectory() as root:
            with open(os.path.join(root, "manifest.json"), "w", encoding="utf-8") as fh:
                json.dump({"rolls": {"Ektar": {"frames": []}}}, fh)
            cases, err = compare.resolve_cases(self.BENCH, "rolls", root)
        self.assertEqual(cases, [])
        self.assertIn("not in roll", err)

    def test_duplicate_case_names_are_refused_at_resolve_time(self):
        # Caught here, where the fix is a manifest edit, rather than converting twice
        # and writing a record `diff` will refuse.
        bench = {"sets": {"dupe": {"root": "repo", "cases": [
            {"name": "f1", "input": "tests/fixtures/hdri-64bit.tif"},
            {"name": "f1", "input": "tests/fixtures/hdr-48bit.tif"},
        ]}}}
        cases, err = compare.resolve_cases(bench, "dupe", "/nonexistent")
        self.assertEqual(cases, [])
        self.assertIn("duplicate case name", err)

    def test_a_case_missing_its_key_fields_is_a_message_not_a_keyerror(self):
        # Every failure in this module is a message with an exit code; a bare
        # `case["roll"]` would traceback on the commonest kind of manifest typo.
        bench = {"sets": {
            "repo-bad": {"root": "repo", "cases": [{"name": "f1"}]},
            "assets-bad": {"root": "assets", "cases": [{"name": "r1", "frame": "x"}]},
        }}
        with tempfile.TemporaryDirectory() as root:
            with open(os.path.join(root, "manifest.json"), "w", encoding="utf-8") as fh:
                json.dump({"rolls": {}}, fh)
            for set_name, missing in (("repo-bad", "input"), ("assets-bad", "roll")):
                with self.subTest(set_name=set_name):
                    cases, err = compare.resolve_cases(bench, set_name, root)
                    self.assertEqual(cases, [])
                    self.assertIn(f"`{missing}`", err)


class TestShippedBenchmark(unittest.TestCase):
    def test_the_committed_benchmark_manifest_is_well_formed(self):
        # The manifest is data the harness depends on; a typo in it must fail here,
        # not 40 seconds into a real-scan run.
        bench, err = compare.load_json(compare.BENCHMARK)
        self.assertIsNone(err)
        self.assertEqual(bench["schema_version"], 1)
        names: list[str] = []
        for set_name, spec in bench["sets"].items():
            self.assertIn(spec["root"], ("repo", "assets"), set_name)
            self.assertTrue(spec["cases"], f"set {set_name} has no cases")
            for case in spec["cases"]:
                names.append(f"{set_name}/{case['name']}")
                if spec["root"] == "repo":
                    self.assertTrue(
                        os.path.isfile(os.path.join(compare.repo_root(),
                                                    case["input"])),
                        f"{set_name}/{case['name']}: input is not committed")
                else:
                    self.assertIn("roll", case)
                    self.assertIn("frame", case)
                if case.get("recipe"):
                    self.assertTrue(
                        os.path.isfile(os.path.join(compare.repo_root(),
                                                    case["recipe"])),
                        f"{set_name}/{case['name']}: recipe is not committed")
        self.assertEqual(len(names), len(set(names)), "duplicate case name")

    def test_a_malformed_container_is_a_message_not_an_attributeerror(self):
        # The recurring shape both review rounds surfaced: the guard lands on the
        # *leaf* while its *parent* is an unvalidated `Value`/dict, so a malformed
        # container reaches `.get()` on a non-dict and tracebacks instead of producing
        # this module's promised exit-coded message. Every JSON document here is
        # external input — an nc report, a telemetry record, a hand-editable benchmark
        # or asset manifest — so any parent can be the wrong type.
        gaps = compare._report_gaps({"output_render": [1]})
        self.assertTrue(any("output_render.encoding" in g for g in gaps), gaps)
        for hollow in ({"output_render": "x"}, {"output_render": {"preset": "legacy"}}):
            self.assertTrue(
                any("output_render.encoding" in g for g in compare._report_gaps(hollow)),
                hollow)

        # A non-dict `timing_ms` is informational data, so it degrades to "no
        # timings" rather than sinking the comparison — but never raises.
        self.assertEqual(compare._timing_delta(compare._dict(["a"]), {"a": 1.0}),
                         {"a": 1.0})

        for label, args in (
            ("sets is a list", ({"sets": []}, "x", "/tmp")),
            ("a set is a string", ({"sets": {"x": "str"}}, "x", "/tmp")),
            ("cases is not a list", ({"sets": {"x": {"cases": 3}}}, "x", "/tmp")),
            ("a case is a string",
             ({"sets": {"x": {"root": "repo", "cases": ["nope"]}}}, "x", "/tmp")),
        ):
            with self.subTest(label=label):
                cases, err = compare.resolve_cases(*args)
                self.assertEqual(cases, [])
                self.assertTrue(err)

        for label, args in (
            ("rolls is a list", ({"rolls": []}, "R", "f")),
            ("a roll is a string", ({"rolls": {"R": "s"}}, "R", "f")),
            ("frames is not a list", ({"rolls": {"R": {"frames": 3}}}, "R", "f")),
            ("a frame is a string", ({"rolls": {"R": {"frames": ["x"]}}}, "R", "f")),
        ):
            with self.subTest(label=label):
                fr, err = compare.find_frame(*args)
                self.assertIsNone(fr)
                self.assertTrue(err)

    def test_clip_fraction_handles_an_empty_an_f32_and_a_malformed_report(self):
        self.assertEqual(compare.clip_fraction({}), 0.0)
        self.assertEqual(compare.clip_fraction({"total_samples": 0}), 0.0)
        self.assertEqual(
            compare.clip_fraction({"total_samples": 100, "clipped_high": 25}), 0.25)
        # An explicit `null` must not raise out of the harness.
        self.assertEqual(
            compare.clip_fraction({"total_samples": 100, "clipped_high": None}), 0.0)


if __name__ == "__main__":
    unittest.main()
