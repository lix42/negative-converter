//! The fixed, stock-agnostic decode (`nf-reconstruction/fixed-decode`,
//! `docs/design-update.md` Part 1) — the reconstruction the new flow runs.
//!
//! ```text
//! D_c  = −log10(scan_c / base_c)          measurement
//! D′_c = scale_c · D_c + offset_c          calibration
//! out_c = 10^(linearization · (D′_c − A))  the curve
//! ```
//!
//! One decode for every negative: a straight line in density against log exposure,
//! with the film's toe passed through as recorded. Every step is strictly
//! increasing and unclamped in f32, so **nothing is lost by construction** — which
//! is why these settings are conventions rather than accuracy questions, and why
//! they cannot be judged by "how much survived". Everything about how the picture
//! should look belongs to rendering.
//!
//! **Written fresh, not extracted.** Per CLAUDE.md's migration rule this does not
//! reuse [`density::reconstruct`]'s seams or its `DensityImage` intermediate: the
//! two passes are fused into one, so the decode allocates one buffer where the
//! legacy path allocates one and transforms it again. It lives in `algo` rather
//! than `pipeline` because [`FilmRgbImage`] is the typed boundary every
//! reconstruction path produces and only this module tree may mint one; the 3×3
//! into ACEScg stays `pipeline::working_space::map_nc_film_rgb_v1`.
//!
//! **Fusing is bit-exact, and stays that way only under four rules.** Each step
//! already rounds to f32 at the same points as the staged path, and Rust does not
//! contract to FMA — so: keep the f32 division *before* the `log10`; add `+ offset`
//! unconditionally, never skipped because the constant is zero; never `mul_add` (an
//! FMA would keep one product in extended precision and bit-differ); and apply the
//! anchor **inside the exponent** rather than factoring it into a `10^(−linearization·A)`
//! gain, which overflows f32 when `linearization · D′` alone leaves the pow10 range even
//! though the anchored exponent is small.
//!
//! **What each rule is worth, measured — one vector cannot hold four rules.** Each
//! count below is that rule broken on purpose and bit-compared over the 12 samples of
//! `tests::scan`:
//!
//! - **the division** — 1 of 12 at the shipped offset, 0 of 12 at a non-zero one. A
//!   single sample (`0.5 / 0.9`), so `tests::scan` carries a note saying so.
//! - **the offset add** — 0 of 12 at the shipped `[0, 0, 0]`, 9 of 12 at a non-zero
//!   offset. At zero, skipping the add is unobservable in the *output*: the `−0.0` it
//!   normalizes cannot survive the curve, which subtracts the anchor from it. So this
//!   rule is held only by the non-zero pass — and it goes live the moment
//!   `nf-calibration/offset-question` moves the constant.
//! - **the FMA** — 0 of 12 at **either** offset, so the equality test cannot hold it
//!   at all.
//! - **the factored anchor** — 6 and 9 of 12; held at either offset.
//!
//! Hence `tests::the_fixed_decode_matches_the_equivalent_legacy_configuration` runs
//! the legacy comparison at two offsets (neither alone holds both of the first two
//! rules), and the FMA has a test of its own,
//! `tests::the_decode_never_contracts_the_calibration_into_an_fma`, swept over the
//! band just under the film base where the two forms do part.
//!
//! # Five quantities, and none of them are interchangeable
//!
//! They are routinely collapsed into "dmax" or "white". They are not the same
//! number, they do not come from the same place, and only one of them is an input
//! here:
//!
//! | name | what it is | where it comes from | status |
//! |---|---|---|---|
//! | **film base** (`Dmin`) | per-roll transmission of unexposed film | measured from that roll's rebate | an input; `D′ = 0` by construction |
//! | **leader `Dmax`** | film **saturation** density | a light-struck leader; `estimate --d-max-region` measured it until `nf-retire/dmax-machinery` | **retired**; nothing reads it |
//! | **anchor `A`** | the corrected density that renders to `1.0` | derived: [`AnchorRule::anchor`] | `0.9924` at the defaults; reported |
//! | **diffuse white** | scene white on a correctly exposed negative | the datasheets: `d + types::REFERENCE_MID_TO_WHITE_DELTA` | `0.98` — a reference number, not an input |
//! | **content white `W`** | a roll's bright end (red p97 of picture density) | `docs/spike/white-placement.md` | **not measured, not shipped** |
//! | **specular headroom** | ~1 stop above diffuse white | where an HDR rendition lives | a consequence, not a knob |
//!
//! The anchor rule here is *reference-free*: it never reads a leader `Dmax`, which
//! is what keeps that measurement's roll-to-roll error (0.295 density between two
//! rolls of one stock, against bases agreeing to 0.0005) out of the render.
//! [`DecodeReport::reads_reference`] states that as a fact the caller can read
//! rather than a claim it has to trust.
//!
//! **What the white-placement shortlist would need, and where.** Of the four
//! placements costed in `docs/spike/white-placement.md`, **C** — the only one that
//! pins mid *and* white — needs nothing from this module: it solves its contrast
//! from a **content** white and the spike puts that per-roll contrast in the look
//! stage — `look.contrast`, as `gamma / LINEARIZATION` — leaving the decode fixed.
//! **B** and **D** would have handed this module a measured density.
//! `nf-calibration/anchor-comparison` chose neither: its rule, a bounded C, places the
//! roll's white through `look.contrast`, so no content-referenced variant is planned.
//! [`AnchorRule`] stays an enum with one variant rather than a bare `f32` field, so a
//! future one would add a variant instead of silently changing what a number means.
//! No white reference is measured, read or representable here.

