# Clipped Dmax Reference Handoff

## Background

Portra 400 roll `portra400-2026-08-04`, frame `20260803-film-1229`, is a confirmed
fully-exposed leader. Its holder-free center 80% has zero transmission in all
three channels of the primary RGB image. That is a valid dense-negative case:
the scanner recorded no visible light, so the true density is above its boundary
and cannot be recovered as an exact number. Today `nc estimate --d-max-region`
rejects the sample and therefore cannot produce a value for the normal
estimate-to-recipe-to-convert workflow.

## Goal

Let estimation report this censored measurement as a machine-readable
clipped/out-of-boundary Dmax state, and let `convert` and `roll` consume that
state through the ordinary recipe handoff using a deterministic, documented
fallback. Reports must keep the fallback distinguishable from a measured density.

## Opening Questions

- What name and serialized shape best express “the reference is denser than the
  scanner can measure” without pretending it is a numeric measurement?
- Is the provisional fallback of `1.3` still appropriate once more clipped
  leaders and output intents have been evaluated?
- How should a confirmed clipped leader be distinguished from the other causes
  of zero samples, such as holder selection, corruption, or a dead channel?
- What should warnings, `--strict`, and older recipes do with the new state?

## Suggested Approach

Treat clipping as a censored calibration outcome rather than a large measured
number. Carry that outcome explicitly from the estimate report into the recipe,
resolve its numeric fallback only when the film base and conversion context are
available, and report both the state and the resolved value. Retain loud failures
where the evidence does not establish that the selected region is a valid leader.
Use `1.3` as the provisional fallback: the Portra 400 region-only sweep from
`1.2` through `1.9` found `1.2`–`1.3` visually strongest, while the shipped
nominal roll-fixed Dmax is already `1.3`. Revisit the value when implementing the
task rather than treating this small visual sample as final calibration.

## References

- [Using nc: measuring Dmax](../../using-nc.md#step-3--optional-measure-dmax)
- [Roll-fixed Dmax reference contract](dmax-reference.md)
- [`DmaxSource` and recipe semantics](../../../src/types.rs)
- [`reference_dmax` measurement policy](../../../src/algo/density.rs)
- [`estimate` report and CLI orchestration](../../../src/cli.rs)

## How to Verify

Cover a non-clipped leader, a confirmed clipped leader, and an invalid zero-valued
region. Verify the estimate output can be reused without manual translation,
`convert` and `roll` resolve the same fallback deterministically, and reports do
not label that fallback as a measured Dmax. Re-run the documented workflow and
the Rust quality gates.

## Dependencies

- [Roll-fixed Dmax from a fully-exposed reference frame](dmax-reference.md)
