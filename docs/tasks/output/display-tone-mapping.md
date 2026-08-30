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
- Reinhard costs a fixed **0.24 stops** of midtone (0.18 → 0.153), and the cost is the
  same at `W = 16` and `W = 64`, so it belongs to the operator rather than to `W` and the
  anchor can absorb it. Decide whether to absorb it or to pick an operator without it.

## Open questions

- **Which operator.** Reinhard is one of a family; it was the one probed. Its midtone
  penalty may be avoidable.
- **Where `W` comes from.** A fixed default, a per-stock value, or measured per roll. It
  is the specular headroom above diffuse white — `W = 64` sits about 3 stops up — so it may
  be a stock property, or a rendering-intent choice.
- **Which stage owns it.** The probe composed the operator into the reconstruction curve
  because `AcesCgImage` is only constructible inside `working_space`; that is a harness
  convenience, not a design. It belongs in render — see
  `algo/reconstruction-render-curve-split` for the boundary argument.
- **Whether `output/linear-render` and this are alternatives or complements.** Linear
  render pairs with a *bounded* reconstruction; a tone mapper pairs with an unbounded one.
  Shipping both means two coherent pipelines rather than one.

## How to Verify

- Re-run `shadow_metrics::tone_map_probe` and beat the shipped sigmoid on `blown%` **and**
  `code sep`, with midtone placement matched rather than confounded — the probe's own
  caveat, since operators that move midtones flatter themselves on those metrics.
- The HDR rendition measures a peak **below** the ceiling with non-zero separation above
  reference white, and the resulting `gain-map-hdr` reports `GainMapMax > 1.0`.
- Visual review on the fixture frames; the highlight metrics have disagreed with the eye
  twice already (see the cautions in `shadow_metrics`).
- Any default change carries a `pipeline_version` bump and a measured report.

## Dependencies

- [SDR display rendering](sdr-display-rendering.md)
- [Display-HDR rendering](hdr-display-rendering.md)