use crate::algo::{FilmRgbImage, density};
use crate::pipeline::pixels;
use crate::types::{FilmBase, LinearImage, MID_GREY_OUTPUT_DECADES, NcError, Result};

/// Mid-grey's density above the film base — the decode's one anchor constant.
///
/// A **convention, not a per-stock value**: stocks measure 0.542–0.699, ≈1.06 stops
/// at the default total contrast 2.0, and choosing per stock would be per-stock
/// exposure normalization inside a decode declared stock-agnostic. A fixed value lets film speed show
/// through, which is the faithful behaviour.
///
/// **Where the number comes from:** `generic-c41`'s mid-grey aim, 0.624 above base on
/// red — the averaged curve's own aim, inside the nine stocks' 0.542–0.699 — rounded.
/// It is **hand-frozen**, never read from the datasheets at runtime: a datasheet input
/// would invite exactly the per-stock variation this constant exists to refuse, and the
/// published `D-min` stays diagnostic-only (`algo/film-stock-profiles`, Constraint 1).
/// `crate::film_stock`'s `the_fixed_decode_mid_is_the_generic_aim` ties the two together.
///
/// The value is today's pick. `nf-calibration/anchor-comparison` left it in place — the
/// roll's white acts through the look's contrast — but a later calibration may move it.
/// That changes every `--new-flow` render, and nothing versions it yet —
/// `pipeline_version` tracks the default render only, and a new-flow fingerprint is
/// `nf-verification/fingerprints`'. It stays reachable as `--anchor-mid-offset`.
pub const MID_ABOVE_BASE: f32 = 0.62;

/// Where diffuse white sits in the **graded** image — the look's output, which every
/// stage from the look on reads.
///
/// At the look's default contrast the chain renders a neutral exactly where the bundled
/// decode did ([`BUNDLED_CONTRAST`]), whose anchor `10^(2.0 · (D′ − A))` is `1.0` at
/// `D′ = A` and lands ≈0.08 stop from the datasheets' diffuse white
/// (`tests::the_anchor_is_the_documented_number`) — so `1.0` **is** diffuse white there,
/// to that tolerance. **Not at the decode's own output**: at the linearization alone the
/// anchor sits higher (`A = d + 0.745/1.8`) and the datasheets' white decodes to ≈0.80,
/// a third of a stop under; the look's contrast, pivoted at mid, is what lifts it. A
/// stronger look contrast lifts it further, which is what print contrast does. A
/// convention rather than a measurement of a frame: scene correction's exposure moves
/// the picture, not this number.
///
/// **The one definition every rendering stage keys on.** It is scene-referred and
/// common to both display branches, which is why fit range's HDR lift starts here and
/// why the look's highlight desaturation (`nf-look/path-to-white`) anchors its
/// threshold here too — a second spelling of it in either stage would let the two
/// disagree about where white is.
pub const DIFFUSE_WHITE: f32 = 1.0;

/// The decode's slope: **the calibrated half of `gamma`**, which linearizes the film.
///
/// A C-41 negative's straight line rises ≈0.55 density per decade of exposure, so
/// `1/0.55 ≈ 1.8` undoes it. That is a calibration and stays in the decode; how
/// contrasty the *picture* is is a look, `look.contrast`
/// (`nf-reconstruction/gamma-split`). Fixed, not per stock: the registry's red film
/// gammas span 0.53–0.61, and choosing per stock would normalize tone character inside
/// a decode declared stock-agnostic.
///
/// **Only the products `LINEARIZATION · scale_c` reach the curve**, so the two are one
/// measurement split by the convention `scale_r = 1` ([`DENSITY_SCALE`];
/// `tests::the_scale_convention_pins_red_to_one`). The decode owns every per-channel
/// exponent; the look owns one factor shared by all three. Moving this value moves the
/// calibration unless `scale` moves with it — `nf-calibration/scale-gamma-loop` tunes
/// the two together, never one alone.
///
/// Reachable as `--density-gamma` (recipe `reconstruction.linearization`). A current
/// pick, expected to move with `scale`.
pub const LINEARIZATION: f32 = 1.8;

/// The single `gamma` nc shipped before the split: linearization and print contrast
/// bundled into one number.
///
/// Two readers. The current chain has no look stage, so its default curve still
/// carries the whole bundle (`types::ExponentialParams::default`), and moves no pixel
/// until `nf-core/default-flip` retires it. And the look's default contrast is defined
/// from it — `BUNDLED_CONTRAST / LINEARIZATION` — so the new flow's default renders a
/// neutral where the bundled decode did.
pub const BUNDLED_CONTRAST: f32 = 2.0;

