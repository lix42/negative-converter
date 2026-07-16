# IR-assisted film-holder detection

## Goal

Use the (already-decoded, currently-unconsumed) IR plane to produce a
content-independent **film-holder mask** — which of the 0–4 edges are occluded by
the opaque holder — so the film-base search runs only on actual film. IR cleanly
separates holder from film regardless of image density, which RGB alone cannot.

## Background

Auto-base's hardest confounder is that the **opaque holder is dark in RGB, and so
is dense / fully-exposed film** — RGB can't tell them apart, so a dark edge is
ambiguous. Infrared resolves it: C-41 color-negative dyes are **transparent to
IR** (the basis of Digital ICE), so in IR the *image density vanishes* and all
film — base, picture, even fully-exposed leader — reads bright, while the opaque
holder blocks IR and reads dark.

**Verification data (2026-07, Phoenix / Ektar HDRi scans):**

- Holder edge (Phoenix `933` top): RGB luma 0.004, **IR 0.023** (dark in both).
- Fully-exposed film (Ektar `1009`): RGB luma 0.016 (dark), **IR 0.587** (bright).
- Base / picture / rebate: RGB varies, **IR ≈ 0.6–0.7** (bright).

So the holder is the *only* thing dark in IR — a ~25× separation (0.02 vs 0.6),
i.e. a robust per-edge threshold. On Phoenix `933` IR correctly reads top =
holder, bottom/left = film, right = partial, matching the RGB heatmap.

**Superseding / related notes:**

- `auto-base-redesign` does per-edge holder classification in **RGB only**; this
  task **augments and may replace that holder-classification sub-step** with the
  IR signal when an IR plane is present.
- It also **largely sidesteps `white-holder-support`**: a light holder is still
  opaque, so it is dark in IR — holder *color* stops mattering, only opacity does.
  (Keep `white-holder-support` for the RGB-only / no-IR path.)

## Design

- When an IR plane is present, build a **holder mask** by thresholding IR **along
  each edge in segments** (per-tile / run-length down the edge), *not* a single
  per-edge mean — a holder can cover only *part* of an edge (e.g. Phoenix `933`
  right). Reducing a partially-covered edge to one label would either admit holder
  pixels into the rebate search or discard valid film. Feed the **film segments**
  to the RGB rebate detector (`auto-base-redesign`); exclude holder segments. A
  whole-edge label is just the degenerate all-segments-agree case.
- IR finds the holder, **not** the rebate — base / picture / leader are all bright
  in IR — so this is a masking pre-step, not a base estimator on its own.
- **Gate to C-41 color negative — by an explicit signal, not IR-plane presence.**
  An IR plane does **not** imply C-41: a silver B&W negative can be scanned as
  HDRi *with* an IR plane, and the decoded image carries no film-type signal —
  silver blocks IR, so dense silver regions are dark in IR and would be
  *misclassified as holder*. So enable this path only under an **explicit
  color-negative declaration** (e.g. a `--film-type` / color-model selection,
  coordinated with `bw-support`), default **off** when the stock is unknown or
  B&W. HDR 48-bit scans (no IR plane) fall back to RGB-only holder logic
  regardless.
- First real consumer of the IR channel (design-spec §6.1; roadmap item 1 dust
  removal is the other). Keep it a pure function over the decoded IR plane.
- Optionally surface the per-edge holder classification in `nc inspect`.

## How to Verify

- Phoenix `933`: top = holder, bottom/left = film, and the **partially-covered
  right edge** is split into holder vs film *segments* (not one whole-edge label)
  — only the film segments reach the rebate search.
- Ektar `1009` (fully-exposed) is classified all-film (bright IR) despite being
  dark in RGB — the disambiguation RGB can't make.
- An HDR 48-bit scan (no IR) falls back to the RGB-only path without error.

## Dependencies

- [Robust auto film-base detection](auto-base-redesign.md)
