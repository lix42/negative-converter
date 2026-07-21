# Light film holder support

## Goal

Let film-base auto/border detection work when the film holder is **white** (light)
rather than the assumed dark surround, via an explicit CLI/recipe control.

## Background

Auto detection assumes the frame is surrounded by a near-black holder and that the
unexposed rebate is the bright band just inside it (see `auto-base-redesign`). Some
holders are white/light, which inverts that assumption — a bright surround would be
mistaken for the rebate. The polarity can't always be inferred reliably, so make
it an explicit knob.

**This is the RGB-only fallback to `ir-holder-detection`.** When IR-based holder
detection is active — an IR plane is present *and* the scan is explicitly declared
chromogenic (C-41) — IR separates the opaque holder from film *regardless of holder
colour* (a light holder is still opaque, so it reads dark in IR), so holder polarity
stops mattering and this knob is unnecessary (`ir-holder-detection` is the primary,
more robust path). This task covers the cases IR can't: **HDR 48-bit scans with no
IR plane**; **silver B&W film** (which blocks IR); and — because `ir-holder-detection`
defaults **off** unless the film type is explicitly declared chromogenic — any
**HDRi scan whose `--film-type` is unknown/undeclared** (the default C-41 workflow,
until the user opts in). It is the RGB path that `ir-holder-detection`'s dispatch
falls back to, which is why it now builds on that task.

## Design

Add a single mutually-exclusive knob (default `black`):

- CLI: `--holder white|black`
- Recipe key: `film_base.holder` (extends the `film_base` section; keep the
  `deny_unknown_fields` contract and the flag↔recipe↔merge↔validate wiring noted
  in `cli-framework`).

Thread it into the auto detector's RGB holder classification (the
`auto-base-redesign` path) so "holder" is classified by the configured polarity
(dark vs light surround) while the rebate remains the bright, uniform, inset band.
This is the branch `ir-holder-detection`'s dispatch falls back to when the IR mask
is unavailable or off; the IR mask supersedes it when active. Only affects `auto`;
`region`/`explicit` are unaffected.

## How to Verify

- `--holder white` on a synthetic light-holder `holder → rebate → picture` image
  finds the rebate; the default (`black`) would misfire on it.
- Round-trips through the recipe (`film_base.holder`) and rejects unknown values.
- Default behavior is unchanged for dark-holder scans.

## Dependencies

- [IR-assisted film-holder detection](ir-holder-detection.md) — `white-holder-support`
  is the RGB-only fallback for the no-IR path, so it builds on the
  holder-classification dispatch that task establishes (which in turn builds on
  [auto-base-redesign](auto-base-redesign.md), a transitive dependency).