/// The per-channel density calibration — **one global value**, never varied per
/// stock, roll or frame. What one value cannot reach is a rendering correction, not
/// a second decode.
///
/// Calibrated from 31 hand-marked neutral patches over five rolls (2026-09-16),
/// averaged with equal weight per roll. It is a fit of *our* chain — one scanner,
/// two developers — shipped as a **default prior**, not as anyone's calibration;
/// the residual belongs to `io/scanner-density-calibration`.
///
/// The current chain's `DensityParams::default` reads this constant (since
/// `pipeline_version` 6), so both chains default to one gain;
/// `tests::the_current_chains_default_is_this_decode_at_the_bundled_contrast` pins it.
pub const DENSITY_SCALE: [f32; 3] = [1.0, 0.84, 0.73];

/// The per-channel density offset. `[0, 0, 0]` is **a pick, not a closed question**:
/// the term is real — the gap between density measured from the rebate and the
/// density where the three layers correspond to equal exposure, which the
/// datasheets carry — but a 2026-09-17 review rejected both candidate values, and
/// the data that could identify one (density varied at a single illuminant) does not
/// exist yet. `nf-calibration/offset-question` owns it.
pub const DENSITY_OFFSET: [f32; 3] = [0.0; 3];

/// Floor applied to the scan transmission before the `log10`, so a dead pixel
/// becomes a very high but finite density rather than `-inf`/`NaN`. `1e-6` ≈ −20
/// stops below unity: darker than any real detail, with headroom before the curve
/// can overflow f32.
///
/// It floors every **finite** sample below it — non-positive, denormal and ordinary
/// normal positives alike (`1e-6` is itself normal in f32). A **non-finite** sample is
/// not floored: it propagates as `NaN`, so `io::encode`'s non-finite counter still
/// surfaces corrupt input rather than seeing a plausible pixel.
pub(crate) const SCAN_FLOOR: f32 = 1e-6;

/// Which tone the decode pins, and at what density.
///
/// One variant today, and that is the point: a bare `f32` field would let a
/// content-referenced placement (`docs/spike/white-placement.md` B and D, neither of
/// them planned) reuse the same number for a different quantity. See the module docs.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum AnchorRule {
    /// Pin **mid-grey** (output 0.18) at `d` density above the film base, letting
    /// white fall where the contrast puts it.
    ///
    /// Mid-grey is what "exposed correctly" means, it is reference-free, and pinning
    /// mid makes contrast and exposure independent: changing the contrast pivots
    /// about mid instead of moving the whole image. Pinning black instead would
    /// leave midtone brightness depending on contrast, and the base is fog rather
    /// than scene black anyway.
    // Spelled as the current chain spells the placement it reproduces, so a
    // recipe reads one name across both flows (see [`AnchorRule::name`]).
    #[serde(rename = "mid-at-base-offset")]
    MidAboveBase(f32),
}

impl AnchorRule {
    /// The corrected density that renders to `1.0`.
    ///
    /// Solving `10^(linearization·(d − A)) = 0.18` gives `A = d + 0.745/linearization`.
    /// This is the **only** definition of the anchor in the new flow: the report reads it
    /// from here rather than recomputing it, so a report cannot document a number the
    /// render did not use.
    pub fn anchor(self, linearization: f32) -> f32 {
        match self {
            AnchorRule::MidAboveBase(d) => d + MID_GREY_OUTPUT_DECADES / linearization,
        }
    }

    /// The rule's name for the report. Matches the legacy placement it reproduces
    /// (`mid-at-base-offset`), so a reader comparing the two flows sees one name.
    pub fn name(self) -> &'static str {
        match self {
            AnchorRule::MidAboveBase(_) => "mid-at-base-offset",
        }
    }

    /// Whether resolving this rule consumes a reference density. Always `false`
    /// today, and stated rather than assumed: it is the property the whole anchor
    /// design rests on, and a content-referenced variant, were one ever added, would
    /// have to return `true` here instead of quietly changing what the decode reads.
    pub fn reads_reference(self) -> bool {
        match self {
            AnchorRule::MidAboveBase(_) => false,
        }
    }
}

/// The decode's parameters — measurement, calibration, curve.
///
/// Also the new chain's recipe section `reconstruction` (`crate::recipe`), field for
/// field: the recipe spells exactly what the decode reads, so there is no mapping to
/// drift. Omitted keys take [`Default`], which is these module constants.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DecodeParams {
    /// Per-channel density gain — the calibration of the common slope error the
    /// film-base division leaves behind.
    pub scale: [f32; 3],
    /// Per-channel density offset.
    pub offset: [f32; 3],
    /// The straight line's slope in density against log exposure — the calibrated
    /// half of `gamma` ([`LINEARIZATION`]). Print contrast is the look's
    /// (`look.contrast`), never this.
    pub linearization: f32,
    /// Which tone is pinned, and where.
    pub anchor: AnchorRule,
}

