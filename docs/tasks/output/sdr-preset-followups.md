# SDR Preset Follow-ups

## Goal

Track the questions the `display-p3` / `compatibility` presets deliberately left
open. Each is small on its own and none blocks the presets, which ship
working; they are filed so the open space is visible rather than remembered.

None of these is settled — the point of this file is to hold the questions, not
to pre-answer them.

## Design

### 1. Make `display-p3` the default

**Decided 2026-08-09 (user): the default becomes `display-p3`.** nc's thrust is
wide-gamut fidelity, and that outweighs sRGB's "surprise nobody" argument. What
remains here is the execution, not the choice.

**Note what this replaces.** `output/presets` ships `gain-map-hdr` as the default
first, so by the time this runs the incumbent is `gain-map-hdr` — a JPEG — not
`legacy`. The sequencing is deliberate: gain-map HDR goes in, the rendering gets
exercised and tuned on real frames, and the default then settles on SDR lossless.
A container change rides along with the pixel change.

- **The cost.** A **pixel change** (different pipeline, not just a different
  profile), so it needs a `pipeline_version` bump, a before/after report like
  `reports/render-defaults-v2.md`, and broad test churn — every test that
  exercises the default output path, twice over if `gain-map-hdr` landed first.
- `legacy` and its frozen `stages::golden` vectors become deletable once a
  modern-path preset is the default. Do not delete them first.

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

`sdr::RenderedSdr::metadata()` still carries its `#[allow(dead_code)]`, which is the
marker for this work. **`output/presets` finished without wiring it** — the report
block was never in that task's scope — so the allowance's comment naming presets as
the next consumer is stale and this task is the real owner.

### Settled: `RunProfile::SdrTiff` is calibrated (2026-08-09)

Recorded here because it was an open item and is not one any more. The profile is
**measured**, not merely inherited from `HdrCodedTiff`'s structurally identical
buffer set: peak RSS **0.850 GB at 15.55 MP** and **3.594 GB at 74.65 MP** against
estimates of **0.921 / 3.911 GB** (1.08x / 1.09x over-estimate), with `accounted`
at 0.80x / 0.91x of measured — under it, as the allowance requires. Two frame
sizes, per the calibration rule. The table in `pipeline::memory`'s module doc
carries the rows.

## How to Verify

- **Default flip:** `display-p3` resolves with no output-selection options; the
  before/after report exists and shows what changed on real frames;
  `pipeline_version` bumped with a recorded row; `legacy` either deleted or
  explicitly retained with a reason.
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
- ~~**The output-suffix rule does not reach `roll` frames.**~~ **Fixed 2026-08-09**
  by `output/presets`' roll chunk: `resolve_frame_output` runs an explicit manifest
  path through `reject_suffix_mismatch`, the same rule `convert` uses, rather than a
  parallel check. Derived names are not re-checked — they are built from the same
  table, and a test over `OutputPreset::ALL` pins that the derived spelling is always
  one the rule accepts.

- **The telemetry record's preset enum has outrun its schema version — still open,
  and now clearer.** `OutputPreset` has grown to **twelve** variants, ten of them
  added after the record last bumped *for a preset reason*. The underlying question
  is untouched: is *adding* an enum member a wire-shape change (the module's rustdoc
  rule, read strictly) or an additive one a forward-compatible consumer tolerates?
  `telemetry/ingestion-service` is the consumer that makes it matter.
  **What changed on 2026-08-09:** `SCHEMA_VERSION` moved 3 → 4, but for the
  `conversion.output_hdr` (bool) → `conversion.output_depth` (`u16`|`f32`) **field
  rename** that rode with `output.hdr` → `output.depth`. A renamed field is a wire
  change under any reading, so that bump does *not* answer this question — and the
  rustdoc no longer restates the preset list at all, deliberately, since restating it
  is what went stale. Do not read the v4 bump as the enum policy having been decided.
- **The asset manifest infers encoding from bit depth alone**
  (`scripts/analysis/nctool/manifest.py`, the `bits == 16` arm). Every 16-bit `nc`
  output is labelled `u16-srgb`, which was true while `nc` had no other 16-bit SDR
  encoding. A `display-p3` result dropped into `converted/nc/` would be recorded
  as sRGB and any cross-encoding analysis would then decode it with the wrong
  primaries. Latent today — no P3 asset exists yet — and the fix needs real
  provenance (the sidecar's `output_render.encoding`) rather than a second
  filename guess.

### Not covered by `nctool compare`: the default preset

Recorded 2026-08-09 while fixing the review round. `benchmark.json`'s six fixture
cases now state `--output-preset legacy` explicitly, because they write `.tiff` and
exist to stay comparable with records made before the default flip. The consequence
is that the **product default is not in the fixed comparison set at all** — a
cross-build comparison says nothing about the container users actually get.

Adding a case changes that fixed set, which is `core/conversion-versioning`'s call
rather than this task's, so it is filed here as a visible gap. Whoever takes it will
also meet the units question: `mean` for a JPEG preset is the normalized 8-bit
buffer handed to the compressor, and the record's marker is `output_depth`, which is
a depth rather than a container.

## Dependencies

- [Output presets and guidance](presets.md)
