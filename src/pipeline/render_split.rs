//! The named-output split out of the NC film RGB v1 ACEScg boundary
//! (design-spec §5/§6, stage 5):
//!
//! ```text
//! linear ACEScg film rendering (AcesCgImage)
//!   ├→ film-master            — encoded directly, no controls at all
//!   └→ shared print controls  — WB → exposure → black point → range placement
//!        └→ SDR / HDR display branches (identical adjusted source)
//! ```
//!
//! **Every entry point here accepts [`AcesCgImage`] only**, and the *construction*
//! half of that is compiler-enforced: `AcesCgImage`'s fields are private and its
//! `fn new` is module-private to
//! [`working_space`](crate::pipeline::working_space), so
//! [`map_nc_film_rgb_v1`](crate::pipeline::working_space::map_nc_film_rgb_v1) is
//! the only way to mint one, and film RGB (`algo::FilmRgbImage`) or raw
//! device/scanner RGB (`types::LinearImage`) cannot be handed to `film_master` or
//! `display_source`.
//!
//! What this does **not** claim: that no unmapped buffer can ever be written with a
//! named profile. `io::encode(image: &LinearImage, params: &OutputParams, …)`
//! accepts exactly that pairing with no type-level relationship between the two —
//! keeping the ACEScg profile matched to ACEScg pixels is the orchestrator's job
//! (`stages::render_film_master` fetches the tag on the same branch that maps the
//! pixels). The type boundary constrains what can *enter the split*, not what the
//! encoder can be told to tag.
//!
//! Producer *provenance* is deliberately **not** a type invariant — a later
//! explicitly-selected correction profile
//! (`color/optional-color-correction-profiles`) may produce the same
//! `AcesCgImage` and feed this unchanged split. This module implements no
//! correction selection, stage, or provenance.
//!
//! ## What `film-master` is (and is not)
//! An **unclamped 32-bit float linear ACEScg** image containing the intentional
//! film, lens, development, scanner, reconstruction, and density-curve rendering,
//! including supported fixed/roll `Dmax` placement. It is **not** a physical
//! scene-linear recovery, and it is **not** what `--output-hdr` produces (that is
//! a transitional *rendered* float TIFF — the print controls already ran).
//! [`film_master`] therefore does exactly one thing: unwrap the mapped ACEScg
//! buffer. Nothing is applied, nothing is clamped (range clamping stays at the
//! u16 encode step, which `film-master` never uses), and non-finite samples ride
//! through to `io::encode`'s counter. The strict rejection of `auto` `Dmax` and of
//! every non-default downstream control happens at the CLI boundary
//! (`cli::validate`), after recipe/flag merge — never silently here.
//!
//! ## The shared display controls
//! [`display_source`] resolves the shared controls **once** and applies them in
//! the pinned order, returning one [`SharedDisplaySource`] that owns exactly one
//! [`AdjustedAcesCgImage`]. Both display branches then *borrow* that single buffer
//! (`&shared.source`), so "SDR and HDR receive identical adjusted source" is
//! structural rather than a convention two renderers must remember: there is no
//! per-branch buffer to diverge, and `AdjustedAcesCgImage`'s module-private
//! constructor means a renderer cannot mint its own. [`DisplayBranch`] is the seam
//! the consumers match on to pick their renderer — it deliberately has no say in
//! the shared stage. Branch-specific reference white, highlight/tone behaviour,
//! destination gamut mapping, and transfer encoding are **not** here: they are
//! owned by `output/sdr-display-rendering` and `output/hdr-display-rendering`,
//! which are this half's consumers.

use crate::algo::density;
use crate::pipeline::working_space::AcesCgImage;
use crate::types::{LinearImage, NcError, PrintParams, Result, WbSource};

/// The `film-master` branch: the mapped linear ACEScg buffer, unchanged.
///
/// No white balance, exposure, black/range placement, highlight compression,
/// display tone mapping, gamut mapping, or transfer encoding is applied — the
/// bypass is the definition of the master, so this is a pure unwrap and any
/// future operation added here would be a bug. Values stay **unclamped** and
/// possibly non-finite; the IR plane rides through untouched.
///
/// Total (no `Result`): there is nothing here that can fail.
pub fn film_master(aces: AcesCgImage) -> LinearImage {
    aces.into_linear()
}

/// The two display renderers the shared adjusted source feeds. This enum is only
/// the seam: the branch does **not** influence the shared stage (that is the
/// invariant), it selects which downstream renderer consumes the result.
#[allow(dead_code)] // constructed by `output/{sdr,hdr}-display-rendering`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DisplayBranch {
    /// SDR (Display P3 / sRGB) — `output/sdr-display-rendering`.
    Sdr,
    /// Display HDR (BT.2020 PQ / HLG) — `output/hdr-display-rendering`.
    Hdr,
}

