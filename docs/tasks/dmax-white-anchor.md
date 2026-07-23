# Display-Range White Anchor (Dmax)

> **Target-pipeline terminology:** this completed task records the shipped
> legacy renderer, where `D' = Dmax` maps to `1.0`. In the replacement pipeline,
> Dmax belongs to the selected density curve: scalar placement for exponential
> and curve shaping for sigmoid. The later SDR/HDR render owns display reference
> white.

## Goal

Make the default `density` conversion fill the display range — scene white lands
at ≈ 1.0 instead of everything sitting above it — so a default u16 encode is
usable without hand-tuned `--print-exposure`. This closes the PR #12 review
finding ("default u16 output clips the whole image") and is the single biggest
default-quality gap versus Negative Lab Pro's per-channel auto-leveling (see the
NLP comparison, `progress.md`).

## Design

Today `render` computes `lin = 10^(γ·D')`, which anchors the film base (scene
black, `D = 0`) at `1.0` and puts all detail *above* it. Add a **white anchor**
`Dmax` — the corrected density of scene white — and render relative to it:

```text
lin = 10^(γ·(D' − Dmax))        # scene white (D' = Dmax) → 1.0, base → 10^(−γ·Dmax) ≈ black
```

(Equivalently a gain of `10^(−γ·Dmax)`; pick whichever composes more cleanly with
`print_exposure`, and document the choice.)

- **Knob shape:** one enum field, per the recipe conventions — e.g.
  `DmaxSource { Auto (default) | Explicit(f32 or [f32;3]) | None }` under the
  `density` recipe section, with matching CLI flags (`--d-max`, `--auto-d-max`,
  `--no-d-max` or similar; mutually exclusive via clap group, like
  `FilmBaseSource`). `None` preserves today's unanchored rendering (unity
  placement: base `1.0`, detail above) for float workflows; it does not recover
  physical scene values.
- **Auto measurement is deterministic**, like `film_base::estimate`: a high
  percentile (e.g. 99.x) of the corrected-density distribution, computed after
  `to_density`. Same input + params ⇒ same anchor ⇒ same output. Report the
  resolved value in the JSON report.
- **Unlike `Dmin`, `Dmax` is frame-local** — it is the frame's scene-white
  density, a property of the scene, not of the roll/scanner. Per-frame `Auto`
  is therefore the default. Reusing one frame's measured anchor across a roll
  (via explicit `--d-max`) is supported as a *deliberate batch-consistency
  choice* — the fixed-print-exposure look — but the docs must state the
  tradeoff plainly: darker frames render dim and denser highlights clip against
  a foreign anchor. It is **not** a calibrate-once pattern like Dmin.
- **Spec updates ride along:** correct design-spec §7.2's stage-3 polarity
  (`10^(−γ·D')` → `10^(+γ·D')`, per the algo-density progress note), document the
  anchor in §7.2, and add the keys to §9. `design-spec.md` is now the sole
  maintained design source.

## Implementation Suggestion

- The anchor applies at the `render` boundary; `to_density` is untouched. Keep
  the two sub-stages separate (core fidelity rule).
- Per-channel vs scalar Dmax: scalar preserves the color balance set by
  `density_scale/offset`; per-channel would also auto-color-correct but overlaps
  with `auto-neutral-wb` — recommend scalar here, revisit after that task.
- Percentile choice must ignore non-finite densities (NaN propagation from
  corrupt input) and should be robust to sprocket/rebate pixels — respect the
  same region conventions as film-base estimation if a sampling window is needed.
- Watch the four coupled spots for every new knob (CLI override, recipe struct,
  merge arm, validate) + merge tests.

## How to Verify

- Unit: known density plane + explicit Dmax → expected linear values; `None`
  reproduces current output bit-exactly; auto measurement on a synthetic ramp
  picks the expected percentile; determinism (two runs identical).
- Merge tests for each new knob; recipe keys land under the §9-assigned section.
- End-to-end on a real scan (`#[ignore]` throwaway or via orchestration): default
  u16 convert reports a small clipped-sample count (spot highlights only), not
  ~100% of pixels.

## Dependencies

- [Density-domain algorithm](algo-density.md)