impl Default for DecodeParams {
    fn default() -> Self {
        Self {
            scale: DENSITY_SCALE,
            offset: DENSITY_OFFSET,
            linearization: LINEARIZATION,
            anchor: AnchorRule::MidAboveBase(MID_ABOVE_BASE),
        }
    }
}

/// What the decode resolved, for the report.
///
/// The values, not new knobs. The CLI serializes it as the report's
/// `new_flow.decode` block.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize)]
pub struct DecodeReport {
    /// The corrected density that rendered to `1.0`, and therefore what sets the
    /// black floor at `10^(−linearization·anchor)`.
    pub anchor: f32,
    /// The anchor rule's name.
    pub anchor_rule: &'static str,
    /// Whether a reference density was consumed. Always `false` — see
    /// [`AnchorRule::reads_reference`].
    pub reads_reference: bool,
    /// The resolved linearization and calibration, so a render is reproducible from
    /// its own report.
    pub linearization: f32,
    pub scale: [f32; 3],
    pub offset: [f32; 3],
}

/// Decode a scan into the typed film positive.
///
/// Pure and total in its inputs: a degenerate film base or an unusable parameter
/// surfaces as an [`NcError`], never a silently-wrong image. Values are unclamped
/// and non-finite samples ride through for `io::encode` to count — the clamping
/// boundary is the u16 encode and nowhere else.
pub fn decode(
    image: &LinearImage,
    base: &FilmBase,
    params: &DecodeParams,
) -> Result<(FilmRgbImage, DecodeReport)> {
    // The base is divided by, so a zero / negative / non-finite one would yield a
    // silently-black or non-finite image. The CLI validates an *explicit* base; an
    // auto/region-estimated one is only guarded at its consumption point, which is
    // here. Shared with the legacy path deliberately: the film base is the one input
    // both flows measure the same way, so one message serves both.
    density::check_base(base)?;
    let anchor = check_params(params)?;

    let base = [base.r, base.g, base.b];
    let DecodeParams {
        scale,
        offset,
        linearization,
        ..
    } = *params;

    // Fused measurement + calibration + curve, one pass. The driver is fallible and
    // this body is not — there is no per-pixel failure in a decode that loses
    // nothing by construction — so every pixel returns `Ok`; the cost is the wrapper.
    let rgb = pixels::try_map(&image.rgb, |_, px| {
        let mut out = [0.0_f32; 3];
        for c in 0..3 {
            let s = px[c];
            let d = if s.is_finite() {
                -(s.max(SCAN_FLOOR) / base[c]).log10()
            } else {
                f32::NAN
            };
            // Not `mul_add`, and `+ offset` is not skipped at zero. See the module
            // docs: both are bit-identity rules, not style.
            let corrected = scale[c] * d + offset[c];
            out[c] = 10f32.powf(linearization * (corrected - anchor));
        }
        Ok(out)
    })?;

    let film = FilmRgbImage::from_linear(
        LinearImage::new(image.width, image.height, rgb, image.ir.clone())
            .expect("the decode preserves the validated buffer-length invariants"),
    );
    Ok((
        film,
        DecodeReport {
            anchor,
            anchor_rule: params.anchor.name(),
            reads_reference: params.anchor.reads_reference(),
            linearization,
            scale,
            offset,
        },
    ))
}

/// What makes a [`DecodeParams`] unusable.
///
/// **One checker, rendered two ways.** [`DecodeParams::check`] is the only place the
/// rules live; the decode stage renders a fault as an internal error, and the recipe
/// gate (`crate::recipe::validate`) as a usage error naming the flag and the recipe
/// key. Two copies of these rules had already drifted apart on whether a
/// non-positive mid-grey offset is legal.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum DecodeFault {
    /// A per-channel offset is not finite. The offset is signed, so finite is the
    /// whole rule.
    Offset { channel: usize, value: f32 },
    /// A per-channel scale is not finite and positive. It is a *gain*: a zero renders
    /// that channel flat (finite, in range, counted by nothing) and a negative one
    /// reverses its density ordering, breaking the strictly-increasing contract.
    Scale { channel: usize, value: f32 },
    /// The linearization is not finite and positive.
    Linearization(f32),
    /// The anchor rule's mid-grey density above the base is not finite and positive —
    /// the range `mid-at-base-offset` has always had on the current chain.
    MidAboveBase(f32),
    /// The resolved anchor, or the curve's exponent at it, overflows f32.
    Anchor { anchor: f32, linearization: f32 },
}

