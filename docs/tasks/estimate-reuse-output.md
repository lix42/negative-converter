# Reuse-ready `nc estimate` output

## Goal

Make `nc estimate` emit the measured film base in a **directly reusable** form so
the recommended calibrate-once → reuse workflow (design-spec §8) is copy-paste
smooth: measure `Dmin` from an unexposed reference once, then apply it across the
roll.

## Background

`Dmin` is a property of the film stock + development + scanner settings, not of a
frame, so the accurate workflow is to measure the base once and reuse it (see the
`film-base-estimation` progress notes). Today the value would have to be read out
of the JSON report and hand-reformatted into `--film-base R,G,B` / a recipe.

## Design

Extend the `estimate` subcommand's output (wired in `pipeline-orchestration`) so
its JSON report includes, alongside the raw `film_base` array:

- a ready `--film-base R,G,B` string, and/or
- a minimal `film_base` recipe fragment (`{ "source": { "explicit": [r,g,b] } }`)

that can be pasted into a `convert` call or merged into a roll recipe. Keep the
JSON on stdout clean (logs/warnings to stderr) per the determinism rules. Report
the sampled region and source (from the resolved `film_base.source`) too, so the
output documents how the base was obtained.

**Grid / multi-region sampling for unexposed-frame calibration (§9 ladder Tier
1).** A dedicated blank frame offers far more area than a rebate strip — exploit
it: a mode (e.g. `--grid`) that samples several regions (center + corners),
requires per-channel agreement within a documented tolerance, and reports the
spread alongside the combined value. Agreement failure is *diagnostic* — it
indicates light leaks, scanner illumination falloff, or dust — so report it
loudly (warning, `--strict`-promotable) with the per-region values rather than
just averaging them away. Deterministic: fixed grid layout, fixed percentile.

## How to Verify

- `nc estimate --base-region … ref.tiff --report json` emits both the array and
  the reuse-ready string/fragment; the fragment parses back as a valid recipe.
- The emitted `--film-base`/fragment, fed to `nc convert`, reproduces the same
  base (round-trip / determinism).

## Dependencies

- [Pipeline orchestration](pipeline-orchestration.md)
