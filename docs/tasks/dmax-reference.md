# Roll-fixed Dmax from a fully-exposed reference frame

## Goal

Make the display-white anchor `Dmax` a **roll-fixed calibration** — a property of
the film stock + development + scanner, measured once and reused across the roll,
exactly like `Dmin` — acquired from a **fully-exposed reference frame** (the
light-struck roll lead, always available). Demote per-frame `--auto-d-max` from
the default to an explicit, lower-priority "exposure-normalizing" mode.

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
  luma/gray density) — and freeze it into the roll recipe (`density.dmax =
  { "reference": … }` resolving to a scalar, or `{ "explicit": <d> }`). Add a flag
  to point at the reference frame / region (mirror `--base-region`).
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
