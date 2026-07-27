"""Stdlib unit tests for `nctool.manifest` (run: `python3 -m unittest`).

Hermetic: every test builds a tiny synthetic asset tree of empty/near-empty
`.tif` files under a temp dir and a hand-written prior manifest — NO real assets,
never `nc` (metadata inspection is either stubbed or driven through a faked
`subprocess.run`). Focuses on the high-blast-radius logic: role/kind/note
preservation across renames, regenerable/encoding inference, source_frame
retarget, coverage gaps, schema handling, the `roles` triple contract, the
nc-parse-vs-rejection distinction in `inspect()`, and `validate`'s classification
+ exit-code contract (including the full-tree orphan and fail-on-missing-checksum
hardening).
"""
from __future__ import annotations

import argparse
import contextlib
import io
import json
import os
import stat
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from nctool import manifest  # noqa: E402


def fake_inspect(nc, path):
    """Deterministic stand-in for `manifest.inspect`. Bit depth keys off a `b32`
    marker in the filename so encoding-from-bits inference is exercised; format /
    ir_present mimic an nc-authoritative decode (no exiftool tag)."""
    name = os.path.basename(path)
    bits = 32 if "b32" in name else 16
    return dict(width=200, height=100, channels=4, bits=bits,
                format="silverfast-hdri", ir_present=True)


