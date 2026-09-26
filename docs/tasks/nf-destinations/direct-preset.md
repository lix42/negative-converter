# The direct destination for external editing

## Goal

A render that does as little as possible, for a workflow that continues in
Lightroom or Photoshop: identity scene correction, an empty look, Adobe RGB, and
only the fit range needed to land in the container.

## Design

What is known:

- **Its two properties come from the decode, not from rendering** (design-update
  Part 2): mid-grey lands mid because of the `mid-at-base-offset` anchor, and the
  cast stays within an acceptable range on any stock because of the `scale`
  calibration. *Acceptable, not perfect* — closing the remainder is a grade this
  destination deliberately does not apply.
- **"Minimal" cannot mean "no tone".** Adobe RGB ends at 1.0 and a real decode
  exceeds it, so a compression is still a choice — just a fixed, documented one.
  This is what makes Adobe RGB a must-have output.
- **It is also the rendering the calibration loop holds fixed** (Part 3): with
  scene correction identity and the look empty, what the eye judges is the decode.
  That second consumer wants stability across review rounds.
- **It is not `film-master`**, which is linear, exceeds 1.0 and cannot be judged by
  eye; this one is viewable by construction.

- **The gamut is ready** ([`output/adobe-rgb-gamut`](../output/adobe-rgb-gamut.md),
  2026-09-24): pass `DestinationGamut::AdobeRgb` and the chain maps into it and the
  encoder writes the Adobe RGB transfer and profile. What is left is the selection,
  the `nctool` `PRESET_SPACES` row, and the memory question below.

Open:

- **`RunProfile`: does it share `NewFlowU16Tiff`** (named `NewFlowSdrTiff` until
  2026-09-25)? Same buffers and shape as the
  Display P3 destination (a matrix change inside an in-place map, and a transfer), so
  it should — confirm by measurement, as `nf-destinations/memory-profiles` requires.
- **Is it a named destination or a rendering profile** that other destinations can
  also resolve (design-update Part 2 open questions)? If the loop holds it fixed
  while destinations change, a profile is the better shape.
- **How much fit range is "only what is needed"?** A gentle fixed compression, or
  reinhard at a pinned headroom — documented as a choice either way, and not moving
  between calibration rounds without a note.
- Whether the empty look is genuinely empty — the 2026-09-17 measurement is that
  per-channel highlight compression flatters any cast, which is precisely what this
  destination must not do.

- **2026-09-25 (`preset-set`):** a destination is now separate knobs, so Adobe RGB is a
  value of `--gamut` (`--new-flow --gamut adobe-rgb` writes the SDR TIFF). What is
  left here is the *rendering*, and the "named destination or rendering profile"
  question now leans profile: range and gamut are knob values, not part of a name.

## How to Verify

- On a correctly exposed frame, mid-grey lands mid without a per-frame correction,
  across stocks.
- Two calibration candidates rendered through it differ only in the decode — no
  scene correction or look knob is reachable, or reachable ones are proven inert.
- Output reads as Adobe RGB under `nctool metrics --space adobe-rgb`.

## Dependencies

- [The destination set](preset-set.md)
- [Adobe RGB (1998) as an output gamut](../output/adobe-rgb-gamut.md)