/// The shared print controls after resolution: the values that actually get
/// applied, in the order [`apply_shared_controls`] applies them. An **auto**
/// [`WbSource`] is resolved to concrete gains here, so the two display branches
/// cannot re-estimate and drift apart — and so a run can freeze the reported
/// gains into an explicit recipe and reproduce the buffer bit-for-bit.
///
/// Fields are **private** and [`new`](Self::new) is the only constructor, so every
/// value that reaches [`apply_shared_controls`]'s infallible arithmetic has been
/// checked here — in particular the range span it divides by. The alternative (all-
/// public fields plus a comment claiming the CLI validated them) was not true even
/// of this module's own tests, and `resolve_shared_controls` already returns
/// `Result`, so the guard costs nothing.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ResolvedPrintControls {
    /// Per-channel white-balance gains, applied first.
    white_balance: [f32; 3],
    /// The linear gain `2^print_exposure` (exposure is configured in stops).
    exposure_gain: f32,
    /// The black floor subtracted after exposure.
    black_point: f32,
    /// The `[low, high]` endpoints of the final affine range placement.
    linear_range: [f32; 2],
}

impl ResolvedPrintControls {
    /// Sole constructor: check every value the arithmetic depends on, or fail.
    ///
    /// - the WB gains are finite and **positive** (a zero or negative gain is not a
    ///   white balance, it is a channel kill / sign flip);
    /// - `exposure_gain` is **normal**. `2^print_exposure` overflows to `inf` above
    ///   ~127 stops and decays through the f32 subnormals (reaching `2^-149`) below
    ///   ~−126, while `cli::validate` only checks that the *stop* value is finite. The
    ///   two ends fail differently: the overflow end yields `inf`/`NaN` samples, which
    ///   `io::encode`'s non-finite counter reports **loudly**, whereas the subnormal end
    ///   crushes every product toward zero — `2^-140 · 0.5` is `3.6e-43`, a value that
    ///   is not literally `0.0` but quantizes to black and carries no recoverable tone,
    ///   and *that* trips neither the clip counter nor the non-finite counter. Only the
    ///   low end is silent, and it is the same silent-destruction class the sigmoid
    ///   contrast/knee caps close. `is_normal()` rejects zero, subnormal, infinite, and
    ///   NaN in one predicate and matches the bound the message quotes;
    /// - each **`wb[c] · exposure_gain` product** is normal too. Neither factor alone is
    ///   enough: `--white-balance 1e-30,1e-30,1e-30 --print-exposure -100` passes both
    ///   checks above (the gains are positive-finite, `2^-100` is normal) yet the
    ///   product is `7.9e-61`, which in f32 *is* exactly `0.0` — so every sample is
    ///   zeroed with no counter firing. Checking the product is what makes the
    ///   silent-destruction claim above actually true;
    /// - `black_point` is finite;
    /// - the range endpoints are finite with `low < high` and a finite, positive
    ///   span — [`apply_shared_controls`] divides by that span.
    ///
    /// What this deliberately does **not** prevent, because both are loud rather than
    /// silent: a *subnormal but positive* span (`linear_range: [0.0, 1e-40]`, which
    /// `cli::validate` also accepts) divides to `inf`, and a validated-normal gain
    /// multiplied by a large enough pixel can still overflow to `inf`. Both reach
    /// `io::encode`'s non-finite counter, which is the designed channel for them; a
    /// bound tight enough to exclude them would have to know the pixel values.
    ///
    /// [`NcError::Usage`] (exit 2) rather than an internal error: every input traces
    /// back to a user-supplied `print.*` value, so the actionable report is "this
    /// number is out of range", not "nc has a bug".
    fn new(
        white_balance: [f32; 3],
        exposure_gain: f32,
        black_point: f32,
        linear_range: [f32; 2],
    ) -> Result<Self> {
        let usage = |m: String| NcError::Usage(m);
        if !white_balance.iter().all(|g| g.is_finite() && *g > 0.0) {
            return Err(usage(format!(
                "print.white_balance resolved to {white_balance:?}; the shared display \
                 stage needs finite positive per-channel gains"
            )));
        }
        // `is_normal()`, not `is_finite() && != 0.0`: a subnormal gain is finite and
        // non-zero yet crushes every product to a quantizes-to-black value with no
        // counter firing.
        if !exposure_gain.is_normal() {
            return Err(usage(format!(
                "print.print_exposure resolved to a linear gain of {exposure_gain:e} \
                 (2^print_exposure), which is not a normal f32; use a stop value in \
                 roughly −126..127. Above it the gain overflows to infinity (loud — the \
                 non-finite counter reports it); below it the gain decays into the \
                 subnormals and crushes every sample to a value that quantizes to black \
                 while tripping neither the clip nor the non-finite counter"
            )));
        }
        // Neither factor alone is sufficient: individually-valid gains can multiply to a
        // subnormal (or to exactly 0.0 in f32), which zeroes every sample silently. This
        // is the check that makes the guard above match its own claim.
        if let Some((c, product)) = (0..3)
            .map(|c| (c, white_balance[c] * exposure_gain))
            .find(|(_, p)| !p.is_normal())
        {
            return Err(usage(format!(
                "print.white_balance[{c}] ({}) times the linear exposure gain \
                 {exposure_gain:e} (2^print_exposure) is {product:e}, which is not a \
                 normal f32 — each factor is individually valid but their product would \
                 crush every sample of that channel to a quantizes-to-black value (or \
                 overflow it) without tripping the clip or non-finite counters. Reduce \
                 the combined attenuation / boost.",
                white_balance[c]
            )));
        }
        if !black_point.is_finite() {
            return Err(usage(format!(
                "print.black_point must be finite (got {black_point})"
            )));
        }
        let [low, high] = linear_range;
        let span = high - low;
        if !low.is_finite() || !high.is_finite() || low >= high || !span.is_finite() || span <= 0.0
        {
            return Err(usage(format!(
                "print.linear_range {linear_range:?} must be finite with low < high and a \
                 representable positive span (the shared display stage divides by it)"
            )));
        }
        Ok(Self {
            white_balance,
            exposure_gain,
            black_point,
            linear_range,
        })
    }

