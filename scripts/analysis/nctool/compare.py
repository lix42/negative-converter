"""Convert a fixed benchmark set and diff two builds' results.

The comparison half of `core/conversion-versioning`. Two commands:

- `run`  — convert every case in a benchmark **set** with one `nc` binary and
  write a **run record**: the build's identity (`nc_version`, git commit,
  `pipeline_version`) plus, per frame, the `params_hash`, the per-channel mean of
  the written samples, the clip fraction, the input's sha256, the output depth, and
  the per-stage timings.
- `diff` — diff two run records into a **version-keyed** report: per-channel mean
  ΔRGB, clip-fraction delta, and timing delta per frame.

**Why the record holds numbers and not pixels.** `nc`'s JSON report already
carries `output_stats.mean` (the per-channel mean of the samples as written) and
`loss` (clip / non-finite counts), so a mean ΔRGB between two builds is exactly
the difference of two recorded means — no output needs to be re-read, registered,
or shipped, and the hard rule that only derived numbers leave these tools
(`docs/progress/analysis.md`) holds by construction. Richer metrics (ΔE2000,
SSIM) need real pixel access and belong to the QA harness (design-spec §12
item 7), deliberately not here.

**What "zero diff" means.** Re-running the *same* build must produce an
`identical: true` diff. That verdict covers the **deterministic** fields only —
`params_hash`, the channel means, and the clip counts. Wall-clock timings are
*informational*: they differ between any two runs, so folding them into the
verdict would make a zero diff impossible. `diff` reports them separately.

**What `identical` cannot see.** The means are *signed per-channel averages*, so a
pixel permutation, or two changes that exactly compensate, read as identical. The
field name is stronger than the measurement; treat `identical: true` as "no
difference in the recorded statistics", not "the same image".

**Comparability is checked, not assumed.** `mean` is written by two different
functions depending on the depth of the artifact actually produced — quantized to
`[0, 1]` for integer output, verbatim and unclamped for f32 — so a u16-vs-f32
comparison would report a *unit* change as a rendering regression. That depth comes
from the report's `output_render.encoding`, not from the `output.depth` knob: an
atomic preset resolves its own container, so the knob says `u16` on a `film-master`
run that writes f32 and on the 8-bit JPEG and 10-bit AVIF presets alike.
`identity.target` is likewise a real axis: transcendental
libm results and the lcms2 transform differ by target (design-spec §8), so a
cross-target diff must be read as such. `diff` surfaces `output_depth_changed`,
`target_changed`, and `pipeline_version_changed` rather than leaving the caveat to
the reader — and it **refuses** a record that omits the `output_depth` marker, since an
absent marker equals an absent marker and would silently permit the one comparison
the marker exists to prevent.

**Asset identity comes from the asset manifest, not from here.** A roll case names
a `roll` + `frame` stem; the path and its `sha256` are resolved through
`<asset-root>/manifest.json` (`nctool manifest`), so there is exactly one asset
inventory. A checksum mismatch fails loudly — comparing two builds over silently
different input bytes would attribute an asset change to the code. The digest of
the bytes actually converted is recorded per frame (and re-checked by `diff`), so
that guarantee is verifiable from the artifacts alone rather than only from the
run's exit code. `--skip-checksums` is recorded too, as `checksums: "skipped"`.

Timings are read from the **telemetry** record (`--telemetry-file`), reusing
`telemetry/perf-telemetry` rather than adding a second measurement path.

**Exit codes.** These deliberately differ from the sibling `manifest` group's
"0 clean / 1 discrepancies / 2 operational" (`docs/progress/analysis.md`), because
a *discrepancy* is this tool's normal answer, not a fault:

- `0` — the comparison ran; its verdict (`identical` true or false) is the report.
  Two different builds rendering differently is the expected outcome and must not
  look like a failure to a shell.
- `1` — the comparison itself failed or proved a broken invariant: a case would not
  convert, an input's bytes drifted from the manifest, the cases disagreed about
  which build ran them, or one build produced two different results (a determinism
  violation). **The determinism claim is precondition-guarded**: `diff` may blame the
  pipeline only after ruling out every other explanation for the difference — see
  `determinism_blockers`. When a precondition fails, the honest outcome is a non-zero
  diff that is *not* a determinism claim, so it stays rc `0` with a
  `determinism_check_blocked` note naming what to fix.
- `2` — operational / usage: unreadable or malformed inputs, an unknown benchmark
  set, a missing asset manifest, or two records that cannot be compared.
"""
from __future__ import annotations

import hashlib
import json
import math
import os
import subprocess
import sys
import tempfile

# The benchmark manifest lives beside the tool (repo-relative), not at the asset
# root: it names *cases* (frame + recipe + flags), which are code-versioned
# decisions, while the asset root owns the inventory of bytes.
BENCHMARK = os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
                         "benchmark.json")

# The run-record schema this build writes and the only one `diff` accepts. A record
# from another version may not carry the fields the verdict reads, and defaulting a
# missing field would make the comparison quietly trivial — the exact failure mode
# `identical` must never have.
#
# v2: the frame field `output_hdr` (bool) became `output_depth` (`u8`/`u10`/`u16`/
# `f32`), following nc's `output.hdr` -> `output.depth` rename and, more importantly,
# a change of *meaning*: it now records the primary container's real depth rather
# than a recipe knob the atomic presets ignore. A v1 record carries neither the field
# nor the meaning, so it must be rejected as an unsupported schema — leaving this at
# 1 made an old record advertise the same version while failing later as merely
# "malformed", which hides why.
RECORD_SCHEMA = 2

