# Roll-fixed Dmax from a fully-exposed reference frame

## Goal

Make the legacy-named `Dmax` parameter a **roll-fixed exposure placement** — a property of
the film stock + development + scanner, measured once and reused across the roll,
exactly like `Dmin` — acquired from a **fully-exposed reference frame** (the
light-struck roll lead, always available). Demote per-frame `--auto-d-max` from
the default to an explicit, lower-priority "exposure-normalizing" mode.

> **Terminology.** `Dmax` is a **scalar** in corrected-density (`D′`) space,
> distinct from classic photographic film Dmax (the negative's physical maximum
> optical density). The shipped pre-artifact renderer used it as a display-white
> anchor. In the replacement pipeline, Dmax belongs to the selected density
> curve: exponential uses it for scalar placement and sigmoid uses it as a
> curve-shaping input. SDR/HDR rendering owns display reference white. This task
> changes acquisition/default policy and keeps the parameter's density units.

## Background

`dmax-white-anchor` initially shipped `Dmax` as **frame-local**, measured per
frame from the corrected-density distribution (`--auto-d-max` was the default).
This completed task has since replaced that default with roll-fixed `fixed`. The design
review (2026-07) found this contradicts NC's core purpose: anchoring each frame's
densest pixel to display white **is per-frame exposure normalization** — it
silently brightens underexposed frames, forces an overcast scene's grey to white,
and breaks roll consistency. NC's job is to *convert* faithfully (preserving
relative exposure); exposure grading belongs in Lightroom.

In the shipped renderer, `Dmax` is mechanically the exponent offset that places
the density scale into the output range (`lin = 10^(gamma·(D′ − Dmax))`). The
principled reference is a
**fixed reference density** (the density a reference diffuse white / full exposure
produces on this stock), not a per-frame extremum — matching Cineon's fixed code
values. The brightest pixel then maps to *wherever it falls* (below white for a
dim frame, clipping above for a specular), which is the faithful behavior.

**Verification data (2026-07):** fully-exposed leader frames are near-opaque in
RGB (Ektar `1009` luma ≈ 0.016, Phoenix `1010` ≈ 0.039 — the max-density
endpoint) while the unexposed base sits at ≈ 0.30 — giving both endpoints per
roll. The leader is uniform and always present (film is light-struck during
loading), so it is a reliable per-roll `Dmax` reference, measured per-channel from
its interior just as `Dmin` is measured from the unexposed frame.

A second failure of per-frame auto-`Dmax` (noted in `bw-support`, PR #21 finding
4): on **uncropped** frames the dark holder / dust sit at the *top* of the
corrected-density distribution and can capture the 99.5th-percentile anchor,
dimming the render. A roll-fixed reference `Dmax` sidesteps per-frame anchoring
entirely; `ir-holder-detection`'s holder mask is the complementary
border-exclusion fix for anyone who keeps per-frame auto-`Dmax`.

**Superseding note:** this task **supersedes the "Dmax is frame-local, auto by
default" decision from the shipped `dmax-white-anchor` task**. The replacement
`negative-reconstruction-density-curves` task owns the curve-specific Dmax
machinery.

## Design

- **`Dmax` stays a scalar** (as `--d-max` is today), so it *places* the density
  scale without smuggling in per-channel color correction: a per-channel `Dmax`
  would apply three different gains — that's a white
  balance, and highlight color balance is the print-render WB stage's job, not the
  anchor's (density conversion ≠ print rendering). Keeping it scalar also makes a
  reference-derived, fallback, and CLI `Dmax` behave identically. Measure a
  **single scalar** from the fully-exposed reference's uniform interior — a
  documented reduction of its corrected R/G/B density to one value (e.g. the
  luma/gray density). **Freeze the resolved *scalar* into the roll recipe**
  (`density.dmax = { "explicit": <d> }`), and record the reference frame/region as
  **provenance** (report/`meta`), *not* as a re-read directive in the recipe. A
  `{ "reference": … }` form baked into the frozen recipe would make the apply phase
  re-read an external file — so the same recipe hash could yield different output
  if that file changes or is missing, breaking the plan→recipe→**deterministic
  apply** contract (`roll-conversion`). The shipped plan-phase interface is
  `nc estimate --d-max-region X,Y,W,H`, which mirrors `--base-region` and emits
  reuse-ready `--d-max` / explicit recipe forms.
- **Default becomes a fixed `Dmax`**, resolved in order: measured reference →
  per-stock constant → a nominal **corrected-density** anchor (e.g. `Dmax ≈ 2.0`,
  a scene-independent placement expressed *in density units*). Note `Dmax` lives
  in the post-base `D′` space where the base is `0`, so the fallback is a density
  value — **not** the base transmission plus a range (mixing transmission and
  density is a unit error). Roll-fixed, reused across frames.
- **Demote `--auto-d-max`** to an explicit opt-in, documented as per-frame
  exposure normalization (lower priority per the user).
- Keep `--d-max` (scalar) and `--no-d-max` as today. In the target density path,
  `--no-d-max` means unity exponential placement and the film-master preset records that
  policy; it is not shorthand for a display transfer or reference-white choice.
- Interacts with `roll-conversion` (`Dmax` is roll-fixed alongside `Dmin`).
- **This changed the default render** (frame-local auto-`Dmax` → fixed anchor).
  `conversion-versioning` has not shipped and there is no code-level
  `pipeline_version` field to bump yet, so that task must label this boundary as
  `pipeline_version 1` and preserve the recorded v0 baseline. Do not fabricate an
  unused constant in the meantime.

## How to Verify

- Measure `Dmax` from Ektar `1009` / Phoenix `1010`; convert several frames of
  each roll with the fixed anchor → underexposed frames render dark (not
  auto-brightened), roll tonality is consistent frame-to-frame.
- Contrast with `--auto-d-max` on the same frames → shows the per-frame stretch.
- The nominal-density fallback (no reference) still produces a viewable default.
- A reference-derived `Dmax` and an equal scalar `--d-max` yield **identical
  color** (the anchor introduces no per-channel correction).

## Dependencies

- [Display-range white anchor (Dmax)](dmax-white-anchor.md)
