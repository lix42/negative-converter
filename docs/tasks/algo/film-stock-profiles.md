# Film-Stock Profiles

## Goal

Let a user name the film stock they shot and have reconstruction use that stock's
published reference densities instead of generic ones — without making stock
selection mandatory. The output is a selectable registry of known stocks carrying
the per-stock numbers reconstruction needs, each traceable to a manufacturer
datasheet, plus a generic C-41 fallback that keeps unknown stocks working.

## Design

**What a profile holds.** Only quantities that are genuinely stock-dependent and
published. From the Kodak datasheet research done under
[reference-anchored-sigmoid](reference-anchored-sigmoid.md):

- the *Judging Negative Exposures* aim densities — grey card and the lightest step
  of a paper grey scale (≈ diffuse white), Status M, red channel, **absolute**
  (base+fog included). These are **tabulated by the manufacturer**, so they are the
  authoritative half.
- the mid→white difference `Δ` derived from them (base-independent, since a
  difference cancels base+fog);
- a **nominal** `D-min`, for diagnostics only — see the two constraints below.

Tabulated aims: Ektar 100 0.82 / 1.18 (`Δ` 0.36), Portra 160 0.84 / 1.20 (`Δ` 0.36),
Portra 400 0.82 / 1.18 (`Δ` 0.36), Gold 200 0.95 / 1.35 (`Δ` 0.40).

**Constraint 1 — measured roll `film_base` stays authoritative; published `D-min` is
nominal only.** This repo already defines `Dmin` as a property of stock **plus
development plus scanner settings**
([estimate-reuse-output](../film-base/estimate-reuse-output.md)). Base fog and the
characteristic curve shift with processing, storage and the individual roll, so
selecting a stock must never substitute a nominal standard-process base for the
measured one — that would misplace tones on a real roll. Store published `D-min` as a
reference/diagnostic value and keep measured `film_base`, and any measured offset,
authoritative in the render path.

**Constraint 2 — the chart-read `D-min` figures are not Status M densities.** Status M
is a prescribed **broadband spectral response**, not a wavelength one can pick off a
curve. Deriving a Status M channel density requires converting the spectral-density
curve to transmittance, integrating against that channel's response, and then taking
the logarithm; single-wavelength sampling can be materially wrong where the dye spectra
overlap. The values read on 2026-08-02 (Ektar red ≈0.20, Portra 160 ≈0.17, Gold 200
≈0.22 — and the Portra 160 midscale read of 0.73 against a tabulated 0.79–0.89, which
is probably this effect showing up) are therefore **provisional**. Before any of them
becomes ground truth for this registry or for
[scanner density calibration](../io/scanner-density-calibration.md), either perform the
proper spectral integration or obtain a manufacturer-tabulated Status M measurement.

**A generic default is viable and matters.** The professional C-41 aims cluster
tightly (0.82 / 1.18 / `Δ` 0.36 across Ektar 100, Portra 160 and Portra 400), so an
unnamed stock can resolve to a generic C-41 profile and still render correctly. Most
users will not know or will not say; stock selection must therefore be a
*refinement*, never a precondition. A named stock that is not in the registry is a
loud error, not a silent fallback — but *no* stock named resolves to generic without
complaint.

**Data shape follows `pipeline/colorimetry/`.** That module is the established
pattern in this repo for reference data that must not drift silently: source data
with provenance, separately pinned literals the runtime reads, and a `#[cfg(test)]`
audit. Reuse the split rather than inventing a second convention. Every number carries
its publication id (E-4046, E-4050, E-4051, E-7022, …) and its **kind**:
`tabulated` (authoritative) or `chart-read` (provisional — not a Status M density at
all, per Constraint 2, so this is a different-quantity marker and not merely a precision
note).

**Knob shape.** One enum field, per the project convention that mutually exclusive
knobs are never parallel fields — e.g. `FilmStock { Generic (default) | Ektar100 |
Portra160 | Gold200 | … }`. It is a **conversion knob**, so it spans all four
coupled spots (CLI `*Overrides`, recipe `*Params`, a `merge` arm, a `validate`
check) and appears in the resolved report with provenance. Recipe placement follows
design-spec §9; `input.film_type` (`FilmType`) already exists as the *input-medium*
axis and is a different quantity — a stock profile is not a film type. Decide
explicitly whether the two compose or one constrains the other.

**Roll-fixed.** Stock is a property of the roll, so `nc roll` must treat it like
`film_base` and `reconstruction.curve.dmax`: a per-frame override is applied but
raises a loud, `--strict`-promotable warning.

## Implementation Suggestion

- Land the registry only *after* `reference-anchored-sigmoid` settles which
  parameters are stock-dependent. Building it earlier risks storing fields nothing
  reads.
- Prefer the **tabulated** aim numbers over anything read off a chart, and record which
  is which. Per Constraint 2, a chart read is not a Status M density at all — the ±0.05
  reading precision is the smaller of the two problems, so do not treat "read more
  carefully" as a fix.
- The aim tables are red-channel only. Per-channel placement is unresolved; do not
  invent per-channel numbers the datasheets do not state.
- Coordinate with
  [dense-base Dmax plausibility](../film-base/dense-base-dmax-plausibility.md): that
  task wants the C-41-calibrated plausibility floor made stock-relative (Harman
  Phoenix false-alarms today). It is deliberately *not* dependent on this task — it
  can loosen its floor without a registry — but the two must not solve
  stock-awareness twice.

## How to Verify

- A recipe naming each registry stock round-trips, and the resolved report shows
  the stock plus the reference densities it resolved to, with provenance.
- Omitting the stock resolves to the generic C-41 profile and renders identically to
  the pre-task default (byte-identical if the generic values equal the previous
  hard-coded ones — assert it, don't assume it).
- An unknown stock name fails loudly with exit 2 and lists the accepted names.
- A per-frame stock override under `nc roll` raises the roll warning and `--strict`
  promotes it.
- Every **tabulated** registry number is covered by a test asserting it against its
  recorded publication id, so a typo cannot ship silently; every **chart-read** number
  is marked provisional and a test asserts no render path consumes one.
- `cargo fmt --all --check`, `cargo clippy --all-targets -- -D warnings`,
  `cargo build`, `cargo test` pass.

## Dependencies

- [Reference-anchored sigmoid calibration and redesign](reference-anchored-sigmoid.md)
