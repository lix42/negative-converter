# SDR Preset Follow-ups

## Goal

Track the questions the `display-p3` / `compatibility` presets deliberately left
open. Each is small on its own and none blocks the presets, which ship
working; they are filed so the open space is visible rather than remembered.

None of these is settled — the point of this file is to hold the questions, not
to pre-answer them.

## Design

### 1. Make one of them the default (replacing `legacy`)

`legacy` is still the default preset, and it is a genuinely different, older
*pipeline* — print controls before the ICC transform, never crossing the ACEScg
boundary. The SDR presets are what make it deletable: they cover what it was
being used for (16-bit SDR TIFF) on the modern path.

Open, and worth deciding with a real frame in front of you:

- **Which gamut.** `compatibility` (sRGB) is the safe default and the name says
  so; `display-p3` preserves more of the film's colour and matches modern
  displays. nc's whole thrust is wide-gamut fidelity, which argues for P3; "a
  default should surprise nobody" argues for sRGB.
- **The cost.** Flipping it is a **pixel change** (different pipeline, not just a
  different profile), so it needs a `pipeline_version` bump, a before/after
  report like `reports/render-defaults-v2.md`, and broad test churn — every test
  that exercises the default output path.
- Once it lands, `legacy` and its frozen `stages::golden` vectors can be deleted.
  Do not delete them first.

### 2. Adobe RGB as an output gamut

Raised while reviewing the colour-space surface: nc supports sRGB, Display P3,
ProPhoto and ACEScg, and **Adobe RGB (1998) is the one notable omission** for a
photography tool. It is already usable via `--output-profile <path-to-icc>` on
the legacy path; what is missing is a first-class name.

Not a one-line addition, and the reason is the interesting part: the modern SDR
renderer does not merely *tag* a profile, it **gamut-maps** into the destination
(`neutral-axis-radial-boundary-v1`). So a new gamut needs a
`pipeline/colorimetry/` definition with provenance, pinned artifacts, an
`SdrGamut` arm, and gamut-mapping coverage — the area CLAUDE.md guards most
carefully. Worth doing, worth not rushing.

ProPhoto and ACEScg deliberately stay off the *display* path: they are editing
spaces, which is what `film-master` and `hdr-linear-tiff` are for.

### 3. Report block for the SDR presets — **open**

The SDR presets ship without one. `stages::render_sdr_preset` drops
`SdrRenderMetadata` as `_metadata`, while `hdr-linear-tiff` surfaces
`hdr_linear_tiff` and the coded presets surface `hdr_coded_tiff`.
`SdrRenderMetadata` carries the identical field set — reference white, shoulder
start, tone curve, gamut mapping, linear domain — and none of it reaches the
report; only prose in `output_render.content` describes the render. That is an
asymmetry a scripted consumer feels: the HDR TIFF presets are machine-readable
about their contract and the SDR ones are not.

`sdr::RenderedSdr::metadata()` still carries `#[allow(dead_code)] // consumed next
by output/presets report wiring`, which is the marker for this work.

### Settled: `RunProfile::SdrTiff` is calibrated (2026-08-09)

Recorded here because it was an open item and is not one any more. The profile is
**measured**, not merely inherited from `HdrCodedTiff`'s structurally identical
buffer set: peak RSS **0.850 GB at 15.55 MP** and **3.594 GB at 74.65 MP** against
estimates of **0.921 / 3.911 GB** (1.08x / 1.09x over-estimate), with `accounted`
at 0.80x / 0.91x of measured — under it, as the allowance requires. Two frame
sizes, per the calibration rule. The table in `pipeline::memory`'s module doc
carries the rows.

## How to Verify

- **Default flip:** the before/after report exists and shows what changed on real
  frames; `pipeline_version` bumped with a recorded row; `legacy` either deleted
  or explicitly retained with a reason.
- **Adobe RGB:** the definition carries provenance, its artifacts are pinned and
  audited, and a render into it is covered — not merely tagged.
- **Report block:** an `nc convert --output-preset display-p3` report carries the
  SDR contract as machine-readable fields (the `hdr_coded_tiff` block is the
  shape to follow), and `RenderedSdr::metadata()`'s `#[allow(dead_code)]` is gone.

## Carried-over review findings (not fixed in the shipping PRs)

Both are P2s raised during review of the defaults/preset PRs and deliberately
left out of them — real, bounded, and not defaults questions.

- **The SDR-range warning is luminance-only, so it can misfire on saturated
  colour** (`pipeline/hdr.rs`, `sdr_range_warning`). MaxCLL is a luminance
  measure, so a rendered BT.2020 blue near `[0, 0, 4]` sits around 48 nits of
  luminance while its blue channel uses substantial per-channel headroom that no
  SDR-range signal can carry without clipping or shifting the colour. The
  warning would then claim the whole signal is SDR-range and `--strict` would
  fail valid HDR colour-volume content. Fix by also checking the rendered
  per-channel peak, or by narrowing the claim to "no *luminance* headroom" —
  the latter is the smaller change and matches what MaxCLL actually witnesses.
- **The output-suffix rule does not reach `roll` frames** (`cli.rs`,
  `resolve_frames`). `validate_convert` composes the suffix check with
  `validate`, but a `--frames` manifest carrying an explicit
  `"output": "frame.jpg"` is resolved on the path that calls `validate` alone,
  so a `legacy`/`film-master` frame can still be pointed at a container those
  presets cannot write. Route the per-frame resolution through the same
  composed gate rather than adding a second check — a parallel check is exactly
  how the suffix rule and the convert-only refusal drifted apart before.

- **The telemetry record's preset enum has outrun its schema version**
  (`src/telemetry.rs`). `SCHEMA_VERSION` is 3 and its rustdoc still describes
  `conversion.preset` as `legacy|film-master`, while `OutputPreset` now has ten
  variants. This is **not** this PR's drift: `telemetry.rs` was last touched by
  `#60`, and `ultra-hdr-v1`, `hdr-pq`, `hdr-hlg`, `hdr-linear-tiff`,
  `hdr-pq-tiff` and `hdr-hlg-tiff` all landed after v3 without a bump — these two
  presets are the seventh and eighth to do so. Worth settling deliberately rather
  than in a preset PR, because the answer is a policy question the module states
  but has never had to apply: is *adding* an enum member a wire-shape change (the
  rustdoc's rule, read strictly) or an additive one a forward-compatible consumer
  tolerates? Whichever way it goes, the rustdoc's preset list must stop naming two
  of ten. `telemetry/ingestion-service` is the consumer that makes it matter.
- **The asset manifest infers encoding from bit depth alone**
  (`scripts/analysis/nctool/manifest.py`, the `bits == 16` arm). Every 16-bit `nc`
  output is labelled `u16-srgb`, which was true while `nc` had no other 16-bit SDR
  encoding. A `display-p3` result dropped into `converted/nc/` would be recorded
  as sRGB and any cross-encoding analysis would then decode it with the wrong
  primaries. Latent today — no P3 asset exists yet — and the fix needs real
  provenance (the sidecar's `output_render.encoding`) rather than a second
  filename guess.

## Dependencies

- [Output presets and guidance](presets.md)