# Fields whose equality decides `identical` — see the module docstring on why
# timings are excluded. Every one must be *present* on every frame: `None == None`
# is not agreement, it is two absent measurements.
DETERMINISTIC = ("params_hash", "mean", "clipped", "non_finite", "total_samples")

# Everything a frame entry must carry for `diff` to be able to read it.
#
# `output_depth` is required, not optional. It is the marker that says which set of
# incompatible units `mean` is in (see the module docstring), and a *missing* marker
# compares equal to a missing marker — so two records that both omitted it would have
# their u16 and f32 means subtracted, or declared identical, which is exactly the
# comparison the marker exists to prevent. Refusing an older record is the only safe
# reading of "the depth is unknown".
#
# `checksums` is required for the mirror-image reason: the diff derives an
# *affirmative* claim from it (`checksums_skipped: false`). A frame that omitted the
# field made no frame count as skipped, so a comparison over unverified — or
# genuinely different — input bytes was reported identical **and** attested to have
# been verified. A false attestation in the artifact whose purpose is attribution is
# worse than an omission, so a record that cannot substantiate the claim is refused.
FRAME_FIELDS = DETERMINISTIC + ("name", "clip_fraction", "output_depth", "checksums")

# Output path suffixes a benchmark case may ask for. nc validates the suffix against
# the resolved preset, so this is the set its containers accept — not a free string.
OUTPUT_EXTENSIONS = ("tiff", "tif", "jpg", "jpeg", "avif")

# The output depths a record may carry — the depth of the **primary** artifact each
# preset actually writes, which for the JPEG and AVIF presets is neither of the two
# values `output.depth` can hold.
OUTPUT_DEPTHS = ("u8", "u10", "u16", "f32")

# The primary artifact's depth per `output_render.encoding` identifier.
#
# **Not `recipe.output.depth`.** That is the *knob*, and an atomic preset ignores it:
# a `film-master` run reports `depth: "u16"` while writing unclamped f32 and a mean in
# the f32 domain, so recording the knob labelled the mean with the wrong units —
# defeating the one field whose whole job is to stop `diff` subtracting incomparable
# means. `output_render.encoding` is the report's container-truthful identifier (nc
# derives it from the resolved preset, the same source as `OutputParams::
# primary_depth_label`), so it is right for every preset including the ones whose
# depth the recipe cannot express.
#
# An encoding this table does not know is **refused**, not guessed: a new preset that
# reaches `run` before this map does must stop the comparison rather than silently
# label its mean with some other preset's units.
PRIMARY_DEPTH_BY_ENCODING = {
    "dual-dialect-gain-map-jpeg": "u8",
    "legacy-ultra-hdr-v1-xmp-mpf-jpeg": "u8",
    "rec2100-pq-10bit-444-avif": "u10",
    "rec2100-hlg-10bit-444-avif": "u10",
    "rendered-u16-tiff": "u16",
    "display-p3-u16-tiff": "u16",
    "srgb-u16-tiff": "u16",
    "rec2100-pq-u16-tiff": "u16",
    "rec2100-hlg-u16-tiff": "u16",
    "transitional-rendered-float-tiff": "f32",
    "unclamped-linear-acescg-float-tiff": "f32",
    "display-linear-bt2020-float-tiff": "f32",
}

# How a frame's input bytes were accounted for.
CHECKSUM_MODES = ("verified", "computed", "skipped")

# The modes that assert a digest was actually computed, and so must carry one.
# `skipped` legitimately carries `input_sha256: null` — that is the honest state, and
# the diff surfaces it as a caveat rather than a silent pass.
CHECKSUM_MODES_WITH_DIGEST = ("verified", "computed")

# Frame fields that must be real numbers, not merely present. A string here reaches
# the arithmetic in `diff_frames` and raises `TypeError` — a traceback at exit 1,
# which is this tool's "comparison failed / invariant broken" code and reads to a
# caller as a determinism failure. `mean`'s three members are checked the same way.
NUMERIC_FRAME_FIELDS = ("clipped", "non_finite", "total_samples", "clip_fraction")

# Identity fields a *run record* must carry to be attributable at all. Deliberately
# **not** `REQUIRED_REPORT["identity"]`: that describes an `nc` report, whereas a
# record's identity is `_build_identity`'s output, which drops `params_hash` on
# purpose (it is per-frame — a benchmark set spans several recipes). `git_commit` /
# `git_dirty` stay optional because a no-git build legitimately omits them, and
# `pins_source` already refuses to draw determinism conclusions without them.
REQUIRED_RECORD_IDENTITY = ("nc_version", "pipeline_version", "target")

# Report blocks `run` needs, with the sub-fields the verdict actually reads. A build
# predating `core/conversion-versioning` has none of them; a *malformed* report may
# have the block and not the field, which is just as unusable and must be as loud.
REQUIRED_REPORT = {
    "identity": ("params_hash", "pipeline_version", "nc_version", "target"),
    "output_stats": ("mean",),
    "loss": ("total_samples", "clipped_low", "clipped_high", "non_finite"),
}


def repo_root() -> str:
    """The repository root (three levels up from `scripts/analysis/nctool/`)."""
    here = os.path.dirname(os.path.abspath(__file__))
    return os.path.dirname(os.path.dirname(os.path.dirname(here)))