impl DecodeParams {
    /// Check the parameters and return the resolved anchor.
    ///
    /// Validates the **resolved** anchor, never a stand-in for it: the rule divides by
    /// the contrast, so a finite-looking configuration can derive a non-finite anchor,
    /// and both ways the exponent goes non-finite render `10^(−inf) = 0.0` for every
    /// sample — an all-black frame that trips neither the clip counter nor the
    /// non-finite one.
    pub fn check(&self) -> std::result::Result<f32, DecodeFault> {
        for (channel, &value) in self.offset.iter().enumerate() {
            if !value.is_finite() {
                return Err(DecodeFault::Offset { channel, value });
            }
        }
        for (channel, &value) in self.scale.iter().enumerate() {
            if !value.is_finite() || value <= 0.0 {
                return Err(DecodeFault::Scale { channel, value });
            }
        }
        if !self.linearization.is_finite() || self.linearization <= 0.0 {
            return Err(DecodeFault::Linearization(self.linearization));
        }
        let AnchorRule::MidAboveBase(d) = self.anchor;
        if !d.is_finite() || d <= 0.0 {
            return Err(DecodeFault::MidAboveBase(d));
        }
        let anchor = self.anchor.anchor(self.linearization);
        if !anchor.is_finite() || !(self.linearization * anchor).is_finite() {
            return Err(DecodeFault::Anchor {
                anchor,
                linearization: self.linearization,
            });
        }
        Ok(anchor)
    }
}

