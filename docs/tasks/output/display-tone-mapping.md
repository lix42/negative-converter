# Display Tone Mapping

## Goal

Give each display renderer a real tone-mapping operator with a stated **white point**,
replacing the fixed-ceiling Hermite knee that cannot hold content overshooting by more
than about a stop.

## Why

Measured 2026-08-28 under `algo/exponential-anchor-placement`, on the committed fixture
frames:

- Both renderers compress with a Hermite whose ceiling is fixed (1.0 for SDR, 4.926 for
  HDR) and which reaches it with zero slope. Content several times over lands on the
  ceiling with **zero separation** — on an unbounded reconstruction, 20.8% of the frame
  on SDR and the same content pinned at exactly 4.926 on HDR.
- **Moving the knee does not help.** `highlight_compress` at 1 and 4 moved the shipped
  default 6.45% → 6.64% blown and a shoulder-less reconstruction 21.38% → 21.58%. It made
  every config tried slightly worse and rescued none.
- **A real operator does help.** Extended Reinhard `v(1 + v/W²)/(1 + v)` at `W = 64` beat
  the shipped sigmoid on **both** metrics on **both** probe frames — Ektar 971 6.24% blown
  / 21.4 code separation against the sigmoid's 6.86 / 11.6, and Portra 1121 6.08 / 3.0
  against 6.30 / 0.6.
- **Global beats knee-based.** The hyperbolic and Hermite forms reserve only `1 − t` of
  output range for everything above the knee, which is not enough: hyperbolic `t = 0.85`
  left 27.7% blown where Reinhard left 6.1%.

**Read those numbers with one correction (found 2026-09-01).** `tone_map_probe` composed
each candidate operator into the *reconstruction* and then rendered through
`sdr::render`, which applied the shipped Hermite **on top of every row** — including the
`none (X3 baseline)` and `reinhard W=64` rows. The ranking survives (all rows shared it),
but no row measured its operator alone. `--display-tone none` (`output/linear-render`) is
now the clean baseline: re-run the probe with it before treating any of these figures as
the operator's own.

## Design

Not predetermined. Extended Reinhard is the candidate that measured well, not a decision.
What the probe established about the shape:

- **`W` is a white point** — the input that maps exactly to display white
  (`reinhard(W, W) = 1.0`). State it as a **density** rather than a linear multiple
  (`W = 10^(contrast·(D − A))`), so it is contrast-independent and roll-measurable, and so
  it reuses the vocabulary `AnchorPlacement` already established. It is effectively a
  second anchor, for the white end.
- The parameters then have three separate jobs — `contrast` the slope, the anchor where
  mid or black sits, `W` where white sits — which is the property the sigmoid's
  `contrast`/`toe`/`shoulder`/`anchor` lacks, since its knees and anchor interact.
- **Per-output ceilings are the point.** The same operator with the ceiling at 1.0 for SDR
  and 4.926 for HDR makes the two renditions agree on midtones and differ only above
  diffuse white — the condition under which a gain map carries information. Today every
  **shouldered** sigmoid config measures `HDR peak = 1.000` and an inert gain map; the
  shoulder is what removes the above-white content, so `--sigmoid-shoulder 0` reaches
  4.87x like the shoulder-less exponential.
- Reinhard costs **0.24 stops** of midtone (0.18 → 0.153), the same at `W = 16` and
  `W = 64`, so it belongs to the operator rather than to `W` and the anchor can absorb it.
  Decide whether to absorb it or to pick an operator without it.
- **Corrected 2026-09-02: that cost is not fixed across the tone scale, and the shipped
  wording said it was.** It is W-independent but strongly value-dependent, because
  extended Reinhard is defined to map `W → 1.0`, which puts `1.0 → ~0.5`: **0.24 stops at
  middle grey and a full 1.000 stops at diffuse white.** This is the whole explanation for
  "every tone-mapped render looks darker than the default" in the 2026-08-31 visual review
  — it is the operator's construction, not the blue cast reviewed alongside it, and the
  matched-midtone protocol hid it by matching at 0.18 where the cost is smallest. Whether a
  stop at diffuse white is acceptable is a **rendering-intent** decision and the main thing
  the HDR review should answer; renormalizing so diffuse white returns to 1.0 would undo
  the compression that buys the headroom, so the two cannot both be had from this operator.

## Open questions

- **Which operator.** Still open. Extended Reinhard shipped as the first one; its
  penalty above is confirmed, quantified, and *not* avoidable within this operator.
- ~~**Where `W` comes from.**~~ **Answered: a fixed default, spelled in stops.** It ships
  as a rendering-intent choice, not a stock property — `--display-tone-headroom`, default
  **6 stops**, carried by a checked `Headroom` newtype. Deferring it to a per-stock or
  per-roll value stays possible and nothing here forecloses it.
  *The prior text said `W = 64` "sits about 3 stops up"; it is 6 stops
  (`log2 64`), and mixing density-referred with display-referred stops is what produced
  the wrong figure. The flag is spelled in stops so the two cannot be confused again.*
