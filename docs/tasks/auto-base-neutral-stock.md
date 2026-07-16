# Neutral-base robustness for auto film-base detection

## Goal

Harden auto film-base detection for film stocks whose base is **near-neutral**
(e.g. Harman Phoenix, R/B ≈ 0.84) rather than orange. Such a base is bright but
**not color-distinctive**, so any confidence signal that assumes a colored/orange
mask — or that keys on base *color* — is weak and can mis-anchor on bright neutral
scene content. A follow-up to `auto-base-redesign` (build on its detector; do not
duplicate it).

## Background

`auto-base-redesign` locates the rebate per edge (brightness + along-edge
uniformity) and corroborates with **cross-edge color agreement** — "a real rebate
is the same orange base value on the edges where it appears; a bright background
usually is not." That corroboration assumes a distinctive base color.

Real-scan verification (2026-07) found two stocks with **opposite** bases:

- **Ektar** — orange mask, R/B ≈ 2.75. Distinctive: a bright strip that is
  emphatically orange is unmistakably film base. Easy case.
- **Phoenix** — near-neutral / faintly blue, R/B ≈ 0.84. **Indistinguishable from
  ordinary bright neutral content** — overcast sky, a white wall, water.

So for a neutral-base stock, two of the current signals degrade:

- The "brighter than interior in *some* channel" gate is weaker — a neutral base
  isn't decisively bright in any single channel the way Ektar's is in R.
- Cross-edge / cross-frame **color** agreement is weak — neutral ≈ sky ≈ wall, so
  agreement can be *falsely* satisfied by neutral scene content.

This is a **stock-level** hardness, independent of holder geometry: IR holder
masking (`ir-holder-detection`) removes the holder confounder but does **not**
help here, because a neutral base and neutral scene content are *both* film and
*both* bright in IR.

## Design

Do not assume a colored base in any confidence gate. Lean on signals that don't
depend on base color:

- **Spatial flatness over color.** A rebate is featureless-flat; sky has gradients,
  walls have texture. Strengthen the along-strip low-spread requirement (and
  consider a 2-D flatness check) as the primary corroborator when base color is
  non-distinctive.
- **Geometry.** The rebate is a thin band at the film edge (inset behind the
  holder); neutral scene content is not edge-locked. Combine with the holder mask
  (`ir-holder-detection` when IR present, else the RGB holder step) to require the
  candidate sit immediately inboard of a holder edge / at the true film boundary.
- **Cross-frame agreement on the transmission *value*, not color.** The base
  transmission is constant per roll; a neutral wall/sky varies frame to frame.
- **Tune and validate on Phoenix, not just Ektar** — thresholds picked only on the
  orange stock will overfit.

## How to Verify

- Phoenix frames with bright neutral content (sky / white wall) → the detector
  does **not** mis-anchor that content as the base.
- Phoenix unexposed / rebate regions → still detected as base.
- Ektar (orange) results unchanged — no regression on the easy case.

## Dependencies

- [Robust auto film-base detection](auto-base-redesign.md)
