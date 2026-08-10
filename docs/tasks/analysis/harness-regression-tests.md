# Harness Regression Tests

## Goal

Give `scripts/real-scan-verify/harness.sh` enough automated coverage that a change
to nc's CLI surface cannot break it silently. Today it has none: it is a bash
script driving the release binary against Drive assets, run by hand, and nothing in
`cargo test` or CI touches it.

## Why this exists

On 2026-08-09 the `output/presets` default flip broke the harness in **three**
places at once, and all four CI gates stayed green through it:

- `stage_freeze`'s `jq` generator still emitted the removed `output.hdr` key, so a
  fresh freeze wrote recipes the next stage could not load.
- The four `convert`-based stages passed `.tiff` paths and hit exit 2 against the
  new JPEG default.
- `stage_convert` failed **silently**: `nc roll` had become container-aware, so it
  succeeded and wrote `_positive.jpg`, the `*_positive.tiff` rename glob matched
  nothing, the HDR outputs were stranded in the `.hdrtmp` scratch dir, and the stage
  still printed its success line.

The third is the one that motivates the task. A hard exit-2 gets noticed the next
time someone runs the harness; a stage that prints `converted <roll>: N frames x2
modes` while producing the wrong container in the wrong directory does not.

The checked-in recipes were also migrated by hand *without* the generator that
writes them, so the fix and the thing that would undo it lived in the same change.
Any coverage here should make that specific divergence visible.

## Open questions

- **What can run without the Drive assets?** The recipe generator, the argument
  shapes, and the filename conventions are all assertable against the committed
  `tests/fixtures/`; the actual verification matrix is not. A split along that line
  is the obvious candidate, but where exactly it falls is worth deciding against the
  script rather than in advance.
- **Does this belong in CI, or as a fast local pre-flight?** CI has no assets and
  no `exiftool`. A `--dry-run`/`--self-check` mode that exercises the plumbing on
  fixtures might be the useful artifact; so might a plain `shellcheck` run, which
  would not have caught any of the three failures above and should not be mistaken
  for coverage of them.
- **What language?** The repo already has a Python test suite under
  `scripts/analysis/` (91 tests, no CI gate of its own — worth resolving together
  rather than adding a third untested surface).
- **Should the generator and the checked-in recipes be tied together?** Asserting
  that re-running `stage_freeze` reproduces the committed recipes byte-for-byte
  would have caught the `output.hdr` divergence directly. It needs the assets, so
  it may only be a documented manual step.

## Known vs unknown

**Known:** the three failure modes above are real and reproduced; the silent one is
the dangerous one; `harness.sh` is the only consumer of
`scripts/real-scan-verify/recipes/*.json`; the Python half of `scripts/` runs under
no CI gate either.

**Unknown:** whether a useful fixture-only harness run exists at all, and whether
the right unit of coverage is the script, the recipes, or the CLI contract the
script depends on.

## How to Verify

- Re-running the 2026-08-09 breakage against the new coverage fails it — all three
  modes, including the silent one. If a proposed test would not have caught the
  stranded-`.hdrtmp` case, it is not covering the thing this task exists for.
- Whatever runs without assets is wired into a gate someone actually runs, and the
  README says plainly which parts are covered and which stay manual.

## Dependencies

- [Real-scan core verification](real-scan-verification.md) — owns the harness