    /// Per-channel white-balance gains, applied first.
    #[allow(dead_code)] // read by `output/{sdr,hdr}-display-rendering` (and the report).
    pub fn white_balance(&self) -> [f32; 3] {
        self.white_balance
    }

    /// The linear gain `2^print_exposure`.
    #[allow(dead_code)] // read by `output/{sdr,hdr}-display-rendering`.
    pub fn exposure_gain(&self) -> f32 {
        self.exposure_gain
    }

    /// The black floor subtracted after exposure.
    #[allow(dead_code)] // read by `output/{sdr,hdr}-display-rendering`.
    pub fn black_point(&self) -> f32 {
        self.black_point
    }

    /// The `[low, high]` endpoints of the final affine range placement.
    #[allow(dead_code)] // read by `output/{sdr,hdr}-display-rendering`.
    pub fn linear_range(&self) -> [f32; 2] {
        self.linear_range
    }
}

/// The one adjusted linear-ACEScg source both display branches consume.
///
/// Fields are **private** and [`new`](Self::new) is module-private, so
/// [`apply_shared_controls`] is the only producer: a display renderer that
/// accepts a `SharedDisplaySource` cannot be handed a buffer that skipped the
/// shared stage, and cannot be handed the `film-master` buffer either (which is a
/// plain [`LinearImage`], never this type).
#[allow(dead_code)] // read by `output/{sdr,hdr}-display-rendering`.
pub struct AdjustedAcesCgImage {
    width: u32,
    height: u32,
    rgb: Vec<f32>,
    ir: Option<Vec<f32>>,
}

impl AdjustedAcesCgImage {
    /// Sole constructor — module-private, so only [`apply_shared_controls`] can
    /// build one. Takes the already-validated buffers out of a [`LinearImage`],
    /// so the length invariants hold by construction.
    fn new(image: LinearImage) -> Self {
        Self {
            width: image.width,
            height: image.height,
            rgb: image.rgb,
            ir: image.ir,
        }
    }

    #[allow(dead_code)] // read by `output/{sdr,hdr}-display-rendering`.
    pub fn width(&self) -> u32 {
        self.width
    }

    #[allow(dead_code)] // read by `output/{sdr,hdr}-display-rendering`.
    pub fn height(&self) -> u32 {
        self.height
    }

    /// Read-only view of the interleaved adjusted linear-ACEScg pixels.
    #[allow(dead_code)] // read by `output/{sdr,hdr}-display-rendering`.
    pub fn rgb(&self) -> &[f32] {
        &self.rgb
    }

    /// Read-only view of the carried IR plane, when the input had one.
    #[allow(dead_code)] // read by `output/{sdr,hdr}-display-rendering`.
    pub fn ir(&self) -> Option<&[f32]> {
        self.ir.as_deref()
    }
}

impl std::fmt::Debug for AdjustedAcesCgImage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdjustedAcesCgImage")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("ir", &self.ir.is_some())
            .finish_non_exhaustive()
    }
}

/// The resolved-once display source: the adjusted buffer plus the controls that
/// produced it (for the JSON report, and so a branch can reason about what was
/// already applied instead of guessing).
#[allow(dead_code)] // consumed by `output/{sdr,hdr}-display-rendering`.
pub struct SharedDisplaySource {
    pub source: AdjustedAcesCgImage,
    pub controls: ResolvedPrintControls,
}

/// Resolve the shared print controls for this frame (design-spec §6).
///
/// An explicit [`WbSource`] passes its gains straight through; an **auto** mode is
/// estimated from a deterministic strided sample of the *mapped ACEScg* buffer
/// with the same estimators the legacy print render uses
/// (`density::estimate_wb_gains`). Note the domain difference this implies: the
/// legacy estimate runs on pre-matrix film RGB, so an auto mode resolves to
/// *different numbers* here — per-channel gains do not commute with the
/// working-space matrix. That is the documented consequence of moving the
/// controls after the ACEScg boundary, not a bug.
///
/// Pure and deterministic: same buffer + same params ⇒ same controls. The only
/// producer of a [`ResolvedPrintControls`], so its checked constructor is what makes
/// the subsequent [`apply_shared_controls`] arithmetic infallible.
#[allow(dead_code)] // called by `output/{sdr,hdr}-display-rendering`.
pub fn resolve_shared_controls(
    aces: &AcesCgImage,
    print: &PrintParams,
) -> Result<ResolvedPrintControls> {
    let white_balance = match print.white_balance {
        WbSource::Explicit(gains) => gains,
        auto_mode => {
            let sampled = density::sample_positive(aces.rgb());
            density::estimate_wb_gains(&sampled, auto_mode)?
        }
    };
    ResolvedPrintControls::new(
        white_balance,
        2f32.powf(print.print_exposure),
        print.black_point,
        print.linear_range,
    )
}