- ~~**Which stage owns it.**~~ **Answered: render.** It lives in `pipeline/display_tone.rs`
  and is applied by `pipeline::sdr` and `pipeline::hdr`; nothing was composed into the
  reconstruction curve. The harness convenience the probe used did not survive into the
  design, as expected.
- **How this composes with `output/linear-render`, which shipped 2026-09-01.** They are
  complements, and the relationship is now concrete rather than open: `print.display_tone`
  is the selector, `shoulder` and `none` are its first two values, and a real operator is a
  **third value of the same knob** — not a parallel one. Two consequences worth knowing
  before designing the surface:
  - The recipe needs **no migration**. Unit variants are bare strings under serde's default
    externally-tagged form, so `reinhard { white }` arrives as `{"reinhard": {…}}` while
    `"shoulder"` / `"none"` keep their spellings (the shape `WbSource` / `DmaxSource`
    already use). A test pins that wire form; do not "tidy" it into `{"type": …}`.
  - The **CLI** is what changes: `clap::ValueEnum` cannot derive over a variant with a
    payload, so the selector needs its own parse fn (the `OutputPreset::parse` pattern) plus
    a flag for `W`, exactly as `DmaxSource` is spelled (`--d-max` / `--auto-d-max`).
  - Put `W` **inside** the variant. The shoulder's knee width could not go there —
    `print.highlight_compress` is shared with the legacy path's above-`1.0` soft clip, where
    it means something else — and that flat pairing is why a contradiction rule
    (`highlight_compress` beside `none`) had to be written at all.
    **The prediction that followed — "a parameter carried by its own variant needs no such
    rule" — did not hold, and the reason generalizes.** Nesting removes the *illegal-state*
    rule, since no other variant can carry a headroom. It does not remove the *lossy-merge*
    rule: `--display-tone-headroom` beside a tone switch that discards it is still a flag
    silently doing nothing, so `display_tone_switch_dropped_headroom` had to be written on
    both the `convert` and `roll` paths. Nesting buys correct *modelling*, not fewer rules —
    a flag can be meaningless without the recipe being able to express a contradiction.
- ~~**Whichever operator ships should close the AVIF report gap it inherits.**~~
  **Closed.** `avif.rendering` now carries the luminance anchors and the renderer's pinned
  identifiers, nested rather than flattened because — unlike every other field in that
  block — it is declared policy, not facts read back out of the file. As this entry
  predicted, the tone *selector* was never the gap; `output_render.display_tone` already
  covered it, and the block now also states what the renderer applied.

## How to Verify

- Re-run `shadow_metrics::tone_map_probe` **under `--display-tone none`**, so the operator
  is measured alone rather than under the shipped Hermite (see the correction in *Why*), and
  beat the shipped sigmoid on `blown%` **and** `code sep`, with midtone placement matched
  rather than confounded — the probe's own caveat, since operators that move midtones
  flatter themselves on those metrics. `shadow_metrics::linear_render_probe` is the
  shouldered-vs-none comparison already written in that shape.
- The HDR rendition measures a peak **below** the ceiling with non-zero separation above
  reference white, and the resulting `gain-map-hdr` reports `GainMapMax > 1.0`.
  **Read this as the conjunction it is — the last clause alone is not evidence.**
  `--sigmoid-shoulder 0` on its own already reaches 4.866x, because the *reconstruction's*
  shoulder is what removes above-white content; but that is 98.8% of the 4.926 ceiling with
  the speculars fused into one flat plateau. The separation clause is what only an
  unbounded operator satisfies, and it is the one to measure.
- Visual review on the fixture frames; the highlight metrics have disagreed with the eye
  twice already (see the cautions in `shadow_metrics`).
  **Done — SDR 2026-08-31, HDR 2026-09-02** (`scripts/hdr-tone-review/`). The HDR verdict:
  shoulder-less reconstruction under this operator **preferred** over both the shipped
  default and shoulder-less reconstruction under the old knee. The metrics and the eye agreed
  this time, but only after the metric was changed — `GainMapMax` calls the two live configs
  equivalent (4.87x vs 4.79x, identical on every frame) and the plateau share separates them
  (6.6–15.2% vs 0.26–0.61%). Read as accepting the rendition, **not** as three other
  decisions it is easily mistaken for: the preferred config includes
  `--sigmoid-shoulder 0`, a *reconstruction* change owned by
  `algo/reconstruction-render-curve-split`; nothing about the default moved; and the
  1.000-stop diffuse-white cost was flagged on the review page and is accepted *for this
  rendition*, not endorsed as a general rendering intent.
- Any default change carries a `pipeline_version` bump and a measured report.

## Dependencies

- [SDR display rendering](sdr-display-rendering.md)
- [Display-HDR rendering](hdr-display-rendering.md)
