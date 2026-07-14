# Regional (Shadow/Highlight) Color Balance

## Goal

Correct color **crossover** — casts that differ between shadows and highlights
(expired film, misprocessing, mixed lighting), which a single global per-channel
gain/offset cannot fix. Equivalent of NLP's Shadows/Highs color toning, expressed
as deterministic density-domain parameters.

## Design

Density space makes this natural: a cast that varies with tone is a per-channel
offset that varies with density. Extend the density-correction sub-stage (stage
2) with density-weighted offsets:

```text
D'_c = scale_c·D_c + offset_c + shadow_balance_c·w_lo(D_c) + highlight_balance_c·w_hi(D_c)
```

where `w_lo`/`w_hi` are smooth, documented weight ramps over the density range
(low density = scene shadows, high density = scene highlights — note the
negative's inversion when naming the knobs from the *positive*'s point of view;
pick the user-facing convention deliberately and document it in §9).

- New params in the `density` recipe section: `shadow_balance: [f32;3]`,
  `highlight_balance: [f32;3]` (defaults `[0,0,0]` = today's behavior,
  bit-exact), plus CLI flags — four coupled spots + merge tests each.
- Weight ramps anchored on the resolved density range (film base at `D = 0`;
  `Dmax` if available) so the "shadow" and "highlight" regions track the actual
  image range rather than fixed constants.
- Stays entirely inside stage 2 — print rendering untouched (core fidelity
  rule); `simple` untouched.
- Spec: §7.2 formula and §9 keys updated (design-spec.md and .html together).

## Implementation Suggestion

- Keep the ramps simple and invertible-in-the-head (e.g. smoothstep over
  documented density breakpoints) — every constant exposed as a param or
  documented anchor, no magic numbers.
- A neutral-default regression test (`[0,0,0]` ⇒ identical output to current
  `to_density`) protects existing behavior and recipes.
- Interaction with `auto-neutral-wb`: that task fixes the *global* cast; this
  one the tone-dependent residual. Order of application is stage 2 (here) before
  print WB — document it.

## How to Verify

- Unit: synthetic crossover (opposite casts injected at low/high density) is
  neutralized by matching balance params; zero params ⇒ bit-exact with current
  output; weights are smooth/monotonic over the range.
- Merge tests for the new knobs; recipe keys under the `density` section.
- Real-scan spot check on a frame with visible shadow/highlight crossover.

## Dependencies

- [Density-domain algorithm](algo-density.md)