class Base(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.A = self._tmp.name
        self.addCleanup(self._tmp.cleanup)

    def put(self, rel, data=None):
        """Write a file under the asset root; return a manifest-shaped entry dict
        with a matching sha256 + bytes (so `validate` sees a clean checksum)."""
        if data is None:
            data = rel.encode()  # unique-per-path content
        p = os.path.join(self.A, rel)
        os.makedirs(os.path.dirname(p), exist_ok=True)
        with open(p, "wb") as f:
            f.write(data)
        return {"file": rel, "sha256": manifest.sha256(p), "bytes": len(data)}

    def write_manifest(self, m):
        with open(os.path.join(self.A, "manifest.json"), "w") as f:
            json.dump(m, f)

    def build(self, prev=None, reuse_hash=False):
        with mock.patch.object(manifest, "inspect", fake_inspect):
            return manifest.build_manifest(self.A, "fake-nc", reuse_hash, prev or {})

    def validate(self):
        args = argparse.Namespace(asset_root=self.A)
        out, err = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            rc = manifest.cmd_validate(args)
        return rc, out.getvalue(), err.getvalue()

    def roles(self):
        args = argparse.Namespace(asset_root=self.A)
        out, err = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            rc = manifest.cmd_roles(args)
        return rc, out.getvalue(), err.getvalue()


# --------------------------------------------------------------------- roles

class TestRoles(Base):
    def test_triple_emitted_for_complete_pair(self):
        self.write_manifest({"schema_version": 1, "rolls": {"RollA": {"frames": [
            {"file": "rolls/RollA/u.tif", "role": "unexposed"},
            {"file": "rolls/RollA/l.tif", "role": "leader"},
            {"file": "rolls/RollA/r1.tif", "role": "real"},
            {"file": "rolls/RollA/r2.tif", "role": "real"},
        ]}}})
        rc, out, _ = self.roles()
        self.assertEqual(rc, 0)
        self.assertEqual(out.strip(), "RollA|u.tif|l.tif|r1.tif r2.tif")

    def test_real_frame_ordering_preserved(self):
        self.write_manifest({"schema_version": 1, "rolls": {"RollA": {"frames": [
            {"file": "rolls/RollA/u.tif", "role": "unexposed"},
            {"file": "rolls/RollA/l.tif", "role": "leader"},
            {"file": "rolls/RollA/zzz.tif", "role": "real"},
            {"file": "rolls/RollA/aaa.tif", "role": "real"},
        ]}}})
        rc, out, _ = self.roles()
        self.assertEqual(rc, 0)
        self.assertTrue(out.strip().endswith("|zzz.tif aaa.tif"))

    def test_all_real_roll_skipped(self):
        # A roll with no unexposed/leader (e.g. the NLP source) emits no triple.
        self.write_manifest({"schema_version": 1, "rolls": {"NlpSrc": {"frames": [
            {"file": "rolls/NlpSrc/a.tif", "role": "real"},
            {"file": "rolls/NlpSrc/b.tif", "role": "real"},
        ]}}})
        rc, out, err = self.roles()
        self.assertEqual(rc, 1)
        self.assertEqual(out.strip(), "")
        self.assertIn("skipping roll", err)

    def test_mixed_rolls_emit_only_complete(self):
        self.write_manifest({"schema_version": 1, "rolls": {
            "RollA": {"frames": [
                {"file": "rolls/RollA/u.tif", "role": "unexposed"},
                {"file": "rolls/RollA/l.tif", "role": "leader"},
                {"file": "rolls/RollA/r1.tif", "role": "real"},
            ]},
            "NlpSrc": {"frames": [{"file": "rolls/NlpSrc/a.tif", "role": "real"}]},
        }})
        rc, out, _ = self.roles()
        self.assertEqual(rc, 0)
        self.assertEqual(out.strip(), "RollA|u.tif|l.tif|r1.tif")

    def test_unknown_role_warns_and_folds_into_real(self):
        # Item 6a: a typo'd role must not silently vanish — warn loudly, count real.
        self.write_manifest({"schema_version": 1, "rolls": {"RollA": {"frames": [
            {"file": "rolls/RollA/u.tif", "role": "unexposed"},
            {"file": "rolls/RollA/l.tif", "role": "leader"},
            {"file": "rolls/RollA/r1.tif", "role": "real"},
            {"file": "rolls/RollA/x.tif", "role": "leadr"},  # typo
        ]}}})
        rc, out, err = self.roles()
        self.assertEqual(rc, 0)
        self.assertIn("unrecognized role 'leadr'", err)
        # folded into real, not dropped:
        self.assertEqual(out.strip(), "RollA|u.tif|l.tif|r1.tif x.tif")

    def test_no_manifest_is_operational_error(self):
        rc, _, err = self.roles()
        self.assertEqual(rc, 2)
        self.assertIn("no manifest.json", err)

    def test_pair_with_no_real_frames_skipped(self):
        # A complete unexposed+leader pair but no `real` frames must NOT emit
        # `RollA|u|l|` (empty 4th field) — the harness convert/ir stages would run
        # over an empty list. Skip it, and with nothing emitted exit 1.
        self.write_manifest({"schema_version": 1, "rolls": {"RollA": {"frames": [
            {"file": "rolls/RollA/u.tif", "role": "unexposed"},
            {"file": "rolls/RollA/l.tif", "role": "leader"},
        ]}}})
        rc, out, err = self.roles()
        self.assertEqual(rc, 1)
        self.assertEqual(out.strip(), "")
        self.assertIn("no real frames", err)

    def test_ir_capable_real_frame_ordered_first(self):
        # The harness IR stage takes the first real frame; ensure an IR-capable
        # (ir_present) frame leads even when it sorts later by name.
        self.write_manifest({"schema_version": 1, "rolls": {"RollA": {"frames": [
            {"file": "rolls/RollA/u.tif", "role": "unexposed"},
            {"file": "rolls/RollA/l.tif", "role": "leader"},
            {"file": "rolls/RollA/aaa_hdr.tif", "role": "real", "ir_present": False},
            {"file": "rolls/RollA/zzz_hdri.tif", "role": "real", "ir_present": True},
        ]}}})
        rc, out, _ = self.roles()
        self.assertEqual(rc, 0)
        self.assertEqual(out.strip(), "RollA|u.tif|l.tif|zzz_hdri.tif aaa_hdr.tif")


# --------------------------------------------------------------- load_manifest

class TestLoadManifest(Base):
    def test_missing_file_ok_empty(self):
        data, err = manifest.load_manifest(os.path.join(self.A, "nope.json"))
        self.assertEqual((data, err), ({}, None))

    def test_bad_json_error(self):
        p = os.path.join(self.A, "manifest.json")
        with open(p, "w") as f:
            f.write("{ this is not json")
        data, err = manifest.load_manifest(p)
        self.assertEqual(data, {})
        self.assertIsNotNone(err)
        self.assertIn("not valid JSON", err)

    def test_non_object_json_rejected(self):
        # Valid JSON that isn't an object ([], null, a scalar) must be a clean
        # error, not an AttributeError from `.get()` on a list/None.
        p = os.path.join(self.A, "manifest.json")
        for payload in ("[]", "null", "\"hi\"", "42"):
            with open(p, "w") as f:
                f.write(payload)
            data, err = manifest.load_manifest(p)
            self.assertEqual(data, {}, payload)
            self.assertIsNotNone(err, payload)
            self.assertIn("not a JSON object", err)

    def test_schema_v2_rejected(self):
        p = os.path.join(self.A, "manifest.json")
        self.write_manifest({"schema_version": 2, "rolls": {}})
        data, err = manifest.load_manifest(p)
        self.assertEqual(data, {})
        self.assertIn("unsupported schema_version 2", err)

    def test_schema_v1_and_missing_ok(self):
        p = os.path.join(self.A, "manifest.json")
        self.write_manifest({"schema_version": 1, "rolls": {"R": {"frames": []}}})
        data, err = manifest.load_manifest(p)
        self.assertIsNone(err)
        self.assertIn("rolls", data)
        # schema_version absent (None) is also accepted
        self.write_manifest({"rolls": {}})
        data, err = manifest.load_manifest(p)
        self.assertIsNone(err)


# --------------------------------------------------------------- build_manifest

class TestBuild(Base):
    def test_regenerable_inference_and_encoding_from_bits(self):
        # nc non-V0 → regenerable (no sha); V0 + nlp → hashed. Encoding from bits.
        self.put("converted/nc/2026-07-22/RollA/r1_b32_positive.tiff")
        self.put("converted/nc/V0/RollA/r1_corr.tif")          # 16-bit
        self.put("converted/nlp/2026-07-23/img1-positive.tif")  # 16-bit
        self.put("converted/nlp/2026-07-23/img2_b32-positive.tif")  # 32-bit
        m, _ = self.build()

        nc_new = m["converted"]["nc/2026-07-22"]
        self.assertTrue(nc_new["regenerable"])
        out = nc_new["outputs"][0]
        self.assertNotIn("sha256", out)              # regenerable → not hashed
        self.assertEqual(out["encoding"], "f32-linear")

        v0 = m["converted"]["nc/V0"]
        self.assertFalse(v0["regenerable"])
        self.assertIn("sha256", v0["outputs"][0])    # non-regenerable → hashed
        self.assertEqual(v0["outputs"][0]["encoding"], "u16-srgb")

        nlp = m["converted"]["nlp/2026-07-23"]
        self.assertFalse(nlp["regenerable"])
        encs = {os.path.basename(o["file"]): o["encoding"] for o in nlp["outputs"]}
        self.assertEqual(encs["img1-positive.tif"], "u16")
        self.assertEqual(encs["img2_b32-positive.tif"], "f32")

    def test_role_preserved_by_path(self):
        self.put("rolls/RollA/u.tif")
        prev = {"rolls": {"RollA": {"frames": [
            {"file": "rolls/RollA/u.tif", "role": "unexposed"}]}}}
        m, _ = self.build(prev=prev)
        self.assertEqual(m["rolls"]["RollA"]["frames"][0]["role"], "unexposed")

    def test_role_preserved_across_rename_by_checksum(self):
        # Old path gone, same bytes at a new path → role carried via sha256.
        new = self.put("rolls/RollA/renamed.tif", data=b"leaderbytes")
        prev = {"rolls": {"RollA": {"frames": [{
            "file": "rolls/RollA/old.tif", "role": "leader",
            "sha256": new["sha256"], "bytes": new["bytes"]}]}}}
        m, _ = self.build(prev=prev)
        fr = m["rolls"]["RollA"]["frames"][0]
        self.assertEqual(fr["file"], "rolls/RollA/renamed.tif")
        self.assertEqual(fr["role"], "leader")

    def test_role_not_cloned_to_copy_when_old_path_present(self):
        # A copy (old path still on disk) must NOT inherit the calibration role.
        orig = self.put("rolls/RollA/old.tif", data=b"leaderbytes")
        self.put("rolls/RollA/copy.tif", data=b"leaderbytes")  # identical bytes
        prev = {"rolls": {"RollA": {"frames": [{
            "file": "rolls/RollA/old.tif", "role": "leader",
            "sha256": orig["sha256"], "bytes": orig["bytes"]}]}}}
        m, _ = self.build(prev=prev)
        roles = {os.path.basename(f["file"]): f["role"]
                 for f in m["rolls"]["RollA"]["frames"]}
        self.assertEqual(roles["old.tif"], "leader")
        self.assertEqual(roles["copy.tif"], "real")  # default, not cloned

    def test_sample_kind_note_preserved(self):
        self.put("samples/foo.tif")
        prev = {"samples": [{"file": "samples/foo.tif", "kind": "perf-worst-case",
                             "note": "keepme"}]}
        m, _ = self.build(prev=prev)
        s = m["samples"][0]
        self.assertEqual(s["kind"], "perf-worst-case")
        self.assertEqual(s["note"], "keepme")

    def test_source_frame_resolution_and_coverage_gaps(self):
        # NLP source roll with two frames, only one has an NLP output → the other
        # is a coverage gap; the output's source_frame resolves by stem.
        self.put("rolls/Portra160-2026-07-22/img1.tif")
        self.put("rolls/Portra160-2026-07-22/img2.tif")
        self.put("converted/nlp/2026-07-23/img1-positive.tif")
        m, _ = self.build()
        out = m["converted"]["nlp/2026-07-23"]["outputs"][0]
        self.assertEqual(out["source_frame"], "rolls/Portra160-2026-07-22/img1.tif")
        self.assertEqual(len(m["coverage_gaps"]), 1)
        self.assertIn("img2.tif", m["coverage_gaps"][0])

    def test_source_frame_retarget_through_rename_map(self):
        # The source frame is renamed (old stem gone); a non-regenerable NLP output
        # keeps its nc↔source link by retargeting through the rename map.
        renamed = self.put("rolls/Portra160-2026-07-22/img1-NEW.tif", data=b"srcbytes")
        outp = self.put("converted/nlp/2026-07-23/legacy-positive.tif", data=b"outbytes")
        prev = {
            "rolls": {"Portra160-2026-07-22": {"frames": [{
                "file": "rolls/Portra160-2026-07-22/img1-OLD.tif", "role": "real",
                "sha256": renamed["sha256"], "bytes": renamed["bytes"]}]}},
            "converted": {"nlp/2026-07-23": {"producer": "nlp", "regenerable": False,
                "outputs": [{
                    "file": "converted/nlp/2026-07-23/legacy-positive.tif",
                    "source_frame": "rolls/Portra160-2026-07-22/img1-OLD.tif",
                    "sha256": outp["sha256"], "bytes": outp["bytes"]}]}},
        }
        m, _ = self.build(prev=prev)
        out = m["converted"]["nlp/2026-07-23"]["outputs"][0]
        self.assertEqual(out["source_frame"], "rolls/Portra160-2026-07-22/img1-NEW.tif")

    def test_reuse_hash_reuses_unchanged_size(self):
        e = self.put("samples/foo.tif", data=b"abcd")
        prev = {"samples": [{"file": "samples/foo.tif",
                             "sha256": "DEADBEEF", "bytes": e["bytes"]}]}
        m, _ = self.build(prev=prev, reuse_hash=True)
        # same byte size → the (stale) prior sha is trusted verbatim
        self.assertEqual(m["samples"][0]["sha256"], "DEADBEEF")


# ------------------------------------------------------------------- inspect

def _completed(cmd, returncode, stdout=""):
    return subprocess.CompletedProcess(cmd, returncode, stdout=stdout, stderr="")


class TestInspect(unittest.TestCase):
    """Item 3: exit-0-but-unparseable is a loud error, distinct from rejection."""

    def _run_with(self, nc_rc, nc_stdout, exif_rc=0, exif_stdout=""):
        def fake_run(cmd, **kw):
            if cmd[0] == "exiftool":
                return _completed(cmd, exif_rc, exif_stdout)
            return _completed(cmd, nc_rc, nc_stdout)  # the nc call
        with mock.patch.object(manifest.subprocess, "run", fake_run):
            return manifest.inspect("nc", "/some/file.tif")

    def test_exit0_valid_json_is_authoritative(self):
        payload = json.dumps({"decode": {"width": 10, "height": 20, "channels": 4,
                                         "bits_per_sample": 16, "format": "hdri",
                                         "ir_present": True}})
        r = self._run_with(0, payload)
        self.assertEqual(r["width"], 10)
        self.assertEqual(r["format"], "hdri")
        self.assertNotIn("metadata_source", r)  # authoritative

    def test_exit0_unparseable_is_loud_error_not_exiftool(self):
        r = self._run_with(0, "this is not json", exif_stdout="99\n99\n16\n4")
        self.assertEqual(r.get("metadata_source"), "none")
        self.assertIn("error", r)
        self.assertNotEqual(r.get("metadata_source"), "exiftool")

    def test_exit0_missing_keys_is_loud_error(self):
        r = self._run_with(0, json.dumps({"decode": {"width": 1}}))
        self.assertEqual(r.get("metadata_source"), "none")
        self.assertIn("error", r)

    def test_nonzero_exit_falls_back_to_exiftool(self):
        # nc rejects (exit 1) → legitimate exiftool fallback (placeholders).
        r = self._run_with(1, "", exif_stdout="640\n480\n16\n3")
        self.assertEqual(r.get("metadata_source"), "exiftool")
        self.assertEqual(r["width"], 640)
        self.assertEqual(r["ir_present"], False)


# ------------------------------------------------------------------ validate

class TestValidate(Base):
    def _clean_manifest(self):
        """A minimal on-disk tree + matching manifest that validates clean."""
        f1 = self.put("rolls/RollA/u.tif"); f1["role"] = "unexposed"
        f2 = self.put("rolls/RollA/r1.tif"); f2["role"] = "real"
        s1 = self.put("samples/largest.tif"); s1["kind"] = "sample"
        # a regenerable output with no sha256 (legitimately unchecked)
        self.put("converted/nc/2026-07-22/RollA/r1_positive.tiff")
        m = {"schema_version": 1,
             "rolls": {"RollA": {"stock": "x", "frames": [f1, f2]}},
             "samples": [s1],
             "converted": {"nc/2026-07-22": {"producer": "nc", "regenerable": True,
                 "outputs": [{"file": "converted/nc/2026-07-22/RollA/r1_positive.tiff"}]}},
             "coverage_gaps": []}
        return m

    def test_clean_tree_exit0(self):
        self.write_manifest(self._clean_manifest())
        rc, out, _ = self.validate()
        self.assertEqual(rc, 0, out)
        self.assertIn("OK", out)
        self.assertIn("unchecked: 1", out)  # the regenerable output, not a problem

    def test_full_tree_orphans(self):
        # Item 1: a deeply-nested image and a root-level stray are both orphans,
        # even though the structured generate walkers would never see them.
        m = self._clean_manifest()
        self.write_manifest(m)
        self.put("samples/icc/sub/x.tif")  # 2 levels deep under samples
        self.put("stray.tif")              # asset-root level
        rc, out, _ = self.validate()
        self.assertEqual(rc, 1)
        self.assertIn("ORPHANS", out)
        self.assertIn("samples/icc/sub/x.tif", out)
        self.assertIn("stray.tif", out)

    def test_companions_not_orphaned(self):
        # A stem-matched .json/.jpg sidecar is not an image → never an orphan.
        m = self._clean_manifest()
        self.write_manifest(m)
        with open(os.path.join(self.A, "samples/largest.tif.json"), "w") as f:
            f.write("{}")
        with open(os.path.join(self.A, "samples/largest.jpg"), "wb") as f:
            f.write(b"jpg")
        rc, out, _ = self.validate()
        self.assertEqual(rc, 0, out)

    def test_missing_and_drift(self):
        m = self._clean_manifest()
        self.write_manifest(m)
        # drift: edit r1.tif in place
        with open(os.path.join(self.A, "rolls/RollA/r1.tif"), "wb") as f:
            f.write(b"changed-bytes-different-size")
        # missing: remove a recorded sample
        os.remove(os.path.join(self.A, "samples/largest.tif"))
        rc, out, _ = self.validate()
        self.assertEqual(rc, 1)
        self.assertIn("DRIFT", out)
        self.assertIn("rolls/RollA/r1.tif", out)
        self.assertIn("MISSING", out)
        self.assertIn("samples/largest.tif", out)

    def test_error_entry_is_problem(self):
        # Item 2a: an entry carrying an inspection error → problem/exit 1.
        m = self._clean_manifest()
        self.put("samples/broken.tif")
        m["samples"].append({"file": "samples/broken.tif", "error": "boom",
                             "metadata_source": "none"})
        self.write_manifest(m)
        rc, out, _ = self.validate()
        self.assertEqual(rc, 1)
        self.assertIn("ERRORS", out)
        self.assertIn("samples/broken.tif", out)

    def test_non_regenerable_missing_sha_is_problem(self):
        # Item 2b: a non-regenerable entry lacking sha256 → problem/exit 1,
        # NOT silently 'unchecked'.
        m = self._clean_manifest()
        # add a non-regenerable NLP output on disk but with no recorded sha256
        self.put("converted/nlp/2026-07-23/x-positive.tif")
        m["converted"]["nlp/2026-07-23"] = {"producer": "nlp", "regenerable": False,
            "outputs": [{"file": "converted/nlp/2026-07-23/x-positive.tif"}]}
        self.write_manifest(m)
        rc, out, _ = self.validate()
        self.assertEqual(rc, 1)
        self.assertIn("NO CHECKSUM", out)
        self.assertIn("converted/nlp/2026-07-23/x-positive.tif", out)

    def test_no_manifest_is_exit2(self):
        rc, _, err = self.validate()
        self.assertEqual(rc, 2)
        self.assertIn("no manifest.json", err)

    def test_bad_schema_is_exit2(self):
        self.write_manifest({"schema_version": 2, "rolls": {}})
        rc, _, err = self.validate()
        self.assertEqual(rc, 2)
        self.assertIn("unsupported schema_version", err)


class TestWriteManifest(Base):
    def test_preserves_existing_mode(self):
        # Replacing a group/world-readable manifest (e.g. 0664 on a shared Drive
        # folder) must not silently tighten it to mkstemp's 0600.
        p = os.path.join(self.A, "manifest.json")
        self.write_manifest({"schema_version": 1, "rolls": {}})
        os.chmod(p, 0o664)
        manifest.write_manifest(p, {"schema_version": 1, "rolls": {"R": {"frames": []}}})
        self.assertEqual(stat.S_IMODE(os.stat(p).st_mode), 0o664)

    def test_new_file_honors_umask_not_0600(self):
        p = os.path.join(self.A, "new.json")
        manifest.write_manifest(p, {"schema_version": 1})
        umask = os.umask(0)
        os.umask(umask)
        self.assertEqual(stat.S_IMODE(os.stat(p).st_mode), 0o666 & ~umask)


if __name__ == "__main__":
    unittest.main()
