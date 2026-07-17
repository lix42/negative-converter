# Conversion baseline — v0

**Version:** `v0` (pipeline_version 0) · **Date:** 2026-07-15 · `nc` 0.1.0
**Assets:** Ektar 100 and Harman Phoenix rolls — full-size SilverFast HDRi scans
from the `nc-assets` Google Drive folder
(https://drive.google.com/drive/folders/1qXE2jF3MuVnQ2sW0pGTp3URwBJuf_LV6), synced
locally to `../nc-assets/{Ektar,phoenix}/` · **Outputs:** `../nc-assets/converted/`

This is the reference point every future version is compared against. It records
*what v0 does*, the numbers it produced on real scans, and where it falls short —
so a later `v1` (auto white-balance, tone curve, Dmax rework) can be measured
against it rather than judged by eye.

## What "v0" is (the behavior under test)

The shipped Step-1 default path, driven through the recommended §8 workflow:

- **Algorithm:** `density` (Cineon / negadoctor density-domain).
- **Film base (Dmin):** measured **once per roll** from an unexposed reference
  frame (`estimate --base-region`), reused across the roll via `--film-base`.
- **Dmax:** `--auto-d-max` (per-frame — the **99.5th percentile** of corrected
  density, `AUTO_DMAX_PERCENTILE`, not the single densest pixel; the top ~0.5%
  above the anchor is what clips).
- **Print rendering:** defaults only — **no auto white-balance**, no tone curve
  beyond the density gamma, `print_exposure = 0`.
- **Output:** 16-bit sRGB TIFF; no crop (rebate borders left in frame).

## Method

```
# Paths are relative to the repo root; sync the nc-assets Google Drive folder
# (https://drive.google.com/drive/folders/1qXE2jF3MuVnQ2sW0pGTp3URwBJuf_LV6) to ../nc-assets/ first.
# 1. Measure Dmin per roll from the unexposed reference frame.
nc estimate ../nc-assets/Ektar/20260713-nikon-963.tif   --base-region 2000,1400,400,400
nc estimate ../nc-assets/phoenix/20260712-nikon-933.tif --base-region 1500,1500,400,400
# 2. Convert each exposed frame with that fixed base (density, u16 sRGB, auto-Dmax).
nc convert ../nc-assets/<roll>/<frame>.tif -o ../nc-assets/converted/<roll>/<frame>_pos.tif \
  --film-base <roll Dmin>
```

**Measured Dmin (roll-fixed):**

| Roll | Dmin (R, G, B) | R/B | Note |
|---|---|---|---|
| Ektar | `0.520, 0.278, 0.190` | 2.73 | orange mask |
| Phoenix | `0.363, 0.262, 0.426` | 0.85 | near-neutral / bluish base |

## Results — default conversions

All six exited 0. Clip is small (spot highlights, not the frame):

| Frame | Scene | resolved Dmax | clip |
|---|---|---|---|
| Ektar 971 | lakeside stump, dusk | 2.377 | 0.50% |
| Ektar 989 | — | 2.229 | 0.50% |
| Ektar 991 | forest, person on log | 2.265 | 0.49% |
| Phoenix 936 | boy, white tee | 2.206 | 0.49% |
| Phoenix 956 | river at dusk | 2.237 | 0.49% |
| Phoenix 958 | — | 2.259 | 0.50% |

Outputs: `converted/{Ektar,phoenix}/<frame>_pos.tif` (+ `_pos.jpg` previews).

## Findings

**The density-conversion core is correct.** Every frame is a faithful,
recognizable positive with real, recoverable color. Two *systematic default gaps*,
visible on all six:

1. **Too dark.** Even the daylight portrait (936) renders the white tee as dark
   grey. Auto-Dmax anchors the 99.5th-percentile corrected density to white and
   the contrasty default gamma crushes everything below. These frames are
   **uncropped** (rebate/holder borders in-frame), and the dark holder pixels
   have the *highest* density of anything, so they likely capture much of the top
   0.5% and pull the anchor high — dimming the scene (`bw-support` PR #21, finding
   4). So the darkness is **anchor pollution + contrasty gamma**, not just
   exposure; `ir-holder-detection` + `dmax-reference` are the fixes.
2. **Blue color cast.** Ektar → cool/blue; Phoenix → blue-magenta. The per-channel
   base division neutralizes the *shadow* anchor, but there is **no neutral white
   balance** on the mid/highlights, so a cast rides through the tones.
3. **(Cosmetic)** Rebate borders remain (orange on Ektar, pink on Phoenix) — no
   crop, by design.

### Quantified cast (measured on a should-be-neutral patch)

Sampled in the default output (sRGB-encoded means):

| Patch | R | G | B | verdict |
|---|---|---|---|---|
| Phoenix 936 white tee | 0.172 | 0.158 | 0.252 | blue-heavy, ~0.25 (white ≈ 0.85) |
| Ektar 971 lake mist | 0.199 | 0.235 | 0.299 | blue-heavy, dark |

### Manual correction → the engine is validated

Correcting **two** frames by hand (measured, not guessed): de-gamma the patch,
compute WB gains that neutralize it (normalized so WB doesn't shift exposure), add
exposure + a highlight roll-off:

- Phoenix 936: `--white-balance 1.24,1.50,0.54 --print-exposure 1.8 --highlight-compress 0.4`
- Ektar 971: `--white-balance 1.51,1.06,0.63 --print-exposure 1.8 --highlight-compress 0.4`

Result (`converted/…/<frame>_corr.jpg`): natural skin tones, the tee reads white,
neutral mist, natural greens, correct exposure — believable film scans. The cast
and darkness are **defaults gaps, not conversion bugs.**

**Tradeoff observed:** the flat +1.8-stop lift pushed clipping from ~0.5% to
**4–6%** (blown highlights) — motivating a proper S-curve tone map (lift midtones,
roll off highlights) rather than a linear exposure gain.

## Baseline metrics to track across versions

| Metric | v0 |
|---|---|
| Default clip fraction | ~0.5% |
| Neutral-patch balance (want R≈G≈B) | blue-heavy (B ≈ 1.5× R) |
| Neutral-patch level (want ≈ 0.85) | ~0.25 (dark) |
| Auto white-balance | none |
| Tone curve | density gamma only |
| Dmax | per-frame auto (exposure-normalizing) |

## What v1 should move

Maps onto the queued Phase 6 tasks:
- **cast** → `auto-neutral-wb` (automates exactly the manual WB above — highest-impact gap)
- **darkness / clipping** → `algo-sigmoid` (tone curve) + `dmax-reference` (roll-fixed Dmax)
- comparison machinery → `conversion-versioning`
