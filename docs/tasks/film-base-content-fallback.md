# Content-based film-base fallback (Tier 3)

## Goal

Add an explicit, opt-in film-base source that estimates `Dmin` from the exposed
image **content** when no unexposed film (dedicated frame, rebate, or holder-inset
band) is available to sample — the design-spec §9 acquisition-ladder **Tier 3**. A
per-channel high percentile of the scene ≈ the thinnest area of the negative ≈ the
scene's deepest black ≈ close to true base. **Never a silent fallback**: the
caller opts in and the report records that the base came from content statistics.

## Background

Real-scan verification (Phoenix / Ektar rolls, 2026-07) confirmed the case this
covers: exposed frames frequently have **no clean rebate** on any edge (the
picture bleeds to the frame border), so both `--base-region` and `--auto-base`
have nothing to sample.

Two facts shape the design:

- **Color-agnostic, correctly.** The estimate is per-channel independent, so it
  works for any base color — verified across stocks with opposite masks (Ektar
  base R/B ≈ 2.75 orange; Phoenix base R/B ≈ 0.84 near-neutral). No knowledge of
  the mask color is needed.
- **It's *spatial* accuracy that bites, not color.** The per-channel maximum is
  the film base only if the rebate is in frame; otherwise it is the scene's
  deepest shadow — scene-dependent, slightly denser than true base, and
  *catastrophically* wrong if anything brighter than the base is in frame (clear
  overscan, light leak) → base overestimated → washed-out output.

**Ownership / superseding note (authoritative).** `auto-base-redesign` still
lists a "content-based source" sub-item **and includes it in its verification**;
that scope is **reassigned here**. The `film_base.source = "content"` enum
variant, the `--base-content` flag, its report wiring, and its tests are owned
**solely by this task** — so the two tasks do not both implement the same surface.
The `auto-base-redesign` owner must treat content mode as **out of scope** there,
and only *suggest* `--base-content` in the auto-refusal message. (That task file
can't be edited from here — agents are active on it — so this note plus the
`TASKS.md` checklist annotation are the authoritative redirect; its owner needs to
be told directly.)

## Design

- New `film_base.source = "content"` (flag `--base-content`). Per-channel high
  percentile (reuse `film_base::percentile`, ~p99; tune on real scans) over the
  whole image, or an optional sub-region.
- **Explicit opt-in only.** `--auto-base` failing must *not* silently fall through
  to content; it errors with a message naming `--base-content` (fail-loud, §11).
- **Report provenance.** The JSON report records `film_base.source = "content"`
  plus the resolved base, so content-derived rolls are auditable.
- **Guard the over-bright failure.** A high percentile resists dust specks but not
  a large clear region brighter than the base; where detectable (base near/above a
  plausible transmission ceiling), warn (`--strict`-promotable).
- Document the wash-out failure mode (foggy / high-key scene → no near-black →
  raised, cast blacks; recoverable downstream as a global cast, per §9).

## How to Verify

- Synthetic image with a known deepest-black patch → content base ≈ that patch.
- Real exposed frame with no rebate (e.g. Ektar `971`): `--base-content` yields a
  finite base and a plausible convert; compare the resulting cast against the
  roll's true `Dmin` (from the unexposed reference `963`) to quantify the bias.
- `--auto-base` on the same frame fails loudly and names `--base-content`.

## Dependencies

- [Film-base / Dmin estimation](film-base-estimation.md)
