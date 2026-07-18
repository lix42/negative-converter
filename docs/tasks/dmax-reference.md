# Roll-fixed Dmax from a fully-exposed reference frame

## Goal

Make the display-white anchor `Dmax` a **roll-fixed calibration** — a property of
the film stock + development + scanner, measured once and reused across the roll,
exactly like `Dmin` — acquired from a **fully-exposed reference frame** (the
light-struck roll lead, always available). Demote per-frame `--auto-d-max` from
the default to an explicit, lower-priority "exposure-normalizing" mode.

> **Terminology.** `Dmax` here is nc's *display-white density anchor* — a **scalar**
> in corrected-density (`D′`) space, **distinct** from classic photographic film
> Dmax (the negative's physical maximum optical density). This task changes only how
> that anchor is *acquired* (roll-fixed reference vs per-frame auto), never its
> meaning or units. See design-spec §4 "Terminology & value domains" for the
> canonical definition and the transmission-vs-density distinction this design
> leans on.

## Background

`dmax-white-anchor` (shipped) defined `Dmax` as **frame-local**, measured per
frame from the corrected-density distribution (`--auto-d-max` default). The design
review (2026-07) found this contradicts NC's core purpose: anchoring each frame's
densest pixel to display white **is per-frame exposure normalization** — it
silently brightens underexposed frames, forces an overcast scene's grey to white,
and breaks roll consistency. NC's job is to *convert* faithfully (preserving
relative exposure); exposure grading belongs in Lightroom.

`Dmax` is mechanically just the exponent offset that places the density scale into
the output range (`lin = 10^(gamma·(D′ − Dmax))`). The principled anchor is a
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
default" decision from the shipped `dmax-white-anchor` task** — the render
machinery stays; the default and the acquisition change.

## Design

- **`Dmax` stays a scalar** (as `--d-max` is today), so it *places* the density
  scale without smuggling in per-channel color correction: a per-channel `Dmax`
  would apply three different gains in `10^(gamma·(D′−Dmax))` — that's a white
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
  apply** contract (`roll-conversion`). Add a flag to point at the reference frame
  / region during the *plan* phase (mirror `--base-region`).
- **Default becomes a fixed `Dmax`**, resolved in order: measured reference →
  per-stock constant → a nominal **corrected-density** anchor (e.g. `Dmax ≈ 2.0`,
  a scene-independent placement expressed *in density units*). Note `Dmax` lives
  in the post-base `D′` space where the base is `0`, so the fallback is a density
  value — **not** the base transmission plus a range (mixing transmission and
  density is a unit error). Roll-fixed, reused across frames.
- **Demote `--auto-d-max`** to an explicit opt-in, documented as per-frame
  exposure normalization (lower priority per the user).
- Keep `--d-max` (scalar) and `--no-d-max` (scene-referred HDR) as today.
- Interacts with `roll-conversion` (`Dmax` is roll-fixed alongside `Dmin`).
- **This changes the default render** (frame-local auto-`Dmax` → fixed anchor) — a
  default-output change that **must bump `pipeline_version`** per
  `conversion-versioning` (its golden-output gate enforces this when that task has
  shipped; if `dmax-reference` lands first, bump the version by hand so outputs
  aren't mislabeled `v0`). Coordinate the release of the two.

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
