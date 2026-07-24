//! Negative reconstruction and density curves (design-spec §7).
//!
//! The tagged [`Reconstruction`] config drives one of two paths — `simple`
//! (direct inversion) or `density` (Dmin-normalized corrected density `D′`
//! mapped through a tagged exponential or sigmoid curve) — and **every path
//! returns the typed [`FilmRgbImage`] boundary**:
//!
//! ```text
//! scan → Dmin normalization → corrected density D′   (density reconstruction)
//!      → exponential | sigmoid density curve          (the curve stage)
//!      → FilmRgbImage                                  (typed boundary)
//! ```
//!
//! [`FilmRgbImage`]'s fields are private and its only constructor is
//! `pub(in crate::algo)`, so [`reconstruct`]'s paths inside this module tree
//! are the only producers — downstream stages that accept a `FilmRgbImage`
//! (the future NC-film-RGB → ACEScg working-space mapper) can never be handed
//! a raw scan or density buffer. [`finish_print`] is the **legacy no-preset
//! bridge**: while the print controls still run before the output color
//! transform (named presets later move them after the ACEScg boundary), it
//! applies stage 4 to the film positive and returns the plain [`LinearImage`]
//! the output transform consumes. The pixel arithmetic of
//! `reconstruct → finish_print` is bit-identical to the pre-split monolithic
//! converters (pinned by the golden fixtures in `pipeline::stages`, `mod
//! golden`).

pub mod density;
pub mod sigmoid;
pub mod simple;

use crate::types::{FilmBase, LinearImage, PrintParams, Reconstruction, Result, WbSource};

/// The typed film-rendering RGB boundary every reconstruction path produces:
/// the unclamped linear positive in NC's film-rendering interpretation, plus
/// the carried-through IR plane. Fields are **private** and the constructor is
/// `pub(in crate::algo)`, so only the `algo` module tree's reconstruction
/// paths can mint one — a raw scan or density buffer cannot impersonate film
/// RGB downstream (the working-space mapper accepts `FilmRgbImage`, nothing
/// else).
///
/// Values are deliberately unclamped (HDR/scene-headroom preserved; range
/// clamping happens only at the u16 encode step) and may be non-finite when
/// the input was (fail-loud propagation to `io::encode`'s counters).
///
/// `Debug` prints only the dimensions (never the pixel buffers) — it exists so
/// `Result<FilmRgbImage, _>` works with `unwrap_err`/`expect` in tests.
pub struct FilmRgbImage {
    width: u32,
    height: u32,
    /// Interleaved `r,g,b` positive, `len == width * height * 3`.
    rgb: Vec<f32>,
    /// Carried-through IR plane (HDRi input), `len == width * height`.
    ir: Option<Vec<f32>>,
}

impl FilmRgbImage {
    /// Sole constructor — restricted to the `algo` module tree (note:
    /// `pub(super)` would NOT do this: `algo` is a top-level module, so its
    /// `super` is the crate root and `pub(super)` would be crate-wide), so
    /// [`reconstruct`]'s paths are the only producers. Takes an
    /// already-validated [`LinearImage`] so the buffer length invariants hold
    /// by construction.
    pub(in crate::algo) fn from_linear(image: LinearImage) -> Self {
        Self {
            width: image.width,
            height: image.height,
            rgb: image.rgb,
            ir: image.ir,
        }
    }

    // The read accessors below are the boundary's inspection API. `rgb` is
    // consumed by the legacy print finishing; `width`/`height`/`ir` are only
    // exercised by tests until the `film-rgb-working-space` mapper (the type's
    // designed consumer) lands — a narrow documented allow per the house rule.
    #[allow(dead_code)]
    pub fn width(&self) -> u32 {
        self.width
    }

    #[allow(dead_code)]
    pub fn height(&self) -> u32 {
        self.height
    }

    /// Read-only view of the interleaved film positive.
    pub fn rgb(&self) -> &[f32] {
        &self.rgb
    }

    /// Read-only view of the carried IR plane, when the input had one.
    #[allow(dead_code)]
    pub fn ir(&self) -> Option<&[f32]> {
        self.ir.as_deref()
    }

