# Conversion-analysis tooling (spike)

## Goal

Grow the [`real-scan-verify`](../../scripts/real-scan-verify/) harness into a
reusable **conversion-analysis toolkit**: an asset manifest, image-library-based
analysis of conversion results, and Negative Lab Pro (NLP) vs nc comparison — so
verifying and judging conversion quality is more powerful and easier to use.

**This is a spike first.** Discuss and decide the scope, structure, and tooling
before building; the deliverable is a short design note plus concretely-scoped
child tasks (or a decision to implement directly).

## Why

`real-scan-verification` produced a single staged bash harness driven by a
hard-coded roll→frame mapping (`ROLLS` in `harness.sh`), and the quantitative
analysis during that task (numpy/tifffile + ImageMagick: per-channel percentiles,
black/white points, saturation, clip %, JPG previews) was done ad hoc. To make
verification repeatable and to judge quality — including against NLP — that needs
to become tracked tooling.

## Scope (spike — decide before building)

1. **Asset manifest.** A tracked manifest of `../nc-assets/`: per roll, each
   frame's role (unexposed / fully-exposed leader / real), dimensions, format,
   `ir_present`, and a checksum. Drives the harness instead of the hard-coded
   `ROLLS` array, and lets us track every file. Decide schema, format (JSON/TOML),
   and how it is generated + validated.
2. **Conversion-result analysis.** Formalize the image-library analysis into a
   reusable script: per-channel percentiles, black/white points, contrast,
   saturation, clip %, and thumbnail/preview generation. Decide the metric set and
   the tooling (numpy+tifffile vs ImageMagick vs OpenImageIO vs a small Rust
   helper).
3. **NLP comparison.** Ingest NLP conversion outputs (the user will add them to
   `nc-assets`) and compare against nc's — metrics + side-by-side previews. Decide
   how to align them, since NLP and nc differ in color space, encoding, and
   framing (normalization / registration may be needed for a fair comparison).
4. **Harness UX.** Organize the scripts, provide a single documented entry point,
   and decide whether to stay in bash or move to a small Python/toolkit.

## Constraints

- **Tooling / analysis only** — does not touch the conversion pipeline or its
  determinism.
- **Never read sample pixels into an agent context** — derived numbers and
  downscaled thumbnails only, consistent with the harness invariant.

> Kept high-level on purpose: the manifest schema, metric set, comparison method,
> and language are exactly what the spike decides.

## Dependencies

- [Real-scan core verification](real-scan-verification.md) — provides the harness,
  frozen recipes, and the asset set this builds on.