/// Apply the shared print controls in the **pinned** order (design-spec §6):
///
/// ```text
/// white balance → exposure → black point → linear_range placement
/// ```
///
/// per channel, `v ← (((v · wb_c) · 2^EV) − black − low) / (high − low)`. The
/// default controls (`[1,1,1]`, `0` EV, `0` black, `[0, 1]` range) are the exact
/// identity, so a defaulted shared stage cannot perturb pixels.
///
/// Nothing is clamped (range clamping stays at the u16 encode step) and nothing
/// is highlight-compressed — SDR highlight roll-off is branch-specific policy. A
/// non-finite input stays non-finite.
#[allow(dead_code)] // called by `output/{sdr,hdr}-display-rendering`.
pub fn apply_shared_controls(
    aces: AcesCgImage,
    controls: &ResolvedPrintControls,
) -> AdjustedAcesCgImage {
    let ResolvedPrintControls {
        white_balance: wb,
        exposure_gain,
        black_point,
        linear_range: [low, high],
    } = *controls;
    // Finite and `> 0` by construction: `ResolvedPrintControls::new` is the only
    // constructor and checks it, so the divide below cannot be by zero, by a negative,
    // or by a non-finite value. It can still divide by a *subnormal* span
    // (`[0.0, 1e-40]` passes both `new` and `cli::validate`) and produce `inf` — that is
    // deliberate: `io::encode`'s non-finite counter reports it loudly, which is the
    // designed channel, unlike the silent underflow `new` guards the gains against.
    let span = high - low;

    let mut image = aces.into_linear();
    // `len % 3 == 0` is a `LinearImage` invariant; `as_chunks_mut` would silently
    // drop a 1–2 element tail, so assert it (`working_space.rs` uses the same
    // `debug_assert!`; `color.rs`, whose `rgb` field is `pub` and reachable from
    // outside its module, returns an `Err` instead). Debug-only, so this documents
    // and catches a regression in tests rather than guarding release.
    let (pixels, rest) = image.rgb.as_chunks_mut::<3>();
    debug_assert!(
        rest.is_empty(),
        "LinearImage rgb length must be a multiple of 3"
    );
    for px in pixels {
        for c in 0..3 {
            let exposed = px[c] * wb[c] * exposure_gain;
            px[c] = (exposed - black_point - low) / span;
        }
    }

    AdjustedAcesCgImage::new(image)
}

