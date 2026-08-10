# Render defaults v2 → v3

Measured baseline for the `pipeline_version` 3 default (2026-08-09,
`output/presets`). One default moved: `output.preset`.

| default | v2 | v3 |
|---|---|---|
| `output.preset` | `legacy` (16-bit TIFF) | **`gain-map-hdr`** (dual-dialect gain-map JPEG) |

This is a **container** change as well as a render change, which makes it unlike
[v2](render-defaults-v2.md). Two consequences a reader should have before the
numbers:

- `nc convert -o out.tif` with no preset is now a **usage error** (exit 2), where it
  previously wrote a 16-bit TIFF. The diagnostic names the default and points at
  `display-p3`. Reaching the old path takes `--output-preset legacy` (or `custom`).
- The default now crosses the ACEScg boundary into the SDR/HDR display renderers
  instead of running `finish_print` before the ICC transform. `legacy` is untouched
  and still selectable by name.

Film base is stated explicitly in every run below, as in v2: it has no default, and
a roll's base is a calibration both versions must share or the comparison measures
the base instead of the pipeline. Produced by
[`scripts/render-defaults-v3/measure.py`](../../scripts/render-defaults-v3/measure.py);
re-run it before quoting anything here.

## The pixels barely move; the container changes completely

`mean` is `output_stats.mean` — for v3 the normalized 8-bit buffer handed to the
JPEG compressor, for v2 the u16 value normalized to `[0, 1]`. The two are directly
comparable as *rendered* values but not as stored bytes, and JPEG is lossy where the
TIFF was not.

| roll | v2 clipped | v3 clipped | v2 mean RGB | v3 mean RGB | ΔRGB |
|---|---|---|---|---|---|
| `2026-07-24-Gold200` | 0.000% | 0.000% | 0.3195, 0.3451, 0.3934 | 0.3251, 0.3446, 0.3906 | +0.0056, −0.0005, −0.0028 |
| `Ektar` | 0.000% | 0.000% | 0.3949, 0.4697, 0.5644 | 0.4114, 0.4685, 0.5573 | +0.0165, −0.0012, −0.0071 |
| `Portra160` | 0.000% | 0.000% | 0.3142, 0.3849, 0.5120 | 0.3305, 0.3835, 0.5033 | +0.0163, −0.0014, −0.0087 |
| `Portra160-2026-07-22` | 0.000% | 0.000% | 0.3872, 0.4411, 0.5240 | 0.3981, 0.4397, 0.5172 | +0.0109, −0.0014, −0.0068 |
| `Portra400` | 0.000% | 0.000% | 0.4359, 0.5104, 0.6032 | 0.4511, 0.5084, 0.5956 | +0.0152, −0.0020, −0.0076 |
| `Portra400-leica-flaw` | 0.000% | 0.000% | 0.2256, 0.2194, 0.2428 | 0.2249, 0.2198, 0.2415 | −0.0007, +0.0004, −0.0013 |
| `phoenix` | 0.000% | 0.000% | 0.2336, 0.2291, 0.3709 | 0.2332, 0.2295, 0.3618 | −0.0004, +0.0004, −0.0091 |

Neither version clips on any of the seven rolls. The shift is a consistent, small
**warming**: red rises up to +0.017 while blue falls up to −0.009, green essentially
still. That is the display renderer's gamut mapping and reference-white handling
replacing the legacy `finish_print` → ICC ordering, and it is largest on the
saturated stocks (Ektar, Portra) and near zero on the two flattest frames.

## The gain map is inert at the default curve — accepted, not an open item

The container is correct and Apple ImageIO reads it — but on every roll measured
here the decoded `GainMapMax` is **0.000001** (log2), i.e. a gain of 1.0x:

| roll | `GainMapMax` (log2) | gain |
|---|---|---|
| `2026-07-24-Gold200` | 0.000001 | 1.00x |
| `Ektar` | 0.000001 | 1.00x |
| `phoenix` | 0.000000 | 1.00x |

So `nc convert` with no flags now produces a **structurally valid HDR file carrying
no HDR**. The cause is the render, not the container: under the default sigmoid the
HDR rendition's brightest pixel measures 203 nits — exactly reference white — and
nc's own `hdr::sdr_range_warning` says so on the single-rendition presets. Selecting
the exponential curve on the same frame gives `GainMapMax` 2.2827 (≈4.87x), close to
the full 4.926 headroom.

That warning does not fire for the gain-map presets, deliberately: they are
dual-rendition, so a flat HDR rendition yields an inert gain map rather than a
mislabelled container.

**Reclassified 2026-08-10, after this report's numbers were reviewed.** This is not
a gap to close before shipping — it is an accepted state, and HDR is now explicitly
**lower priority**. The reasoning that changed:

- **The film is not the limitation.** Negative stock carries wide latitude; the
  *print rendering* decides whether output exceeds diffuse white. The
  reference-anchored sigmoid pins mid-grey at half the reference density and rolls
  its shoulder so diffuse white lands *at* reference white — so nothing exceeds it
  **by construction**, and the 1.0x above is the curve working as designed.
- **The exponential already shows the other answer**, at 4.87x on the same frame,
  because it pins white at `Dmax` with no placement rule. So the headroom question
  is really a question about *which curve does the tone shaping*, which is now
  tracked as
  [`algo/reconstruction-render-curve-split`](../tasks/algo/reconstruction-render-curve-split.md).
- **`gain-map-hdr` therefore stays the default on purpose**, rather than being
  moved to `display-p3` sooner. The default is not a quality claim here; it keeps
  the most capable path exercised while the reconstruction/render split is tried.
  `display-p3` remains the eventual destination via
  [`output/sdr-preset-followups`](../tasks/output/sdr-preset-followups.md).

## What this report does not cover

The drift gate cannot witness this change. `PIPELINE_FINGERPRINTS`' `render` and
`base` hashes cover `reconstruct_and_print` and `film_base::estimate`; the output
preset selects neither, so the v3 row carries the **same** two hashes as v2 and only
`recipe` moved. That is the documented coverage limit of
`version::PipelineFingerprint`, not a sign the render is unchanged — this report is
the evidence for v3, and the numbers above are what a future comparison should be
measured against.