/// [`DecodeParams::check`] for the stage: a programmatic caller reaches here, and a
/// CLI one has already been refused at the recipe gate naming the flag.
fn check_params(params: &DecodeParams) -> Result<f32> {
    params.check().map_err(|fault| {
        NcError::Other(match fault {
            DecodeFault::Offset { channel, value } => {
                format!("the fixed decode's offset[{channel}] must be finite (got {value})")
            }
            DecodeFault::Scale { channel, value } => {
                format!("the fixed decode's scale[{channel}] must be finite and > 0 (got {value})")
            }
            DecodeFault::Linearization(v) => {
                format!("the fixed decode's linearization must be finite and > 0 (got {v})")
            }
            DecodeFault::MidAboveBase(d) => format!(
                "the fixed decode's mid-above-base density must be finite and > 0 (got {d})"
            ),
            DecodeFault::Anchor {
                anchor,
                linearization,
            } => format!(
                "the fixed decode derived a non-usable anchor ({anchor:e}) at linearization \
                 {linearization}: the curve's exponent `linearization · (density − anchor)` is not \
                 finite, so every sample would render as exactly 0.0"
            ),
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::algo::reconstruct;
    use crate::types::{
        AnchorPlacement, DensityParams, ExponentialParams, REFERENCE_MID_TO_WHITE_DELTA,
        Reconstruction,
    };

    /// Bit patterns, not values: `NaN != NaN`, so an `==` comparison would pass over
    /// exactly the samples these assertions exist to cover.
    fn bits(pixels: &[f32]) -> Vec<u32> {
        pixels.iter().map(|v| v.to_bits()).collect()
    }

    fn base() -> FilmBase {
        FilmBase::from([0.9, 0.55, 0.42])
    }

    /// Ordinary densities, the film base itself (which decodes through `D = 0`, the
    /// `−0.0` the offset add normalizes in the staged path), a dead pixel, a negative
    /// sample, a denormal, and the non-finite trio.
    ///
    /// **`0.5 / 0.9` is the vector's only witness that the density rounds to f32
    /// before the `log10`** — computing the whole measurement in f64 moves that one
    /// sample and no other. Removing or rounding it silently retires a bit-identity
    /// rule the module documents.
    fn scan() -> LinearImage {
        LinearImage::new(
            4,
            1,
            vec![
                0.5,
                0.3,
                0.2,
                0.9,
                0.55,
                0.42,
                0.0,
                -0.25,
                f32::MIN_POSITIVE / 4.0,
                f32::NAN,
                f32::INFINITY,
                f32::NEG_INFINITY,
            ],
            Some(vec![0.1, 0.2, 0.3, 0.4]),
        )
        .unwrap()
    }

    /// A non-zero offset for the equality test's second pass. `DENSITY_OFFSET` is
    /// `[0, 0, 0]` today, which makes `+ offset` a no-op on every sample — so the
    /// shipped-value pass cannot witness the rule that it is never dropped, and
    /// `nf-calibration/offset-question` may move that constant out from under it.
    /// Distinct per channel, so a transposed channel reds it too.
    const PROBE_OFFSET: [f32; 3] = [-0.05, 0.02, 0.07];

    /// The current chain's configuration this decode reproduces: the exponential curve
    /// at the same slope, the same calibration, and the same reference-free placement.
    /// At the slope [`BUNDLED_CONTRAST`] it is that chain's default since
    /// `pipeline_version` 6; at [`LINEARIZATION`] it is this decode's default.
    fn equivalent_legacy(offset: [f32; 3], gamma: f32) -> Reconstruction {
        Reconstruction {
            density: DensityParams {
                scale: DENSITY_SCALE,
                offset,
            },
            curve: ExponentialParams {
                gamma,
                anchor: AnchorPlacement::MidAtBaseOffset(MID_ABOVE_BASE),
            },
        }
    }

    #[test]
    fn the_fixed_decode_matches_the_equivalent_legacy_configuration() {
        // The task's acceptance test, and it is a real one rather than a tautology:
        // the arithmetic here is written fresh and fused, so equality is a claim
        // about two implementations rather than about one call site. It is also
        // cross-target safe by construction — two implementations compared on one
        // host, never against a checked-in constant — which is what lets it be
        // bit-exact over `log10` and `powf` at all (CLAUDE.md, determinism).
        //
        // Run at **both** the shipped offset and a non-zero one, because neither alone
        // holds the rules the module docs claim for it: at `[0, 0, 0]` dropping the
        // offset add moves no sample, and at a non-zero offset the density's f32
        // rounding moves none. See the module docs for the measured split. The FMA
        // rule is witnessed by neither and has its own test below.
        //
        // Run at both slopes as well: this decode's default linearization, and the
        // bundled contrast the current chain still ships — the configuration that makes
        // that chain's default *this* decode.
        let (img, b) = (scan(), base());
        for gamma in [LINEARIZATION, BUNDLED_CONTRAST] {
            for offset in [DENSITY_OFFSET, PROBE_OFFSET] {
                let params = DecodeParams {
                    offset,
                    linearization: gamma,
                    ..DecodeParams::default()
                };
                let (fresh, _) = decode(&img, &b, &params).unwrap();
                let (legacy, _) = reconstruct(&img, &b, &equivalent_legacy(offset, gamma)).unwrap();
                assert_eq!(
                    bits(fresh.rgb()),
                    bits(legacy.rgb()),
                    "the fresh decode is not bit-identical to the equivalent legacy \
                     configuration at offset {offset:?}, slope {gamma}"
                );
            }
        }
    }

    #[test]
    fn the_decode_never_contracts_the_calibration_into_an_fma() {
        // The bit-identity rule the equality vector above cannot see: over `scan()`,
        // `scale · d + offset` and `scale.mul_add(d, offset)` agree bit-for-bit at
        // every offset, so an FMA would slip past it. They part just under the film
        // base, where the rounded product and the offset are the same size — so this
        // sweeps a band of samples there, pins every one of them against the plain
        // product-then-add, and asserts the band distinguishes the two forms at all.
        //
        // Swept rather than pinned to one sample: the witness is reached through
        // `log10f`, which may differ by a ULP across targets (CLAUDE.md, determinism),
        // so a hard-coded sample could witness here and not in CI. Both halves stay
        // sound either way — the equality is computed on the host that renders it, and
        // the `assert!` below fails loudly rather than passing vacuously.
        const N: usize = 100_000;
        const BASE: f32 = 0.9;
        let b = FilmBase::from([BASE; 3]);
        let params = DecodeParams {
            offset: PROBE_OFFSET,
            ..DecodeParams::default()
        };

        let mut samples = Vec::with_capacity(N * 3);
        let mut s = BASE;
        for _ in 0..N {
            samples.extend_from_slice(&[s; 3]);
            s = f32::from_bits(s.to_bits() - 1);
        }
        let img = LinearImage::new(N as u32, 1, samples.clone(), None).unwrap();
        let (film, report) = decode(&img, &b, &params).unwrap();

        let mut witnesses = 0usize;
        for (i, out) in film.rgb().iter().enumerate() {
            let c = i % 3;
            let d = -(samples[i] / BASE).log10();
            let curve =
                |corrected: f32| 10f32.powf(report.linearization * (corrected - report.anchor));
            let plain = curve(report.scale[c] * d + report.offset[c]);
            let fused = curve(report.scale[c].mul_add(d, report.offset[c]));
            assert_eq!(
                out.to_bits(),
                plain.to_bits(),
                "sample {i} did not take the plain product-then-add"
            );
            witnesses += usize::from(plain.to_bits() != fused.to_bits());
        }
        assert!(
            witnesses > 0,
            "no sample in the swept band tells an FMA from the plain form, so this \
             test proves nothing"
        );
    }

    #[test]
    fn a_one_ulp_move_in_the_anchor_is_visible_to_that_comparison() {
        // Falsifiability for `the_fixed_decode_matches_the_equivalent_legacy_configuration`:
        // it is evidence only if the comparison can fail on the values this decode
        // actually carries. The smallest move of `d` that moves the resolved anchor
        // must red it — and that is at most two ULP of `d`: at the linearization the
        // anchor (≈1.03) sits a binade above `d` (0.62), so its ULP is twice as coarse
        // and a single-ULP move of `d` can round away in the sum.
        let (img, b) = (scan(), base());
        let shipped_params = DecodeParams::default();
        let shipped_anchor = shipped_params.anchor.anchor(shipped_params.linearization);
        let mut d = MID_ABOVE_BASE;
        let mut steps = 0;
        let moved_params = loop {
            d = d.next_up();
            steps += 1;
            let p = DecodeParams {
                anchor: AnchorRule::MidAboveBase(d),
                ..DecodeParams::default()
            };
            if p.anchor.anchor(p.linearization) != shipped_anchor {
                break p;
            }
        };
        assert!(steps <= 2, "{steps} ULP of `d` before the anchor moved");
        let shipped = decode(&img, &b, &shipped_params).unwrap().0;
        let moved = decode(&img, &b, &moved_params).unwrap().0;
        assert_ne!(bits(shipped.rgb()), bits(moved.rgb()));
    }

    #[test]
    fn the_comparison_distinguishes_a_different_placement() {
        // The other half of falsifiability: the equality must be pinning *this*
        // configuration, not passing because every configuration agrees on this
        // vector. A different placement on the same curve must differ.
        let (img, b) = (scan(), base());
        let (fresh, _) = decode(&img, &b, &DecodeParams::default()).unwrap();
        let other = Reconstruction {
            density: DensityParams {
                scale: DENSITY_SCALE,
                offset: DENSITY_OFFSET,
            },
            curve: ExponentialParams {
                gamma: LINEARIZATION,
                anchor: AnchorPlacement::MidAtBaseOffset(0.5),
            },
        };
        let (legacy, _) = reconstruct(&img, &b, &other).unwrap();
        assert_ne!(bits(fresh.rgb()), bits(legacy.rgb()));
    }

    #[test]
    fn the_current_chains_default_is_this_decode_at_the_bundled_contrast() {
        // Since `pipeline_version` 6 the current chain's default reconstruction reads
        // its curve constants from here, so the equality tests above describe *the*
        // default rather than one configuration of it. That chain has no look stage, so
        // it still carries the whole bundled `gamma` rather than the linearization —
        // which is what keeps the split from moving a pixel there. This pins the part
        // those constants do not reach: the offset default and the whole shape.
        assert_eq!(
            Reconstruction::default(),
            equivalent_legacy(DENSITY_OFFSET, BUNDLED_CONTRAST)
        );
        assert_eq!(SCAN_FLOOR, density::SCAN_EPSILON);
    }

    #[test]
    fn the_scale_convention_pins_red_to_one() {
        // Only the products `linearization · scale_c` reach the curve, so the two
        // factors are one measurement split by a convention. Red carries the
        // linearization alone; move it off 1 and the same products are spelled a
        // second way, which a later retune of either factor would then correct twice.
        assert_eq!(DENSITY_SCALE[0], 1.0);
        assert_eq!(DecodeParams::default().scale[0], 1.0);
    }

    #[test]
    fn the_anchor_is_the_documented_number() {
        // 0.62 + 0.745/1.8 at the linearization. Before the split the decode ran at the
        // bundled 2.0, where `anchor-spike` read `anchor_value: 0.99236375` off the
        // binary — the number `docs/spike/white-placement.md` reasons from. Both are
        // pinned, so a change to any of the three constants lands here first.
        let report = decode(&scan(), &base(), &DecodeParams::default())
            .unwrap()
            .1;
        assert_eq!(report.anchor, 1.033_737_5);
        assert_eq!(report.anchor_rule, "mid-at-base-offset");
        let bundled = AnchorRule::MidAboveBase(MID_ABOVE_BASE).anchor(BUNDLED_CONTRAST);
        assert_eq!(bundled, 0.992_363_75);

        // And where that sits against the *other* white in the glossary: the
        // datasheets' diffuse white, `d + 0.36`. At the bundled contrast — what the
        // chain renders once the look's default contrast is applied — the gap is ~0.08
        // stops, the measured fact behind "nc anchors at diffuse white, spelled as a mid
        // anchor" — not an equality, and the two must not be conflated. At the
        // linearization alone the decode's own anchor is further off, which is why
        // `DIFFUSE_WHITE` is a property of the graded image, not of this output.
        let diffuse_white = MID_ABOVE_BASE + REFERENCE_MID_TO_WHITE_DELTA;
        assert!((bundled - diffuse_white).abs() < 0.02);
        assert!((report.anchor - diffuse_white).abs() > 0.05);
    }

    #[test]
    fn changing_the_anchor_is_a_pure_gain() {
        // On a straight line the anchor factors out as `10^(−contrast·A)`, identical
        // on every channel — which is why `d` is a calibration and brightness belongs
        // to rendering. (It holds only without a shoulder, which is part of why the
        // sigmoid's knees retired.)
        let (img, b) = (scan(), base());
        let a = decode(&img, &b, &DecodeParams::default()).unwrap().0;
        let shifted = DecodeParams {
            anchor: AnchorRule::MidAboveBase(MID_ABOVE_BASE + 0.1),
            ..DecodeParams::default()
        };
        let c = decode(&img, &b, &shifted).unwrap().0;
        let gain = 10f32.powf(-LINEARIZATION * 0.1);
        for (x, y) in a.rgb().iter().zip(c.rgb()) {
            if x.is_finite() && *x > 0.0 {
                assert!(
                    (y / x - gain).abs() < 1e-4,
                    "not a pure gain: {x} -> {y}, expected ratio {gain}"
                );
            }
        }
    }

    #[test]
    fn the_ir_plane_is_carried_and_never_minted() {
        // Preserve, don't consume — and the falsifiable half: an IR-free scan must
        // stay IR-free.
        let (film, _) = decode(&scan(), &base(), &DecodeParams::default()).unwrap();
        assert_eq!(film.ir(), Some(&[0.1_f32, 0.2, 0.3, 0.4][..]));
        assert_eq!((film.width(), film.height()), (4, 1));

        let bare = LinearImage::new(1, 1, vec![0.5, 0.3, 0.2], None).unwrap();
        let (film, _) = decode(&bare, &base(), &DecodeParams::default()).unwrap();
        assert_eq!(film.ir(), None);
    }

    #[test]
    fn a_degenerate_base_fails_loudly() {
        for bad in [[0.0, 0.5, 0.5], [-0.1, 0.5, 0.5], [f32::NAN, 0.5, 0.5]] {
            let err = decode(&scan(), &FilmBase::from(bad), &DecodeParams::default()).unwrap_err();
            assert!(err.message().contains("film base"), "{}", err.message());
        }
    }

    #[test]
    fn a_non_usable_anchor_is_refused_rather_than_rendered_black() {
        // The failure this guard exists for renders every sample as exactly 0.0 and
        // trips neither the clip counter nor the non-finite one, so it has to be
        // refused before the pass rather than counted after it. There are **two**
        // routes to it and one check does not cover both: a positive-but-tiny
        // contrast overflows the *anchor* through the `0.745/contrast` division
        // (needs a subnormal — `f32::MIN_POSITIVE` divides to a finite 6.3e37), and a
        // large-but-finite anchor overflows the *product* `contrast · anchor` at an
        // ordinary contrast (so it has to clear f32::MAX/contrast, not merely look
        // absurd — 1e38 at contrast 2.0 is still finite).
        for (params, what) in [
            (
                DecodeParams {
                    linearization: f32::from_bits(1),
                    ..DecodeParams::default()
                },
                "the anchor",
            ),
            (
                DecodeParams {
                    anchor: AnchorRule::MidAboveBase(2e38),
                    ..DecodeParams::default()
                },
                "the product",
            ),
        ] {
            let err = decode(&scan(), &base(), &params).unwrap_err();
            assert!(
                err.message().contains("non-usable anchor"),
                "{what} overflow was not refused: {}",
                err.message()
            );
        }

        // And the ordinary parameter guards, each naming what it rejected.
        for (params, needle) in [
            (
                DecodeParams {
                    linearization: 0.0,
                    ..DecodeParams::default()
                },
                "linearization",
            ),
            (
                DecodeParams {
                    scale: [1.0, f32::NAN, 1.0],
                    ..DecodeParams::default()
                },
                "scale[1]",
            ),
            (
                DecodeParams {
                    offset: [0.0, 0.0, f32::INFINITY],
                    ..DecodeParams::default()
                },
                "offset[2]",
            ),
        ] {
            let err = decode(&scan(), &base(), &params).unwrap_err();
            assert!(err.message().contains(needle), "{}", err.message());
        }

        // The scale is a gain, so finite is not enough — and both failures are quiet:
        // a zero renders that channel flat (finite, in range, tripping no counter) and
        // a negative one reverses its density ordering. `cli::validate` refuses both at
        // the flag; a programmatic caller reaches this guard.
        for scale in [[1.0, 0.0, 1.0], [1.0, -0.84, 1.0]] {
            let err = decode(
                &scan(),
                &base(),
                &DecodeParams {
                    scale,
                    ..DecodeParams::default()
                },
            )
            .unwrap_err();
            assert!(
                err.message().contains("scale[1] must be finite and > 0"),
                "{}",
                err.message()
            );
        }

        // The mid-grey offset is a density *above* the base, so it is positive — the
        // range `mid-at-base-offset` has on the current chain too.
        for d in [0.0, -0.1, f32::NAN] {
            let err = decode(
                &scan(),
                &base(),
                &DecodeParams {
                    anchor: AnchorRule::MidAboveBase(d),
                    ..DecodeParams::default()
                },
            )
            .unwrap_err();
            assert!(
                err.message().contains("mid-above-base"),
                "{}",
                err.message()
            );
        }

        // Falsifiability for the pair above: the *offset* is signed, so a negative one
        // is ordinary and must still decode.
        decode(
            &scan(),
            &base(),
            &DecodeParams {
                offset: [-0.05; 3],
                ..DecodeParams::default()
            },
        )
        .unwrap();
    }

    #[test]
    fn the_scan_floor_bounds_dead_pixels_and_lets_corrupt_input_through() {
        // Two different failures, deliberately handled differently: a physically real
        // dead pixel is floored to a high-but-finite density, while a non-finite
        // sample propagates as `NaN` so `io::encode`'s counter still surfaces it.
        // Laundering the second would hide corrupt input behind a plausible pixel.
        let img = LinearImage::new(2, 1, vec![0.0, -1.0, 0.5, f32::NAN, 0.5, 0.5], None).unwrap();
        let (film, _) = decode(&img, &base(), &DecodeParams::default()).unwrap();
        let out = film.rgb();
        assert!(out[0].is_finite() && out[0] >= 0.0, "{}", out[0]);
        assert!(out[1].is_finite() && out[1] >= 0.0, "{}", out[1]);
        assert!(out[3].is_nan(), "{}", out[3]);
        assert!(out[4].is_finite());
    }
}