/// The display branch of the split: resolve the shared controls once and apply
/// them, yielding the single source **both** SDR and HDR consume.
#[allow(dead_code)] // called by `output/{sdr,hdr}-display-rendering`.
pub fn display_source(aces: AcesCgImage, print: &PrintParams) -> Result<SharedDisplaySource> {
    let controls = resolve_shared_controls(&aces, print)?;
    let source = apply_shared_controls(aces, &controls);
    Ok(SharedDisplaySource { source, controls })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::algo::reconstruct;
    use crate::pipeline::working_space::map_nc_film_rgb_v1;
    use crate::types::{
        DensityCurve, DensityParams, ExponentialParams, FilmBase, Reconstruction, SigmoidParams,
    };

    /// Build an `AcesCgImage` whose *film RGB* input was exactly `rgb`, through
    /// the real `reconstruct → map_nc_film_rgb_v1` path (the only way to mint
    /// one). `simple` with a unit base and a pre-inverted scan gives
    /// `1 − (1 − target)/1 == target` bit-for-bit, so the film RGB entering the
    /// mapper is exactly `rgb` — the same trick `working_space`'s tests use.
    fn aces_from(width: u32, height: u32, rgb: &[f32], ir: Option<Vec<f32>>) -> AcesCgImage {
        let base = FilmBase::from([1.0, 1.0, 1.0]);
        let scan: Vec<f32> = rgb.iter().map(|&t| 1.0 - t).collect();
        let img = LinearImage::new(width, height, scan, ir).unwrap();
        let (film, _) = reconstruct(&img, &base, &Reconstruction::Simple).unwrap();
        map_nc_film_rgb_v1(film)
    }

    /// Every supported reconstruction config — the split must be indifferent to
    /// which one produced the `AcesCgImage`.
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

    // -- film-master branch ----------------------------------------------------

    #[test]
    fn film_master_is_a_bit_exact_unwrap_of_the_mapped_buffer() {
        // The master is a pure unwrap of the mapped ACEScg buffer: the pixels are
        // bit-identical to `map_nc_film_rgb_v1`'s output. `film_master` does not
        // even *take* a `PrintParams`, so a non-default control cannot leak in —
        // this pins the value side of that type-level guarantee.
        let px = [0.85_f32, 0.5, 0.38, 0.02, 0.012, 0.009];
        let want: Vec<u32> = aces_from(2, 1, &px, None)
            .rgb()
            .iter()
            .map(|v| v.to_bits())
            .collect();
        let got: Vec<u32> = film_master(aces_from(2, 1, &px, None))
            .rgb
            .iter()
            .map(|v| v.to_bits())
            .collect();
        assert_eq!(got, want, "film-master must not touch the mapped pixels");
    }

    #[test]
    fn film_master_round_trips_unclamped_finite_and_non_finite_values() {
        // Out-of-`[0,1]` finite values survive unclamped (the master is unclamped
        // by definition — it never reaches the u16 quantizer), and a non-finite
        // sample propagates for `io::encode` to count instead of being swallowed.
        let out = film_master(aces_from(
            3,
            1,
            &[5.0, 5.0, 5.0, -2.0, -2.0, -2.0, f32::NAN, 0.1, 0.2],
            None,
        ));
        assert!(
            out.rgb[0] > 4.9,
            "above-one neutral clamped: {}",
            out.rgb[0]
        );
        assert!(out.rgb[3] < -1.9, "negative clamped: {}", out.rgb[3]);
        assert!(
            out.rgb[6..9].iter().all(|v| v.is_nan()),
            "NaN must propagate, got {:?}",
            &out.rgb[6..9]
        );
    }

    #[test]
    fn film_master_carries_dimensions_and_ir() {
        let out = film_master(aces_from(
            2,
            1,
            &[0.4, 0.5, 0.6, 0.1, 0.2, 0.3],
            Some(vec![0.25, 0.75]),
        ));
        assert_eq!((out.width, out.height), (2, 1));
        assert_eq!(out.ir.as_deref(), Some(&[0.25_f32, 0.75][..]));
    }

    #[test]
    fn split_is_producer_agnostic_over_every_reconstruction_path() {
        // The split's input is `AcesCgImage` regardless of which reconstruction
        // produced it (compiler-enforced by the signatures; this exercises all
        // three paths so no path is accidentally excluded).
        let scan = vec![0.5, 0.3, 0.2, 0.05, 0.03, 0.02];
        let base = FilmBase::from([0.9, 0.55, 0.42]);
        for config in all_configs() {
            let img = LinearImage::new(2, 1, scan.clone(), Some(vec![0.1, 0.9])).unwrap();
            let (film, _) = reconstruct(&img, &base, &config).unwrap();
            let master = film_master(map_nc_film_rgb_v1(film));
            assert_eq!(master.rgb.len(), 6, "{config:?}");
            assert_eq!(
                master.ir.as_deref(),
                Some(&[0.1_f32, 0.9][..]),
                "{config:?}"
            );

            let img = LinearImage::new(2, 1, scan.clone(), None).unwrap();
            let (film, _) = reconstruct(&img, &base, &config).unwrap();
            let shared = display_source(map_nc_film_rgb_v1(film), &PrintParams::default()).unwrap();
            assert_eq!(shared.source.rgb().len(), 6, "{config:?}");
        }
    }

    // -- shared display controls ------------------------------------------------

    #[test]
    fn default_shared_controls_are_the_exact_identity() {
        // WB [1,1,1] → 2^0 → −0 → (x−0)/(1−0). Every step is an exact-identity
        // f32 operation, so the adjusted buffer must be bit-identical to the
        // mapped ACEScg input — the guarantee that a defaulted display branch
        // adds no error of its own.
        let px = [0.85_f32, 0.5, 0.38, 0.3, 0.18, 0.12];
        let want: Vec<u32> = aces_from(2, 1, &px, None)
            .rgb()
            .iter()
            .map(|v| v.to_bits())
            .collect();
        let shared = display_source(aces_from(2, 1, &px, None), &PrintParams::default()).unwrap();
        let got: Vec<u32> = shared.source.rgb().iter().map(|v| v.to_bits()).collect();
        assert_eq!(got, want);
        assert_eq!(
            shared.controls,
            ResolvedPrintControls {
                white_balance: [1.0, 1.0, 1.0],
                exposure_gain: 1.0,
                black_point: 0.0,
                linear_range: [0.0, 1.0],
            }
        );
    }

    #[test]
    fn shared_controls_apply_in_the_pinned_wb_exposure_black_range_order() {
        // Deliberately asymmetric values so *every* pairwise reordering changes the
        // result. Input film RGB is neutral, and the mapper's rows sum to 1, so the
        // mapped ACEScg value of a neutral input is that same neutral value — which
        // makes the expected arithmetic exact and hand-checkable.
        //
        //   x = 0.5, wb = 2.0, EV = 1 (gain 2), black = 0.25, range = [0.5, 1.5]
        //   WB → exposure → black → range: ((0.5·2·2) − 0.25 − 0.5)/1.0 = 1.25
        // Reorderings differ, e.g.
        //   black before exposure:  ((0.5·2) − 0.25)·2 = 1.5      → /1.0 − 0.5 = 1.0
        //   range before black:     ((0.5·2·2) − 0.5)/1.0 − 0.25  = 1.25 (same!),
        // so the black/range pair is additionally pinned by a non-unit span below.
        let print = PrintParams {
            print_exposure: 1.0,
            black_point: 0.25,
            white_balance: WbSource::Explicit([2.0, 1.0, 0.5]),
            linear_range: [0.5, 1.5],
            ..PrintParams::default()
        };
        let shared = display_source(aces_from(1, 1, &[0.5, 0.5, 0.5], None), &print).unwrap();
        let got = shared.source.rgb();
        for (c, want) in [1.25_f32, 0.25, -0.25].into_iter().enumerate() {
            assert!(
                (got[c] - want).abs() < 2e-6,
                "channel {c}: got {} want {want}",
                got[c]
            );
        }

        // A non-unit span separates "black then range" from "range then black":
        //   ((0.5·1·1) − 0.25 − 0.0)/0.5 = 0.5   (pinned order)
        //   ((0.5·1·1) − 0.0)/0.5 − 0.25 = 0.75  (range before black)
        let print = PrintParams {
            black_point: 0.25,
            linear_range: [0.0, 0.5],
            ..PrintParams::default()
        };
        let shared = display_source(aces_from(1, 1, &[0.5, 0.5, 0.5], None), &print).unwrap();
        assert!(
            (shared.source.rgb()[0] - 0.5).abs() < 2e-6,
            "black must be subtracted before the range placement, got {}",
            shared.source.rgb()[0]
        );

        // …and a non-unit WB gain separates "WB then exposure" from a lone
        // exposure: exposure alone would give 0.5·2 = 1.0, not 2.0.
        let print = PrintParams {
            print_exposure: 1.0,
            white_balance: WbSource::Explicit([2.0, 2.0, 2.0]),
            ..PrintParams::default()
        };
        let shared = display_source(aces_from(1, 1, &[0.5, 0.5, 0.5], None), &print).unwrap();
        assert!(
            (shared.source.rgb()[0] - 2.0).abs() < 2e-6,
            "WB and exposure must both apply, got {}",
            shared.source.rgb()[0]
        );
    }

    #[test]
    fn the_shared_adjusted_source_is_one_buffer_with_the_resolved_controls_applied() {
        // "SDR and HDR receive the identical adjusted source" is **structural**, not a
        // value property worth asserting: `SharedDisplaySource` owns exactly one
        // `AdjustedAcesCgImage`, a renderer is handed `&shared.source`, and
        // `AdjustedAcesCgImage`'s constructor is module-private so neither branch can
        // mint its own. (An earlier version of this test compared one `&` against
        // itself, which can never fail.) What IS worth pinning is that the single
        // buffer really carries the resolved controls — so recompute it independently
        // from the reported `controls` and the mapped input.
        let px = [0.85_f32, 0.5, 0.38, 0.3, 0.18, 0.12, 0.02, 0.012, 0.009];
        let print = PrintParams {
            print_exposure: 0.5,
            black_point: 0.02,
            white_balance: WbSource::Explicit([1.05, 1.0, 0.93]),
            linear_range: [0.01, 0.98],
            ..PrintParams::default()
        };
        let mapped: Vec<f32> = aces_from(3, 1, &px, None).rgb().to_vec();
        let shared = display_source(aces_from(3, 1, &px, None), &print).unwrap();
        assert_eq!((shared.source.width(), shared.source.height()), (3, 1));

        let c = &shared.controls;
        let [low, high] = c.linear_range();
        for (i, &src) in mapped.iter().enumerate() {
            let want = (src * c.white_balance()[i % 3] * c.exposure_gain() - c.black_point() - low)
                / (high - low);
            assert!(
                (shared.source.rgb()[i] - want).abs() < 2e-6,
                "sample {i}: got {} want {want}",
                shared.source.rgb()[i]
            );
        }
    }

    #[test]
    fn auto_white_balance_resolves_once_to_concrete_gains() {
        // An auto mode is resolved to explicit gains *before* the branches split,
        // so neither can re-estimate; and feeding the resolved gains back as
        // explicit reproduces the buffer bit-for-bit. Both estimators, since each
        // reaches `estimate_wb_gains` through this same slot.
        let px = [0.85_f32, 0.5, 0.38, 0.3, 0.18, 0.12, 0.02, 0.012, 0.009];
        for mode in [WbSource::Percentile, WbSource::GrayWorld] {
            let auto = PrintParams {
                white_balance: mode,
                ..PrintParams::default()
            };
            let shared = display_source(aces_from(3, 1, &px, None), &auto).unwrap();
            let gains = shared.controls.white_balance();
            assert!(
                gains.iter().all(|g| g.is_finite() && *g > 0.0),
                "{mode:?}: resolved gains must be usable: {gains:?}"
            );

            let frozen = PrintParams {
                white_balance: WbSource::Explicit(gains),
                ..PrintParams::default()
            };
            let replay = display_source(aces_from(3, 1, &px, None), &frozen).unwrap();
            assert_eq!(replay.controls, shared.controls, "{mode:?}");
            let bits = |img: &AdjustedAcesCgImage| -> Vec<u32> {
                img.rgb().iter().map(|v| v.to_bits()).collect()
            };
            assert_eq!(bits(&replay.source), bits(&shared.source), "{mode:?}");
        }
    }

    #[test]
    fn resolved_controls_reject_the_values_the_arithmetic_cannot_survive() {
        // `apply_shared_controls` divides by the range span and multiplies by the
        // gains, so `ResolvedPrintControls::new` — the sole constructor — is the guard.
        // Each case below is reachable from a *finite* `print.*` value that
        // `cli::validate`'s own `finite()` checks accept, so this is not
        // belt-and-braces: `2^200` overflows to `inf` from a finite stop value.
        let px = [0.5_f32, 0.5, 0.5];
        let bad = [
            (
                "overflowing exposure",
                PrintParams {
                    print_exposure: 200.0,
                    ..PrintParams::default()
                },
            ),
            (
                "underflowing exposure",
                PrintParams {
                    print_exposure: -200.0,
                    ..PrintParams::default()
                },
            ),
            (
                // `2^-140` is finite AND non-zero (f32 subnormals reach `2^-149`), so
                // the weaker `is_finite() && != 0.0` guard let it through — and every
                // `px · wb · gain` product then underflows to 0.0, an all-black image
                // that trips neither the clip nor the non-finite counter.
                "subnormal exposure gain",
                PrintParams {
                    print_exposure: -140.0,
                    ..PrintParams::default()
                },
            ),
            (
                // Neither factor is individually invalid — `1e-30` is a positive finite
                // gain and `2^-100` (`7.9e-31`) is normal — but their product is
                // `7.9e-61`, which in f32 is *exactly* `0.0`, zeroing every sample of
                // every channel with no counter firing. This is the case that makes the
                // per-factor guards insufficient on their own.
                "individually-valid factors whose product collapses",
                PrintParams {
                    white_balance: WbSource::Explicit([1e-30, 1e-30, 1e-30]),
                    print_exposure: -100.0,
                    ..PrintParams::default()
                },
            ),
            (
                // …and the same trap on one channel only, so the per-channel loop is
                // exercised rather than a whole-triple shortcut.
                "one channel's product collapses",
                PrintParams {
                    white_balance: WbSource::Explicit([1.0, 1e-30, 1.0]),
                    print_exposure: -100.0,
                    ..PrintParams::default()
                },
            ),
            (
                // The overflow direction of the same product trap.
                "product overflows",
                PrintParams {
                    white_balance: WbSource::Explicit([1e30, 1.0, 1.0]),
                    print_exposure: 100.0,
                    ..PrintParams::default()
                },
            ),
            (
                "zero gain",
                PrintParams {
                    white_balance: WbSource::Explicit([1.0, 0.0, 1.0]),
                    ..PrintParams::default()
                },
            ),
            (
                "negative gain",
                PrintParams {
                    white_balance: WbSource::Explicit([1.0, -1.0, 1.0]),
                    ..PrintParams::default()
                },
            ),
            (
                "non-finite black point",
                PrintParams {
                    black_point: f32::NAN,
                    ..PrintParams::default()
                },
            ),
            (
                "degenerate range",
                PrintParams {
                    linear_range: [0.5, 0.5],
                    ..PrintParams::default()
                },
            ),
            (
                "inverted range",
                PrintParams {
                    linear_range: [1.0, 0.0],
                    ..PrintParams::default()
                },
            ),
            (
                "unrepresentable span",
                PrintParams {
                    linear_range: [-f32::MAX, f32::MAX],
                    ..PrintParams::default()
                },
            ),
        ];
        for (name, print) in bad {
            match resolve_shared_controls(&aces_from(1, 1, &px, None), &print) {
                Err(e) => assert_eq!(e.exit_code(), 2, "{name}: wrong exit code"),
                Ok(c) => panic!("{name} must be rejected, got {c:?}"),
            }
        }
        // …and the defaults, plus a legitimately non-unit set, are accepted.
        for print in [
            PrintParams::default(),
            PrintParams {
                print_exposure: -1.5,
                black_point: 0.01,
                white_balance: WbSource::Explicit([1.05, 1.0, 0.93]),
                linear_range: [0.02, 0.95],
                ..PrintParams::default()
            },
        ] {
            resolve_shared_controls(&aces_from(1, 1, &px, None), &print).unwrap();
        }
    }

    #[test]
    fn shared_stage_does_not_clamp_or_highlight_compress() {
        // Highlight compression is branch-specific SDR policy, so a non-zero
        // `highlight_compress` must NOT be applied by the shared stage: with
        // otherwise-default controls the result stays the exact identity even for a
        // sample far above 1.0 (the legacy soft-clip would have bounded it).
        let print = PrintParams {
            highlight_compress: 0.5,
            ..PrintParams::default()
        };
        let shared = display_source(aces_from(1, 1, &[8.0, 8.0, 8.0], None), &print).unwrap();
        assert!(
            shared.source.rgb()[0] > 7.9,
            "shared stage must not roll off highlights, got {}",
            shared.source.rgb()[0]
        );
    }

    #[test]
    fn shared_stage_carries_the_ir_plane_and_dimensions() {
        let shared = display_source(
            aces_from(2, 1, &[0.4, 0.5, 0.6, 0.1, 0.2, 0.3], Some(vec![0.2, 0.8])),
            &PrintParams::default(),
        )
        .unwrap();
        assert_eq!((shared.source.width(), shared.source.height()), (2, 1));
        assert_eq!(shared.source.ir(), Some(&[0.2_f32, 0.8][..]));
    }

    #[test]
    fn wb_gains_do_not_commute_with_the_working_space_matrix() {
        // The documented migration boundary (design-spec §7.1): moving the simple
        // controls after the ACEScg mapping preserves the requested *values*, never
        // the legacy *pixels*. Per-channel gains are a diagonal matrix, and a
        // diagonal only commutes with the working-space matrix when its entries are
        // equal — so `M·(diag(g)·v)` ≠ `diag(g)·(M·v)` for non-uniform gains. Pinned
        // here so nobody later promises bit-identical migration.
        let film = [0.8_f32, 0.4, 0.2];
        let gains = [1.4_f32, 1.0, 0.7];

        // (a) gains applied *after* the mapping — what the shared display stage does.
        let after = display_source(
            aces_from(1, 1, &film, None),
            &PrintParams {
                white_balance: WbSource::Explicit(gains),
                ..PrintParams::default()
            },
        )
        .unwrap();

        // (b) gains applied *before* the mapping — the legacy pre-ACEScg placement.
        let pre: Vec<f32> = film.iter().zip(gains).map(|(v, g)| v * g).collect();
        let before = aces_from(1, 1, &pre, None);

        let (a, b) = (after.source.rgb(), before.rgb());
        assert!(
            a.iter().zip(b).any(|(x, y)| (x - y).abs() > 1e-3),
            "non-uniform gains must NOT commute with the matrix: after {a:?} vs before {b:?}"
        );

        // …and the control-free case is the sanity check on the fixture itself: a
        // *uniform* gain is a scalar, which does commute, so the difference above is
        // genuinely the ordering and not a broken helper.
        let uniform = [1.4_f32, 1.4, 1.4];
        let after_u = display_source(
            aces_from(1, 1, &film, None),
            &PrintParams {
                white_balance: WbSource::Explicit(uniform),
                ..PrintParams::default()
            },
        )
        .unwrap();
        let pre_u: Vec<f32> = film.iter().zip(uniform).map(|(v, g)| v * g).collect();
        let before_u = aces_from(1, 1, &pre_u, None);
        for (x, y) in after_u.source.rgb().iter().zip(before_u.rgb()) {
            assert!(
                (x - y).abs() < 2e-6,
                "uniform gain must commute: {x} vs {y}"
            );
        }
    }

    #[test]
    fn the_master_is_never_a_display_source() {
        // `film_master` yields a plain `LinearImage`, never an
        // `AdjustedAcesCgImage` — so a display renderer that takes a
        // `SharedDisplaySource` (or its `&AdjustedAcesCgImage`) cannot be handed the
        // master, and the master cannot pick up branch tone/gamut/transfer work.
        // That is compiler-enforced; the value side is that with a non-default
        // control the two branches genuinely diverge (identical output would mean
        // the shared stage was a no-op and the distinction vacuous).
        let px = [0.7_f32, 0.4, 0.25];
        let print = PrintParams {
            print_exposure: 1.0,
            ..PrintParams::default()
        };
        let master = film_master(aces_from(1, 1, &px, None));
        let display = display_source(aces_from(1, 1, &px, None), &print).unwrap();
        assert!(
            (display.source.rgb()[0] - master.rgb[0] * 2.0).abs() < 2e-6,
            "the display branch applies 2^1; the master does not \
             (display {} vs master {})",
            display.source.rgb()[0],
            master.rgb[0]
        );
    }

    // The split's *construction* boundary is compiler-enforced and cannot be
    // exercised at runtime: `AcesCgImage`'s fields are private and its `fn new` is
    // module-private to `pipeline::working_space`, so `map_nc_film_rgb_v1` is the only
    // producer — and every entry point here takes an `AcesCgImage` (or, for the
    // branches, a `SharedDisplaySource` whose only producer is
    // `apply_shared_controls`). So a `FilmRgbImage` or a raw device-RGB `LinearImage`
    // cannot be handed to `film_master` / `display_source`. It does *not* follow that
    // an unmapped buffer can never be tagged with a named profile: `io::encode` takes
    // a `LinearImage` and an `OutputParams` with no relationship between them, and
    // keeping the tag matched to the pixels is the orchestrator's job (see the module
    // doc). Every fixture above routes through `reconstruct` → `map_nc_film_rgb_v1`,
    // the direct uncorrected mapper path this task is scoped to. (A `trybuild`
    // compile-fail case would need a dev-dependency the crate deliberately avoids;
    // see `algo/mod.rs`.)
}