    /// Unwrap into the plain working-space image type — the **read** direction
    /// of the boundary, for the legacy no-preset path (and, later, the
    /// working-space mapper). Constructing a `FilmRgbImage` stays restricted;
    /// reading one out is not the invariant the type protects.
    pub(crate) fn into_linear(self) -> LinearImage {
        // The fields came from a validated LinearImage and are never resized,
        // so the invariants hold; route through the validated constructor
        // anyway (its checks are O(1)) so a future regression fails loudly.
        LinearImage::new(self.width, self.height, self.rgb, self.ir)
            .expect("FilmRgbImage preserves the validated buffer-length invariants")
    }
}

impl std::fmt::Debug for FilmRgbImage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FilmRgbImage")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("ir", &self.ir.is_some())
            .finish_non_exhaustive()
    }
}

/// Diagnostics the reconstruction stage surfaces for the JSON report — the
/// resolved values, not new knobs (controls live in [`Reconstruction`]).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ReconstructionReport {
    /// The resolved display-white anchor density (`Dmax`) the curve used.
    /// `None` for `simple` (no curve stage) and for the exponential curve with
    /// `dmax = none` (unity placement).
    pub dmax: Option<f32>,
    /// The resolved regional-balance tone-ramp range `[lo, hi]` (corrected
    /// density), when a shadow/highlight balance was applied. `None` for
    /// `simple` or when both balances are the neutral `[0, 0, 0]`.
    pub balance_range: Option<[f32; 2]>,
}

/// Stage 3 — reconstruct the negative into the typed film positive
/// (design-spec §7): pure `(input, config) -> output`, dispatching on the
/// tagged [`Reconstruction`]. Every supported path returns [`FilmRgbImage`];
/// the IR plane is carried through untouched (Step-1 rule: preserve, don't
/// consume). Total in its inputs: a degenerate film base or an unusable curve
/// anchor surfaces as an [`NcError`](crate::types::NcError), never a
/// silently-wrong image.
pub fn reconstruct(
    image: &LinearImage,
    base: &FilmBase,
    config: &Reconstruction,
) -> Result<(FilmRgbImage, ReconstructionReport)> {
    match config {
        Reconstruction::Simple => Ok((
            simple::reconstruct(image, base)?,
            ReconstructionReport::default(),
        )),
        Reconstruction::Density { density, curve } => {
            density::reconstruct(image, base, density, curve)
        }
    }
}

