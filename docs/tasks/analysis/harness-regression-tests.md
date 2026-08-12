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

## Outcome (2026-08-11)

The fixture-only boundary is useful and now runs in CI. A stdlib Python black-box
test builds a temporary one-roll asset tree from the committed TIFF fixtures and
drives the real debug `nc` binary through `freeze` and `convert`. It pins the
generated recipe structure and explicit `legacy`/`f32` selections, then requires
the exact TIFF and sidecar set.

The harness itself now runs both render modes in a fresh per-run staging tree and
publishes nothing until every expected artifact has TIFF magic, every sidecar has
object-valued `meta` and `params`, and there are no extras or directory-shaped final targets. It
revalidates published files before success and rewrites each roll report's
`frames[].output` from the removed staging path to the durable published path.
The raw roll report must first name every expected staging path exactly once with
`status: ok`, so a report-schema regression cannot publish images and fail only
afterward.
This makes old persistent outputs unable to mask a failure. A fake binary reproduces
the historical sharp case—u16 TIFF succeeds, float roll exits 0 but writes JPEG—and
the harness fails before publication or its success line. Ordinary command errors
and determinism mismatches also propagate nonzero; the intentionally failing strict
probe accepts only exit 1 carrying both the IR-ignored warning and strict-promotion
diagnostic.

The full `scripts/analysis` unittest discovery command is a Linux and macOS CI gate.
Drive-backed image-quality, interoperability, IR, determinism, and resource checks
remain manual. `shellcheck` was not added: it would not have caught any of the three
failures this task exists to prevent.

## How to Verify

- Re-running the 2026-08-09 breakage against the new coverage fails it — all three
  modes, including the silent one. If a proposed test would not have caught the
  stranded-`.hdrtmp` case, it is not covering the thing this task exists for.
- Whatever runs without assets is wired into a gate someone actually runs, and the
  README says plainly which parts are covered and which stay manual.

## Dependencies

- [Real-scan core verification](real-scan-verification.md) — owns the harness
