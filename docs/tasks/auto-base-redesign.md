# Robust auto film-base detection

## Goal

Replace the Step-1 margin heuristic in `pipeline/film_base.rs::auto_estimate`
with a detector that works on real scans, whose layout is
`dark film holder → thin unexposed rebate → exposed picture`. Auto (`--auto-base`,
the default) must locate the thin rebate band and return its per-channel
transmission as the `FilmBase`, or fail loudly when there is no confident band.

## Background (why the Step-1 heuristic fails)

Real-scan verification of `film-base-estimation` (see `progress.md`) showed the
current heuristic samples the outer 4% margin as one blob — holder + rebate +
picture — which is high-spread, so it always bails. The rebate (the actual film
base) is a **narrow, uniform, bright orange band inset behind the holder**, and it
may appear on **only some edges**. Measured transmission is consistent per film
stock (e.g. `48bit-full/1` bottom and `/2` left both ≈ `[0.53, 0.26, 0.16]`).

## Design

Per edge, march a thin (1px) strip parallel to the edge inward from depth 0 to a
cap (e.g. ~10% of the short dimension). For each strip compute a per-channel high
percentile (p97, reuse `percentile`) and an along-strip spread `(p97−p10)/p97`.
Classify:

- **Holder** — very dark; skip.
- **Rebate candidate** — bright *and* low-spread (uniform along the edge).
- **Picture** — high-spread; skip.

The film base is the **brightest low-spread band across all edges** (the rebate is
the film's minimum density ⇒ maximum transmission, so it is the brightest uniform
region). If no edge yields a confident band, return a clear `NcError` pointing at
`--base-region` / `--film-base`, exactly as today.

**Must not mis-anchor on a uniform bright surround.** The Step-1 heuristic's two
gates (per-channel uniformity spread ≤ 0.15, and brighter-than-interior-median by
>2% on *any* channel) are jointly insufficient: a frame with a uniformly bright
surround bleeding to the edge (white background, sky) passes both and yields a base
anchored on that surround instead of the film rebate — a silently-wrong `Dmin`
(flagged in code review of `film-base-estimation`). The redesign must add a
corroborating signal — e.g. require **cross-edge agreement** (a real rebate is the
same orange base value on the edges where it appears; a bright background usually
is not), and/or a more meaningful base-vs-interior margin than 2% — and revisit the
lenient `any`-channel brightness gate vs. the strict all-channel uniformity gate.

Keep it deterministic and modest — this is not a segmentation problem. Coordinate
with `white-holder-support` (a light holder inverts the "holder is dark"
assumption) and consider exposing only minimal tuning (if any) as flags.

**Thresholds stay deliberately strict, tuned on real scans.** Auto is Tier 2 of
the design-spec §9 acquisition ladder — a convenience layer, not the accurate
path (that's a dedicated unexposed frame) — so a refused detection is acceptable
and a wrong one is not. Tune the confidence gates against the
`real-scan-verification` results rather than loosening them to make demos pass.

**Also in this task's scope (same file/family, from the §9 ladder):**

- **Content-based source (ladder Tier 3, explicit opt-in).** New
  `film_base.source = "content"` (flag e.g. `--base-content`) for scans cropped
  to the image with no unexposed film visible: per-channel high percentile of the
  exposed content (thinnest area ≈ scene's deepest black ≈ base). Never a silent
  fallback when auto refuses — the auto failure message *suggests* it, the user
  or agent opts in, and the report records the content source so such rolls are
  auditable. Document the failure mode (no near-black in scene → washed, cast
  blacks; recoverable downstream as a global cast).
- **Uniformity warning on `--base-region`.** Today `sample_region` takes the
  percentile with no spread check, so a mixed rebate/image rectangle yields a
  plausible-looking bad base silently. Apply the same spread gate as auto — as a
  report warning (`--strict`-promotable), not an error, since a human may
  legitimately sample an odd patch. This is also the validation a future UI
  region-picker calls (§12).
- **`nc inspect` reports candidate rebate regions** (coordinates + confidence)
  from the inward-scan detector, so CLI users confirm a region instead of
  measuring one in an image viewer — and a future UI gets its highlight
  rectangles from the same data.

## How to Verify

- Synthetic `holder → thin rebate → picture` image (rebate on one/two edges only)
  yields a base close to the rebate value.
- A uniform dark picture region does **not** out-rank a genuine (brighter) rebate.
- No-rebate image still fails loudly with an actionable error that names the
  recovery flags (`--film-base`, `--base-region`, `--base-content`).
- Content mode: synthetic image with a known near-black patch → base ≈ that
  patch; report marks the content source; merge test for the new source arm.
- `--base-region` over a deliberately mixed rectangle emits the uniformity
  warning; a clean rebate rectangle does not.
- Regression: the existing explicit/region paths and their tests are unchanged
  (the region uniformity check warns, never changes the value).
- Validate against the real scans in `../nc-assets` and `~/Pictures/scan`
  (uncommitted probe, as in the `film-base-estimation` verification).

## Dependencies

- [Film-base / Dmin estimation](film-base-estimation.md)
