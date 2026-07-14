# Sigmoid / H&D-Curve Tone Algorithm

## Goal

Add the roadmap's third converter: an S-curve (photographic H&D / paper-response)
tone mapping in density space, giving shoulder/toe control comparable to NLP's
tone profiles (Cinematic/Lab families) instead of the density algorithm's straight
`10^(γ·D')` line. This is the main tone-shaping gap identified in the NLP
comparison (design-spec §12 roadmap item).

## Design

A new `Converter` implementation `algo/sigmoid.rs` that **reuses `to_density`**
(stages 1–2 are identical) and replaces the straight-line density→positive map
with a parameterized sigmoid anchored on the density range `[0, Dmax]`:

```text
1–2. to_density (shared with `density`)             D' ∈ [0, Dmax]
3.   S-curve: lin = S(D'; contrast, toe, shoulder)  S(0) ≈ black, S(Dmax) ≈ 1.0
4.   print render (shared PrintParams)
```

- Candidate curve: a log-domain logistic/sigmoid with `contrast` (slope at
  mid-density), `toe` (shadow compression) and `shoulder` (highlight roll-off)
  params — pick one concrete, documented formula (darktable's sigmoid and
  filmic's spline are reference points; record the exact equation in
  `progress.md`).
- Selected via the existing `Algorithm` enum (`--algorithm sigmoid`); params in a
  new `SigmoidParams` recipe section per §9 conventions (one section, no
  parallel bools). Spec §7 gains a §7.3 describing it; §9 gains its keys
  (design-spec.md and .html together).
- Print rendering stays the **separate** sub-stage it is today — the S-curve
  replaces stage 3 only; stage 4 (`PrintParams`) is shared and unchanged.

## Implementation Suggestion

- Depends on the Dmax anchor semantics: `S` maps `[0, Dmax] → [~0, 1]`, so build
  after `dmax-white-anchor` lands and reuse its resolved anchor rather than
  inventing a second one.
- Keep `simple` and `density` untouched — this is additive; the highlight
  soft-clip may be redundant under a shoulder (document the interaction, don't
  silently disable either).
- Property tests: monotonicity over the domain, endpoint anchoring, reduction to
  ≈ the linear map when toe/shoulder → 0 (so `density` remains the debuggable
  reference).

## How to Verify

- Unit: monotonic on `[0, Dmax]`; `S(0)`/`S(Dmax)` hit the documented anchors;
  neutral params ≈ straight-line map within tolerance; merge tests for every
  new knob.
- `--algorithm sigmoid` selects it end to end; JSON report names the algorithm
  and its resolved params.
- Real-scan spot check: midtone contrast visibly adjustable while highlights
  roll off smoothly (no hard clip at white), shadows keep separation.

## Dependencies

- [Algorithm interface](algo-interface.md)
- [Display-range white anchor (Dmax)](dmax-white-anchor.md)