/// Stage 4, legacy placement — resolve the print white-balance gains and run
/// the print render on the reconstructed film positive; `simple` has no print
/// stage, so its typed positive passes through unchanged. Returns the finished
/// linear image plus the resolved gains (`None` when no print stage ran) for
/// the JSON report.
///
/// This is the **no-preset bridge**: the print controls still run here, before
/// the output color transform, exactly as the pre-split converters ordered
/// them — named output presets later move these controls after the ACEScg
/// working-space boundary (`film-master-render-pipeline`), behind a
/// `pipeline_version` bump owned by `conversion-versioning`.
///
/// An auto WB mode ([`WbSource::GrayWorld`]/[`Percentile`](WbSource::Percentile))
/// is estimated from a deterministic strided sample of the film positive — the
/// same values the pre-split code produced by toning a strided sample of the
/// density buffer (a per-sample map commutes with striding) — and applied
/// through the standard stage-4 slot, so reusing the reported gains via
/// `--white-balance` reproduces the output bit-for-bit.
pub fn finish_print(
    film: FilmRgbImage,
    config: &Reconstruction,
    print: &PrintParams,
) -> Result<(LinearImage, Option<[f32; 3]>)> {
    match config {
        // `simple` consumes no print controls (`cli::validate` rejects the auto
        // WB modes for it, and the explicit controls are inert as before).
        Reconstruction::Simple => Ok((film.into_linear(), None)),
        Reconstruction::Density { .. } => {
            let wb = match print.white_balance {
                WbSource::Explicit(gains) => gains,
                auto_mode => {
                    let sampled = density::sample_positive(film.rgb());
                    density::estimate_wb_gains(&sampled, auto_mode)?
                }
            };
            Ok((density::render_print(film, wb, print), Some(wb)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{DensityCurve, DensityParams, ExponentialParams, SigmoidParams};

    fn image() -> LinearImage {
        LinearImage::new(
            2,
            1,
            vec![0.5, 0.3, 0.2, 0.05, 0.03, 0.02],
            Some(vec![0.25, 0.75]),
        )
        .unwrap()
    }

    fn base() -> FilmBase {
        FilmBase::from([0.9, 0.55, 0.42])
    }

    /// Every supported reconstruction config for exhaustive path checks.
    fn all_configs() -> [Reconstruction; 3] {
        [
            Reconstruction::Simple,
            Reconstruction::Density {
                density: DensityParams::default(),
                curve: DensityCurve::Exponential(ExponentialParams::default()),
            },
            Reconstruction::Density {
                density: DensityParams::default(),
                curve: DensityCurve::Sigmoid(SigmoidParams::default()),
            },
        ]
    }

    #[test]
    fn every_path_returns_a_film_rgb_image_and_preserves_ir() {
        // The type-level boundary: each supported config produces a
        // `FilmRgbImage` (enforced by `reconstruct`'s signature — this test
        // exercises all paths) with the dimensions and IR plane intact.
        for config in all_configs() {
            let (film, _) = reconstruct(&image(), &base(), &config).unwrap();
            assert_eq!((film.width(), film.height()), (2, 1), "{config:?}");
            assert_eq!(film.rgb().len(), 6, "{config:?}");
            assert_eq!(film.ir(), Some(&[0.25_f32, 0.75][..]), "{config:?}");
            // The read direction round-trips losslessly.
            let linear = film.into_linear();
            assert_eq!((linear.width, linear.height), (2, 1));
            assert_eq!(linear.ir.as_deref(), Some(&[0.25_f32, 0.75][..]));
        }
    }

    // `FilmRgbImage`'s construction privacy is enforced by the compiler:
    // `from_linear` is `pub(in crate::algo)`, so no code outside the `algo`
    // module tree can mint one — the working-space mapper can only receive
    // what `reconstruct` produced. (A compile-fail test would need a
    // `trybuild` dev-dependency; the privacy annotation is the guarantee.)

    #[test]
    fn simple_reports_no_curve_diagnostics() {
        let (_, report) = reconstruct(&image(), &base(), &Reconstruction::Simple).unwrap();
        assert_eq!(report, ReconstructionReport::default());
    }

    #[test]
    fn density_paths_report_their_resolved_anchor() {
        for config in &all_configs()[1..] {
            let (_, report) = reconstruct(&image(), &base(), config).unwrap();
            // Both curves default to the fixed nominal anchor.
            assert_eq!(report.dmax, Some(density::NOMINAL_DMAX), "{config:?}");
            assert_eq!(report.balance_range, None, "{config:?}");
        }
    }

    #[test]
    fn finish_print_passes_simple_through_and_prints_density() {
        // Simple: no print stage — the positive passes through bit-identically
        // and no gains are reported, even with non-default print params.
        let (film, _) = reconstruct(&image(), &base(), &Reconstruction::Simple).unwrap();
        let expected = film.rgb().to_vec();
        let print = PrintParams {
            print_exposure: 1.0,
            ..PrintParams::default()
        };
        let (out, wb) = finish_print(film, &Reconstruction::Simple, &print).unwrap();
        assert_eq!(out.rgb, expected);
        assert_eq!(wb, None);

        // Density: the print stage runs (2^1 exposure doubles every sample)
        // and the resolved (explicit, neutral) gains are reported.
        let config = all_configs()[1].clone();
        let (film, _) = reconstruct(&image(), &base(), &config).unwrap();
        let expected: Vec<f32> = film.rgb().iter().map(|v| v * 2.0).collect();
        let (out, wb) = finish_print(film, &config, &print).unwrap();
        assert_eq!(out.rgb, expected);
        assert_eq!(wb, Some([1.0, 1.0, 1.0]));
    }
}