def load_json(path: str) -> tuple[dict | None, str | None]:
    """Read a JSON **object**, returning `(data, error)` — never raising, so every
    failure becomes a loud exit-coded message rather than a traceback.

    `UnicodeDecodeError` is caught explicitly: without it, pointing this at a TIFF
    (an easy `before`/`after` mix-up) tracebacks instead of reporting a bad file.
    """
    try:
        with open(path, encoding="utf-8") as fh:
            data = json.load(fh)
    except FileNotFoundError:
        return None, f"no such file: {path}"
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as e:
        return None, f"cannot read {path}: {e}"
    if not isinstance(data, dict):
        return None, f"{path}: expected a JSON object, got {type(data).__name__}"
    return data, None


def sha256(path: str) -> str:
    """Streamed checksum — the file is never held in memory (scans are 50-160 MB)."""
    h = hashlib.sha256()
    with open(path, "rb") as fh:
        for chunk in iter(lambda: fh.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def _dict(value, default: dict | None = None) -> dict:
    """`value` when it is a dict, else `default` (empty).

    Every JSON document this module reads is external input — an `nc` report, a
    telemetry record, a hand-editable benchmark or asset manifest — so a *container*
    can be the wrong type just as easily as a leaf. Chained `x.get("a").get("b")`
    raises `AttributeError` on a non-dict parent, which is a traceback rather than the
    exit-coded message this module promises; funnelling parents through here turns a
    malformed container into the same "missing" report a malformed leaf gets.
    """
    return value if isinstance(value, dict) else (default if default is not None else {})


def find_frame(assets: dict, roll: str, frame: str) -> tuple[dict | None, str | None]:
    """Look a `roll`/`frame`-stem case up in the asset manifest, returning its
    entry. The manifest is the single inventory: this never guesses a path."""
    r = _dict(assets).get("rolls")
    r = _dict(r).get(roll)
    if not isinstance(r, dict):
        return None, f"roll {roll!r} is not in the asset manifest"
    frames = r.get("frames")
    if not isinstance(frames, list):
        return None, f"roll {roll!r} in the asset manifest has no `frames` list"
    for fr in frames:
        if not isinstance(fr, dict):
            return None, f"roll {roll!r} in the asset manifest has a non-object frame"
        if os.path.splitext(os.path.basename(str(fr.get("file", ""))))[0] == frame:
            return fr, None
    return None, f"frame {frame!r} is not in roll {roll!r} in the asset manifest"


def _need(case: dict, key: str, set_name: str, name: str) -> tuple[str | None, str | None]:
    """A required benchmark-case string field, or a message naming what is missing.
    A bare `case[key]` would raise `KeyError` and violate this module's own
    every-failure-is-a-message contract for the commonest kind of typo."""
    value = case.get(key)
    if not isinstance(value, str) or not value:
        return None, (f"case {name!r} in set {set_name!r} needs a non-empty `{key}` "
                      f"(got {value!r})")
    return value, None


def resolve_cases(bench: dict, set_name: str, asset_root: str,
                  ) -> tuple[list[dict], str | None]:
    """Resolve a benchmark set's cases into absolute inputs + recipes.

    A `repo`-rooted set (the committed `tests/fixtures/`) needs no assets, so
    `compare` is runnable — and testable — on any checkout. An `assets`-rooted set
    is resolved (and checksum-verified) through the asset manifest.
    """
    sets = _dict(bench).get("sets")
    if not isinstance(sets, dict):
        return [], "the benchmark manifest has no `sets` object"
    spec = sets.get(set_name)
    if spec is None:
        return [], (f"unknown benchmark set {set_name!r}; "
                    f"available: {', '.join(sorted(sets)) or '(none)'}")
    if not isinstance(spec, dict):
        return [], f"benchmark set {set_name!r} is not an object"
    root_kind = spec.get("root", "repo")
    assets: dict = {}
    if root_kind == "assets":
        assets, err = load_json(os.path.join(asset_root, "manifest.json"))
        if err:
            return [], (f"benchmark set {set_name!r} needs the asset manifest: {err}\n"
                        "Sync the nc-assets Drive folder and point ../nc-assets at it, "
                        "then run `python -m nctool manifest generate`.")

    cases = spec.get("cases", [])
    if not isinstance(cases, list):
        return [], f"benchmark set {set_name!r} has a non-list `cases`"
    out: list[dict] = []
    for case in cases:
        if not isinstance(case, dict):
            return [], f"a case in set {set_name!r} is not an object: {case!r}"
        name = case.get("name")
        if not name:
            return [], f"a case in set {set_name!r} has no `name`"
        if root_kind == "assets":
            roll, err = _need(case, "roll", set_name, name)
            if err:
                return [], err
            stem, err = _need(case, "frame", set_name, name)
            if err:
                return [], err
            fr, err = find_frame(assets, roll, stem)
            if err:
                return [], f"case {name!r}: {err}"
            path = os.path.join(asset_root, fr["file"])
            expect = fr.get("sha256")
        else:
            rel, err = _need(case, "input", set_name, name)
            if err:
                return [], err
            path = os.path.join(repo_root(), rel)
            # A committed fixture has no manifest entry, so there is no recorded
            # digest to verify against. `run` still records the digest it computes,
            # so `diff` can prove both builds read the same bytes.
            expect = None
        if not os.path.isfile(path):
            return [], f"case {name!r}: input not found: {path}"
        recipe = case.get("recipe")
        if recipe:
            recipe = os.path.join(repo_root(), recipe)
            if not os.path.isfile(recipe):
                return [], f"case {name!r}: recipe not found: {recipe}"
        # Defaults to `tiff` so every existing case is unchanged; a case selecting a
        # JPEG/AVIF preset states its own. Validated here rather than at use, so a
        # typo fails while resolving the set instead of mid-run.
        output_ext = case.get("output_ext", "tiff")
        if output_ext not in OUTPUT_EXTENSIONS:
            return [], (f"case {name!r}: output_ext {output_ext!r} is not one of "
                        f"{', '.join(sorted(OUTPUT_EXTENSIONS))}")
        out.append(dict(name=name, input=path, recipe=recipe, output_ext=output_ext,
                        args=list(case.get("args", [])), expect_sha256=expect))
    if not out:
        return [], f"benchmark set {set_name!r} has no cases"
    # A duplicate case name would produce two frames `diff` then matches by one name,
    # keeping only the last — so catch it here, where the fix is a manifest edit,
    # rather than letting an unusable record be written and refused later.
    names = [c["name"] for c in out]
    dupes = sorted({n for n in names if names.count(n) > 1})
    if dupes:
        return [], (f"benchmark set {set_name!r} has duplicate case name(s) "
                    f"{', '.join(map(repr, dupes))} — frames are keyed by name, so a "
                    "duplicate would be converted and then silently not compared")
    return out, None


def _is_number(value) -> bool:
    """Whether `value` is a finite real number. `bool` is excluded on purpose — it is
    an `int` subclass in Python, so `True` would otherwise pass as a count."""
    return (isinstance(value, (int, float)) and not isinstance(value, bool)
            and math.isfinite(value))


def _number(value, default: float = 0.0) -> float:
    """Coerce a recorded count to a number, treating an explicit `null` (or any
    non-number) as the default rather than raising. Field *presence* is validated
    separately and loudly; this only keeps a malformed value from surfacing as a
    `TypeError` traceback whose exit code (1) is indistinguishable from "the
    pipeline is not deterministic"."""
    return value if isinstance(value, (int, float)) and not isinstance(value, bool) else default


def clip_fraction(loss: dict) -> float:
    """Fraction of written samples the u16 quantizer had to clamp. 0.0 for an f32
    (unclamped) output, where `total_samples` is still reported but nothing clips."""
    total = _number(loss.get("total_samples"))
    if not total:
        return 0.0
    return (_number(loss.get("clipped_low")) + _number(loss.get("clipped_high"))) / total


def _report_gaps(report: dict) -> list[str]:
    """Which of the blocks/fields the comparison basis needs are missing from an `nc`
    report. Checks the *fields the verdict reads*, not just the blocks: a present
    `loss` with no `clipped_low` would otherwise default to 0 and make the clip half
    of the verdict trivially equal instead of loudly absent."""
    gaps: list[str] = []
    for block, fields in REQUIRED_REPORT.items():
        got = report.get(block)
        if not isinstance(got, dict) or not got:
            gaps.append(block)
            continue
        gaps += [f"{block}.{f}" for f in fields if got.get(f) is None]
    stats = report.get("output_stats")
    if isinstance(stats, dict):
        mean = stats.get("mean")
        if mean is not None and not (isinstance(mean, list) and len(mean) == 3
                                     and all(isinstance(v, (int, float)) for v in mean)):
            gaps.append("output_stats.mean (must be three numbers)")
    if _dict(report.get("output_render")).get("encoding") not in PRIMARY_DEPTH_BY_ENCODING:
        gaps.append("output_render.encoding (a known primary encoding)")
    return gaps


def convert_case(nc: str, case: dict, workdir: str,
                 ) -> tuple[dict | None, dict, str | None]:
    """Convert one case, returning `(frame entry, build identity, error)`.

    Runs with the default JSON report on stdout (the agent contract) and a one-off
    `--telemetry-file` for the per-stage timings. Output TIFFs land in `workdir`
    and are never read back — every number comes from the two JSON documents.

    The build identity is lifted straight out of the report's `identity` block
    (typed: `pipeline_version` is an integer, `git_dirty` a bool) rather than
    scraped from `--version` text, so the record and the report can't disagree.
    """
    # The suffix follows the case, because nc validates it against the resolved
    # preset and never renames the path. A case selecting a JPEG or AVIF preset would
    # otherwise fail the CLI suffix check before producing a report, making the
    # `u8`/`u10` entries in PRIMARY_DEPTH_BY_ENCODING unreachable.
    # `.get` with the same default `resolve_cases` applies: a hand-built case (the
    # unit tests) stays valid, while a real benchmark case is already validated
    # against OUTPUT_EXTENSIONS by the time it reaches here.
    out_path = os.path.join(workdir, f"{case['name']}.{case.get('output_ext', 'tiff')}")
    tel = os.path.join(workdir, f"{case['name']}.telemetry.json")
    argv = [nc, "convert", case["input"], "-o", out_path, "--telemetry-file", tel]
    if case["recipe"]:
        argv += ["--params", case["recipe"]]
    argv += case["args"]
    proc = subprocess.run(argv, capture_output=True, text=True)
    if proc.returncode != 0:
        return None, {}, (f"case {case['name']!r}: nc exited {proc.returncode}\n"
                          f"{proc.stderr.strip()}")
    try:
        report = json.loads(proc.stdout)
    except json.JSONDecodeError as e:
        return None, {}, f"case {case['name']!r}: nc stdout is not JSON ({e})"

    # Every field the comparison basis reads must be present. A build predating
    # `core/conversion-versioning` reports none of them, and recording nulls would
    # produce a diff full of `None` deltas that reads like "no change" — exactly the
    # quietly-wrong answer the fail-loudly rule forbids.
    gaps = _report_gaps(report)
    if gaps:
        return None, {}, (
            f"case {case['name']!r}: this nc report is missing {', '.join(gaps)} — a build "
            "predating core/conversion-versioning reports no identity/output_stats at all, "
            "so there is nothing to key a comparison on. Both builds in a comparison must "
            "stamp identity; compare against docs/reports/v0-baseline.md for anything older.")

    identity = report["identity"]
    stats = report["output_stats"]
    loss = report["loss"]
    timing: dict = {}
    record, err = load_json(tel)
    if err:
        # Timings are informational; losing them must not sink the comparison.
        print(f"warning: case {case['name']!r}: no telemetry record ({err})",
              file=sys.stderr)
    else:
        timing = _dict(_dict(record).get("timing_ms"))
    return dict(
        name=case["name"],
        input=os.path.basename(case["input"]),
        input_sha256=case.get("input_sha256"),
        checksums=case.get("checksums", "skipped"),
        params_hash=identity["params_hash"],
        # `mean`'s units follow the depth of the artifact actually written (see the
        # module docstring), so that depth rides with it — a mean is not comparable
        # without it. Taken from the resolved *encoding*, never from the
        # `output.depth` knob an atomic preset ignores. Reachable only because
        # `_report_gaps` already proved the chain is well-formed dicts ending in a
        # known encoding identifier.
        output_depth=PRIMARY_DEPTH_BY_ENCODING[report["output_render"]["encoding"]],
        mean=stats["mean"],
        clipped=_number(loss.get("clipped_low")) + _number(loss.get("clipped_high")),
        non_finite=_number(loss.get("non_finite")),
        total_samples=_number(loss.get("total_samples")),
        clip_fraction=clip_fraction(loss),
        timing_ms=timing,
    ), _build_identity(identity), None


def _build_identity(identity: dict) -> dict:
    """The build-level half of a report's `identity` block — everything that labels
    the *binary*, with the per-frame `params_hash` left behind (it belongs to the
    frame entry, and a benchmark set deliberately spans several recipes)."""
    return {k: identity.get(k) for k in
            ("nc_version", "git_commit", "git_dirty", "pipeline_version", "target")}


def pins_source(identity: dict) -> bool:
    """Whether an identity actually identifies the **source** that produced a record.

    Only a clean checkout does. Two builds from *different uncommitted trees at the
    same commit* produce byte-identical identity dicts (same commit, same version,
    same target, `git_dirty: true`), so identity equality alone does not mean "the
    same build" — and asserting determinism on it would call the most ordinary
    workflow there is (iterating on a change) a broken reproducibility contract.
    """
    return bool(identity.get("git_commit")) and identity.get("git_dirty") is False


def _verify_inputs(cases: list[dict], skip: bool) -> str | None:
    """Attach each case's input digest (and how it was obtained) in place, verifying
    it against the asset manifest where one exists. Runs BEFORE any conversion:
    comparing two builds over different input bytes would blame the code for an
    asset change."""
    for case in cases:
        if skip:
            case["input_sha256"], case["checksums"] = None, "skipped"
            continue
        try:
            got = sha256(case["input"])
        except OSError as e:
            return f"case {case['name']!r}: cannot checksum {case['input']}: {e}"
        expect = case["expect_sha256"]
        if expect and got != expect:
            return (f"case {case['name']!r}: input checksum drift\n"
                    f"  manifest: {expect}\n  on disk:  {got}\n"
                    "Re-run `python -m nctool manifest validate` — a benchmark "
                    "over changed bytes is not a build comparison.")
        case["input_sha256"] = got
        case["checksums"] = "verified" if expect else "computed"
    return None


def _write_out(path: str, text: str) -> str | None:
    """Write `text` to `path` atomically: a temp file in the same directory, then
    `os.replace`. A half-written record, or a stale one left looking current after a
    crash mid-write, would be read as a real measurement."""
    tmp = f"{path}.tmp.{os.getpid()}"
    try:
        with open(tmp, "w", encoding="utf-8") as fh:
            fh.write(text)
        os.replace(tmp, path)
    except OSError as e:
        try:
            os.unlink(tmp)
        except OSError:
            pass
        return f"cannot write {path}: {e}"
    return None


def cmd_run(args) -> int:
    """Convert a benchmark set with one `nc` build and write its run record."""
    bench, err = load_json(args.benchmark)
    if err:
        print(f"error: {err}", file=sys.stderr)
        return 2
    cases, err = resolve_cases(bench, args.set_name, args.asset_root)
    if err:
        print(f"error: {err}", file=sys.stderr)
        return 2

    if not os.path.isfile(args.nc) or not os.access(args.nc, os.X_OK):
        print(f"error: not an executable nc binary: {args.nc}", file=sys.stderr)
        return 2

    if err := _verify_inputs(cases, args.skip_checksums):
        print(f"error: {err}", file=sys.stderr)
        return 1
    if args.skip_checksums:
        print("warning: --skip-checksums: input bytes are unverified and the record "
              "will say so (checksums: skipped)", file=sys.stderr)

    frames: list[dict] = []
    identity: dict = {}
    with tempfile.TemporaryDirectory(prefix="nc-compare-") as workdir:
        for case in cases:
            entry, case_identity, err = convert_case(args.nc, case, workdir)
            if err:
                print(f"error: {err}", file=sys.stderr)
                return 1
            assert entry is not None
            # Every case ran through the same binary, so every case must report the
            # same build identity; a disagreement means the record is not
            # attributable to one build and the comparison axis is meaningless.
            if identity and case_identity != identity:
                print(f"error: case {entry['name']!r} reported a different build "
                      f"identity than earlier cases ({case_identity} vs {identity}) — "
                      "a run record must describe exactly one build",
                      file=sys.stderr)
                return 1
            identity = case_identity
            frames.append(entry)
            print(f"  {entry['name']}: mean={entry['mean']} "
                  f"clip={entry['clip_fraction']:.6f}", file=sys.stderr)

    record = dict(schema_version=RECORD_SCHEMA, benchmark_set=args.set_name,
                  identity=identity, frames=frames)
    text = json.dumps(record, indent=2, sort_keys=True) + "\n"
    if args.out:
        if err := _write_out(args.out, text):
            print(f"error: {err}", file=sys.stderr)
            return 2
        print(f"wrote {args.out} ({len(frames)} frames)", file=sys.stderr)
    else:
        sys.stdout.write(text)
    return 0


def validate_record(record: dict, label: str) -> str | None:
    """Whether `record` is a run record `diff` can actually read, or a message saying
    what it is not.

    Every check here exists because its absence made the verdict *quietly* wrong:
    two `{}` documents shared a (missing) benchmark set, had no frames to disagree
    about, and reported `identical: true` at exit 0 — as did re-diffing a diff
    report, whose rows carry deltas rather than measurements.
    """
    if record.get("schema_version") != RECORD_SCHEMA:
        return (f"{label}: schema_version is {record.get('schema_version')!r}, not "
                f"{RECORD_SCHEMA} — this is not a run record from a comparable "
                "`nctool compare run` (a diff report is not a run record)")
    if "benchmark_set" not in record:
        return f"{label}: no `benchmark_set` — a run record names the set it ran"

    # The comparison AXIS. Without it a diff is attributed to neither build, which
    # contradicts this format's whole premise ("keyed on pipeline_version + commit +
    # target") — and `{}` == `{}`, so two unattributable records used to compare
    # "equal" and return success.
    identity = record.get("identity")
    if not isinstance(identity, dict) or not identity:
        return (f"{label}: no `identity` block — a diff is only interpretable when it "
                "is attributed to a build, and two records with no identity would "
                "compare as one build and report success for nothing")
    missing = [f for f in REQUIRED_RECORD_IDENTITY if identity.get(f) is None]
    if missing:
        return (f"{label}: `identity` is missing {', '.join(missing)} — the record "
                "cannot be attributed to a build. (`git_commit`/`git_dirty` are "
                "legitimately absent for a no-git build and are not required here; "
                "`params_hash` is per-frame, never part of a record's identity.)")

    frames = record.get("frames")
    if not isinstance(frames, list) or not frames:
        return (f"{label}: no frames. An empty frame list has nothing to disagree "
                "about and would report `identical: true` having compared nothing")
    for i, frame in enumerate(frames):
        if not isinstance(frame, dict):
            return f"{label}: frames[{i}] is not an object"
        missing = [f for f in FRAME_FIELDS if frame.get(f) is None]
        if missing:
            return (f"{label}: frames[{i}] ({frame.get('name', '?')}) is missing "
                    f"{', '.join(missing)} — a missing field is not agreement, and "
                    "comparing two absent measurements would read as 'no change'")
        if not (isinstance(frame["mean"], list) and len(frame["mean"]) == 3
                and all(_is_number(v) for v in frame["mean"])):
            return (f"{label}: frames[{i}] ({frame['name']}) has mean="
                    f"{frame['mean']!r}, expected three finite per-channel numbers")
        bad = [f for f in NUMERIC_FRAME_FIELDS if not _is_number(frame[f])]
        if bad:
            return (f"{label}: frames[{i}] ({frame['name']}) has non-numeric "
                    + ", ".join(f"{f}={frame[f]!r}" for f in bad)
                    + " — a malformed count would be coerced to 0 and compare as "
                      "trivially equal, or reach the arithmetic and traceback")
        if frame["output_depth"] not in OUTPUT_DEPTHS:
            return (f"{label}: frames[{i}] ({frame['name']}) has output_depth="
                    f"{frame['output_depth']!r}, expected one of "
                    f"{', '.join(OUTPUT_DEPTHS)} — it decides which units `mean` is in")
        mode = frame["checksums"]
        if mode not in CHECKSUM_MODES:
            return (f"{label}: frames[{i}] ({frame['name']}) has checksums={mode!r}, "
                    f"expected one of {', '.join(CHECKSUM_MODES)} — the diff reports "
                    "`checksums_skipped` from this field, and an unreadable mode would "
                    "become the affirmative claim that verification happened")
        if mode in CHECKSUM_MODES_WITH_DIGEST and not frame.get("input_sha256"):
            return (f"{label}: frames[{i}] ({frame['name']}) claims checksums={mode!r} "
                    "but carries no `input_sha256` — the claim is unsubstantiated, and "
                    "a comparison cannot show both builds read the same input bytes")

    # `diff_frames` indexes frames BY NAME, so a duplicate silently keeps only the
    # last entry: two conversions get recorded and one gets compared, and if the
    # surviving pair happens to agree the whole thing reports `identical: true`.
    names = [f["name"] for f in frames]
    dupes = sorted({n for n in names if names.count(n) > 1})
    if dupes:
        return (f"{label}: duplicate frame name(s) {', '.join(map(repr, dupes))}. "
                "Frames are matched by name, so a duplicate would be silently "
                "dropped and its measurement never compared")
    return None


def diff_frames(a: dict, b: dict) -> tuple[list[dict], bool]:
    """Diff two run records' frame lists by case name.

    A case present in only one record is reported as such and counts as a
    difference — a silently dropped frame must not read as "no change".

    Assumes both records passed [`validate_record`], which is what makes the
    index-by-name safe: it guarantees every frame carries every field read here, that
    the numeric ones really are numbers, and that **names are unique** (a duplicate
    would collapse into one entry and take its measurement with it). The `_number`
    coercions below are therefore defense-in-depth, not the validation.
    """
    by_name_a = {f["name"]: f for f in a.get("frames", [])}
    by_name_b = {f["name"]: f for f in b.get("frames", [])}
    identical = True
    rows: list[dict] = []
    for name in sorted(set(by_name_a) | set(by_name_b)):
        fa, fb = by_name_a.get(name), by_name_b.get(name)
        if fa is None or fb is None:
            rows.append(dict(name=name, status="missing",
                             present_in=("b" if fa is None else "a")))
            identical = False
            continue
        mean_a, mean_b = fa.get("mean") or [], fb.get("mean") or []
        # `mean`'s units depend on the output depth, so a u16-vs-f32 delta would be a
        # unit conversion dressed up as a rendering change. Refuse to compute it.
        depth_changed = fa.get("output_depth") != fb.get("output_depth")
        d_mean = None
        if not depth_changed and len(mean_a) == len(mean_b):
            d_mean = [round(y - x, 12) for x, y in zip(mean_a, mean_b)]
        row = dict(
            name=name,
            status="ok",
            params_hash_changed=fa.get("params_hash") != fb.get("params_hash"),
            output_depth_changed=depth_changed,
            input_sha256_changed=_sha_changed(fa, fb),
            mean_delta_rgb=d_mean,
            clip_fraction_delta=round(_number(fb.get("clip_fraction"))
                                      - _number(fa.get("clip_fraction")), 12),
            clipped_delta=_number(fb.get("clipped")) - _number(fa.get("clipped")),
            non_finite_delta=_number(fb.get("non_finite")) - _number(fa.get("non_finite")),
            # Informational only — never folded into `identical`.
            timing_ms_delta=_timing_delta(_dict(fa.get("timing_ms")),
                                          _dict(fb.get("timing_ms"))),
        )
        rows.append(row)
        if depth_changed or any(fa.get(k) != fb.get(k) for k in DETERMINISTIC):
            identical = False
    return rows, identical


def _sha_changed(fa: dict, fb: dict) -> bool | None:
    """Whether the two runs converted different input bytes. `None` when at least one
    side skipped checksums and there is nothing to compare."""
    sa, sb = fa.get("input_sha256"), fb.get("input_sha256")
    if not sa or not sb:
        return None
    return sa != sb


def determinism_blockers(a: dict, b: dict) -> list[str]:
    """Every reason, other than the pipeline, that could explain a non-zero diff
    between two records — i.e. the reasons `diff` must **not** claim nondeterminism.
    Empty means every alternative explanation has been ruled out.

    **This is a precondition set, not an identity check, and that distinction is the
    whole point.** `compare` may blame the pipeline only when it has eliminated every
    other cause of a difference. Guarding on the build identity alone left several
    routes to the same false accusation — measured, all with the *same clean* identity
    on both sides:

    - differing per-frame `params_hash`: the two runs used **different recipes**, so of
      course the output differs. Blaming the pipeline is simply wrong.
    - `checksums: skipped` with no digest: `diff` printed a note conceding the input
      bytes were never verified and then contradicted itself two lines later.
    - differing `output_depth`: the means are in different units.
    - differing frame sets: the two runs did not convert the same work.

    Each was patched-then-rediscovered as a variant of one invariant, so the guard is
    now the precondition set rather than another special case.

    **If you add a field to the run record, ask whether it belongs here.** Anything
    that can independently change the recorded numbers is an alternative explanation,
    and omitting it from this set turns a real difference into a false accusation
    against the pipeline.
    """
    blockers: list[str] = []
    id_a = a.get("identity") or {}
    if not pins_source(id_a):
        blockers.append(
            f"the shared identity does not pin the source (git_commit="
            f"{id_a.get('git_commit')!r}, git_dirty={id_a.get('git_dirty')!r}), so two "
            "different uncommitted trees are indistinguishable — a non-zero diff is "
            "expected while iterating; compare two runs of one CLEAN checkout")

    fa, fb = a["frames"], b["frames"]
    names_a, names_b = [f["name"] for f in fa], [f["name"] for f in fb]
    if names_a != names_b:
        blockers.append(
            f"the frame sets differ ({names_a} vs {names_b}), so the two runs did not "
            "convert the same work")
        # Nothing below can be compared pairwise once the sets diverge.
        return blockers

    for x, y in zip(fa, fb):
        name = x["name"]
        if "skipped" in (x["checksums"], y["checksums"]):
            blockers.append(
                f"frame {name!r} ran with unverified input bytes (checksums: skipped), "
                "which cannot support a determinism claim — an asset change would be "
                "indistinguishable from a pipeline change")
        elif x.get("input_sha256") != y.get("input_sha256"):
            blockers.append(f"frame {name!r} converted different input bytes")
        if x["output_depth"] != y["output_depth"]:
            blockers.append(
                f"frame {name!r} was written at a different output depth, so its two "
                "means are in different units")
        if x["params_hash"] != y["params_hash"]:
            blockers.append(
                f"frame {name!r} ran a DIFFERENT RECIPE (params_hash "
                f"{x['params_hash']} vs {y['params_hash']}), so a different output is "
                "expected — this is not a pipeline fault")
    return blockers


def _determinism_confidence(a: dict) -> str:
    """What the nondeterminism claim has actually ruled out, for the error message. An
    accusation this serious should be able to say why it is confident."""
    id_a = a.get("identity") or {}
    n = len(a["frames"])
    return (f"same commit ({id_a.get('git_commit')}, clean tree), same target "
            f"({id_a.get('target')}), same pipeline_version "
            f"({id_a.get('pipeline_version')}), and across all {n} frame(s): identical "
            "input bytes by sha256, identical params_hash, identical output depth, and "
            "the same frame set in the same order")


def _timing_delta(a: dict, b: dict) -> dict:
    """Per-stage wall-clock delta, rounded. Present for information; two runs of one
    build always differ here, which is why it never decides `identical`."""
    return {k: round(_number(b.get(k)) - _number(a.get(k)), 3)
            for k in sorted(set(a) | set(b))}


def cmd_diff(args) -> int:
    """Diff two run records into a version-keyed comparison report on stdout."""
    a, err_a = load_json(args.before)
    b, err_b = load_json(args.after)
    for err in (err_a, err_b):
        if err:
            print(f"error: {err}", file=sys.stderr)
            return 2
    assert a is not None and b is not None
    for record, label in ((a, f"before ({args.before})"), (b, f"after ({args.after})")):
        if err := validate_record(record, label):
            print(f"error: {err}", file=sys.stderr)
            return 2
    if a["benchmark_set"] != b["benchmark_set"]:
        print(f"error: records are from different benchmark sets "
              f"({a['benchmark_set']!r} vs {b['benchmark_set']!r}) — "
              "there is nothing meaningful to diff", file=sys.stderr)
        return 2

    id_a, id_b = a.get("identity") or {}, b.get("identity") or {}
    rows, identical = diff_frames(a, b)
    changed_inputs = [r["name"] for r in rows if r.get("input_sha256_changed")]
    if changed_inputs:
        print("error: these cases converted DIFFERENT input bytes in the two runs: "
              f"{', '.join(changed_inputs)} — this is not a build comparison. "
              "Re-run `python -m nctool manifest validate` and re-run both sides.",
              file=sys.stderr)
        return 2

    skipped = [r for record in (a, b) for r in record["frames"]
               if r.get("checksums") == "skipped"]
    notes = []
    if id_a.get("target") != id_b.get("target"):
        notes.append("target_changed: the two records were produced on different "
                     "compile targets; nc's byte-identity contract is per "
                     "build/architecture (design-spec §8), so read every delta as a "
                     "cross-target comparison, not a behavior change")
    if id_a.get("pipeline_version") != id_b.get("pipeline_version"):
        notes.append("pipeline_version_changed: a difference here is expected and "
                     "attributable — that is what the label is for")
    if any(r.get("output_depth_changed") for r in rows):
        notes.append("output_depth_changed: `mean` is quantized to [0,1] for integer "
                     "output and verbatim/unclamped for f32, so the means are in "
                     "different units and mean_delta_rgb is withheld for those frames")
    if skipped:
        notes.append("checksums_skipped: at least one frame's input bytes were never "
                     "verified (--skip-checksums), so an asset change could be "
                     "misattributed to the code")

    # Whether a nondeterminism claim is even *available*, decided before the report is
    # written so the blocked reasons ride in `notes[]` rather than only on stderr.
    blockers: list[str] = []
    if id_a == id_b and not identical:
        blockers = determinism_blockers(a, b)
        notes += [f"determinism_check_blocked: {r}" for r in blockers]

    out = dict(
        schema_version=RECORD_SCHEMA,
        benchmark_set=a["benchmark_set"],
        # The comparison AXIS: a diff is only interpretable as a behavior change
        # when it is attributed to a pipeline_version + commit pair.
        before=id_a,
        after=id_b,
        target_changed=id_a.get("target") != id_b.get("target"),
        pipeline_version_changed=(id_a.get("pipeline_version")
                                  != id_b.get("pipeline_version")),
        checksums_skipped=bool(skipped),
        notes=notes,
        identical=identical,
        frames=rows,
    )
    sys.stdout.write(json.dumps(out, indent=2, sort_keys=True) + "\n")
    for note in notes:
        print(f"note: {note}", file=sys.stderr)

    # The pipeline may be blamed only when every OTHER explanation for the difference
    # has been ruled out — see `determinism_blockers` for why this is a precondition
    # set rather than an identity check. A blocked check is not a failure: it is a
    # non-zero diff that simply is not a determinism claim, so it keeps rc 0 (the
    # "verdict delivered" code) and says which precondition to fix.
    if id_a == id_b and not identical and not blockers:
        print("error: the SAME build produced a non-zero diff — the pipeline is not "
              "deterministic, which breaks the core reproducibility contract.\n"
              f"Ruled out: {_determinism_confidence(a)}.",
              file=sys.stderr)
        return 1
    if identical:
        print("identical: no deterministic difference between the two records",
              file=sys.stderr)
    else:
        print("differs: see the frames[] deltas (timings are informational)",
              file=sys.stderr)
    return 0
