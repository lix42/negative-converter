//! Peak-memory preflight: one sizing model for "how much will this run
//! allocate", the operational budget it is checked against, and the fail-loud
//! verdict (`io/memory-preflight`).
//!
//! ## Why this exists
//!
//! `io::decode::decode_limits` advertises a 4 GiB *input read buffer*, which
//! guards only the `tiff` crate's `u16` staging buffer. The derived peak is a
//! multiple of it, because whole-image `f32` buffers **overlap**: the
//! orchestrator holds the decoded image until after the render (it is the source
//! for `--export-ir`), while the algorithm produces the positive. So the run's
//! real ceiling was never checked at all. This module makes it explicit and
//! checkable *before* the first big allocation.
//!
//! ## The model
//!
//! Per-phase accounting of the full-frame buffers that are **simultaneously
//! live**, in bytes-per-pixel for the shipped 16-bit RGB + IR input (`decoded` =
//! the decoded image, `rgb32` = one interleaved `f32` RGB buffer, `ir32` = one
//! `f32` IR plane):
//!
//! ```text
//! decode     rgb32 + max(rgb16 read buffer, ir16 + ir32)      18 B/px
//! film-base  decoded (rgb32+ir32) + 3 f32 channel vectors     16 + 12·s B/px
//!            over the sampled rectangle
//! render     decoded + positive + retained sample             32 + 12·s B/px
//! encode     decoded + rendered + u16 quantize + retained     38 + 12·s B/px
//! ```
//!
//! `hdr-pq` / `hdr-hlg` ([`RunProfile::HdrAvif`]) replace the last two rows — one
//! display rendition instead of a positive, and a native encoder instead of a u16
//! quantize buffer:
//!
//! ```text
//! render     decoded + shared ACEScg + BT.2020 rendition      2·decoded + 12 + 12·s B/px
//! encode     decoded + rendition + AVIF staging               28 + 48 + 12·s B/px
//! ```
//!
//! The shared ACEScg source is `decoded`-shaped rather than RGB-only — it carries
//! the IR plane through, so it is 16 B/px on an HDRi input and 12 on an IR-free
//! one — which is why the render row counts `decoded` twice instead of adding a
//! flat 12.
//!
//! The transfer (PQ or HLG) is applied *in place* to the rendition, so it adds
//! nothing. [`AVIF_STAGING_BYTES_PER_PX`] is the calibrated 48.
//!
//! `hdr-linear-tiff` ([`RunProfile::HdrLinearTiff`]) shares that render row and then
//! drops the staging entirely — f32 is written verbatim, and the `tiff` writer
//! streams strips rather than assembling the file in memory:
//!
//! ```text
//! render     decoded + shared ACEScg + BT.2020 rendition      2·decoded + 12 + 12·s B/px
//! encode     decoded + rendition                             28 + 12·s B/px
//! ```
//!
//! Its peak is therefore the **render** phase rather than encode, as it is for
//! `hdr-pq-tiff`/`hdr-hlg-tiff` (no container is assembled in memory) and — for a
//! different reason — for `ultra-hdr-v1`, whose four simultaneous display buffers
//! outweigh its encode set. Which phase peaks is **per profile**, not a property of
//! any category: `which_phase_peaks_is_per_profile_and_measured_not_assumed` pins
//! each one, because prose about it has been wrong twice. Note a lossless-TIFF
//! compression option would add a staging term and could move this peak to encode.
//!
//! `s` is the sampled rectangle as a fraction of the frame ([`SamplePlan`]): `0`
//! for an explicit `--film-base` (nothing is sampled), ~0.69 for the `auto` path's
//! frame interior on a 3:2 frame, up to 1.0 for a full-frame `--base-region` /
//! `--d-max-region`. `estimate --grid` gathers five cells of its rectangle one at
//! a time, so it counts as one *cell*
//! ([`grid_cell_pixels`](crate::pipeline::film_base::grid_cell_pixels), ~1/16 of
//! the rectangle), not the whole rectangle.
//!
//! Notes on the non-obvious entries:
//!
//! - **Two images at render, not one.** The in-place color transform removed the
//!   third (`pipeline::color::to_output` used to clone), but the decoded image
//!   still outlives the render, so `render` counts two.
//! - **Freed is not gone: one retention rule, applied everywhere.** Peak RSS is a
//!   high-water mark and the allocator does not return freed pages to the OS, so a
//!   buffer freed mid-run still occupies the process at every later peak. Hence
//!   the film-base sample is **added into** the render and encode phases rather
//!   than competing with them, and the IR export buffer is summed with the
//!   quantize buffer it precedes. The one place the rule does *not* apply is
//!   `decode`'s `u16` read buffer, a genuine alternative (`max`): it is freed and
//!   the IR buffers are allocated into the space it just vacated, within one stage.
//!
//!   Retention is measurement-driven, not decorative. A full-frame `--base-region`
//!   `convert` measures **3.743 GB**, which 50 B/px (32 render + 6 quantize + 12
//!   retained sample) reproduces to +0.3%; modelling the sample as a *competing*
//!   phase instead put that run at 38 B/px and **under-estimated it by 10.2%**.
//!   The IR-export sum is the conservative end of the same rule — there the
//!   calibration table shows `convert` and `convert --export-ir` peaking at the
//!   *same* RSS, so summing over-counts by 2 B/px, which this module prefers.
//! - **`film_base` *does* allocate a full-frame-scale buffer.** It samples
//!   rectangles, but `film_base::region_channels` materializes each one
//!   *unstrided* into three `Vec<f32>` — 12 bytes per sampled pixel — and the
//!   `auto` path's interior rectangle
//!   ([`film_base::auto_interior_pixels`](crate::pipeline::film_base::auto_interior_pixels))
//!   is ~69% of a 3:2 frame. It is live alongside the decoded image, so the phase
//!   costs ~24 B/px there, and 28 B/px for a full-frame rectangle. For
//!   [`RunProfile::DecodeOnly`] (`inspect` / `estimate`, which stop after
//!   sampling) that phase **is** the peak, well above decode's 18 B/px. For
//!   [`RunProfile::Convert`] the phase itself stays under the encode phase — but
//!   the sample is *retained* into it (see the retention rule above), so sampling
//!   still raises `convert`'s peak rather than being free.
//!
//!   This note replaces an earlier claim that `film_base` allocated no full-frame
//!   buffer. That claim was false twice over: it let `inspect`/`estimate` be
//!   admitted at 18 B/px and then exceed their own predicted peak (measured +26%
//!   on the auto-interior rectangle, +43% on a full-frame one), and the first fix
//!   — counting the phase but letting it *compete* — still under-estimated a
//!   full-frame-region `convert` by 10.2%.
//!
//! On top of the named buffers sits an **allowance** ([`ALLOWANCE_PERCENT`] +
//! [`ALLOWANCE_FIXED_BYTES`]) for what a byte-exact model cannot see: allocator
//! slack and freed-but-resident pages, the binary and its static data, lcms2
//! profiles and transforms, and the `tiff` writer's buffering. It is calibrated
//! to **over**-estimate on every measured point — a preflight that
//! under-estimates would approve a run that then OOMs, which is the failure this
//! task exists to prevent.
//!
//! **Nothing enforces this model against the code.** The test
//! `phase_totals_match_the_documented_bytes_per_pixel` compares `estimate_peak`
//! against the per-pixel figures written *in this doc*; no test observes what the
//! pipeline actually allocates. So a new full-frame buffer in any stage stays
//! invisible until someone adds it here by hand, and the gate silently
//! under-approves in the meantime. That failure has already happened once — the
//! film-base phase above was missing from the first version of this model.
//!
//! ## Calibration (macOS/aarch64, `/usr/bin/time -l` peak RSS, 2026-07-27)
//!
//! Measured on `largest.tif` (10368x7200 = 74.65 MP HDRi, the largest scan on
//! hand), a standard roll frame (5184x3599 = 18.66 MP HDRi), and the Ultra HDR
//! gain-map calibration scan (5184x3600 = 18.66 MP HDRi). Units: measured peaks
//! and model outputs in decimal GB (that is what `time -l` reports against);
//! budgets and the allowance in GiB/MiB. The calibration conversions used an
//! explicit film base, so their render/encode peaks carry no sampled rectangle.
//!
//! Every row is model-vs-measured for the *same* invocation; the model must never
//! come in under measured. The sampling rows are what the film-base phase was
//! added for — before it was modelled, the last three 74.65 MP rows here
//! under-estimated by 26%, 43% and 10% respectively.
//!
//! | run | model | measured | margin |
//! |---|---|---|---|
//! | `convert` u16 74.65 MP | 3.396 GB | 3.146 GB | +8.0% |
//! | `convert --export-ir` u16 74.65 MP | 3.568 GB | 3.146 GB | +13.4% |
//! | `convert --output-hdr` 74.65 MP | 2.881 GB | 2.698 GB | +6.8% |
//! | `estimate --film-base` 74.65 MP (no sampling) | 1.679 GB | 1.502 GB | +11.8% |
//! | `estimate --grid` 74.65 MP | 1.679 GB | 1.558 GB | +7.8% |
//! | `estimate --base-region` (auto-interior rect) 74.65 MP | 2.217 GB | 2.119 GB | +4.6% |
//! | `estimate --base-region` (full frame) 74.65 MP | 2.538 GB | 2.398 GB | +5.8% |
//! | `convert --base-region` (full frame) 74.65 MP | 4.427 GB | 3.743 GB | +18.3% |
//! | `convert` u16 18.66 MP | 0.950 GB | 0.681 GB | +39.4% |
//! | `ultra-hdr-v1` 18.66 MP (explicit base, no IR export) | 1.851 GB | 1.681 GB | +10.1% |
//! | `hdr-pq` 18.66 MP (explicit base, no IR export) | 1.765 GB | 1.472 GB | +19.9% |
//! | `hdr-pq` 74.65 MP (explicit base, no IR export) | 6.659 GB | 5.866 GB | +13.5% |
//! | `hdr-hlg` 18.66 MP (explicit base, no IR export) | 1.765 GB | 1.503 GB | +17.5% |
//! | `hdr-linear-tiff` 18.66 MP (explicit base, no IR export) | 1.078 GB | 0.907 GB | +18.9% |
//! | `hdr-linear-tiff` 74.65 MP (explicit base, no IR export) | 3.911 GB | 3.578 GB | +9.3% |
//! | `hdr-pq-tiff` 18.66 MP (explicit base, no IR export) | 1.078 GB | 0.906 GB | +19.0% |
//! | `hdr-hlg-tiff` 18.66 MP (explicit base, no IR export) | 1.078 GB | 0.906 GB | +19.0% |
//! | `hdr-pq-tiff` 74.65 MP (explicit base, no IR export) | 3.911 GB | (not measured) | — |
//!
//! Two sources of slack are visible and deliberate. Small frames run looser
//! (+39.4% for the u16 18.66 MP run) because [`ALLOWANCE_FIXED_BYTES`] stops being negligible —
//! harmless, since they are nowhere near any plausible budget. And `inspect` on
//! these assets measures far under its estimate (1.502 GB vs 2.217 GB) because the
//! model charges the `auto` path's interior sample while the detector actually
//! *refuses* on every real scan we have (no uniform rebate band ⇒ it errors before
//! sampling). The model cannot know that in advance, so it charges the sample it
//! would take. That is the conservative direction, and it will tighten on its own
//! when `film-base/content-fallback` makes the auto path succeed.
//!
//! No Ultra HDR run with `--export-ir` has been measured. Its optional export
//! therefore retains the TIFF model's conservative 2 B/px u16 staging term;
//! tests pin that structural increment without claiming it as a calibrated RSS
//! observation. The same holds for `hdr-pq`/`hdr-hlg` with `--export-ir`.
//!
//! The five TIFF-HDR rows share one number per frame size because all three presets
//! peak at the **render** phase, which is identical across them (they share
//! `hdr::render_linear`); the u16 quantize buffer that distinguishes `hdr-*-tiff`
//! lands in the cheaper encode phase and never sets the peak. Their margin is wider
//! than `hdr-pq`'s at the same size because the AVIF profile's calibrated codec
//! staging pushes *its* peak to encode, where the model was fitted; here the peak is
//! a phase built only from enumerated buffers, so the residual is unmodelled
//! allocator and writer overhead. Every one of these estimates stays **above**
//! measured, which is the direction the gate requires.
//!
//! The two `hdr-pq` rows are the *pair* that solved
//! [`AVIF_STAGING_BYTES_PER_PX`] — they are a fit, not two independent
//! confirmations, and their 78.47 B/px slope is what a future re-calibration
//! should re-measure. The `hdr-hlg` row is an independent check of the shared
//! profile: HLG differs from PQ only in a transfer applied in place to an
//! already-allocated buffer, and it measured 1.503 GB against the same 1.765 GB
//! estimate, so one profile covers both. (It does emit a substantially larger
//! codestream — 621 KB against PQ's 72 KB on that frame — which costs allocation
//! only inside the lumped staging term.)
//!
//! Peak RSS varies by a few tens of KB between identical runs; the frozen literals
//! in the tests are single observations, which is why the assertions are
//! `estimate >= measured` rather than equality.
//!
//! Measured overhead over the *accounted* buffers peaked at **12.9%** on the
//! no-sampling rows, which is why [`ALLOWANCE_PERCENT`] sits above it rather than
//! at it.
//!
//! For the pre-fix three-image render peak (3.808 GB on the same 74.65 MP frame)
//! and the rest of the before/after set, see `docs/progress/io.md`
//! (`## memory-preflight`).
//!
//! ## Determinism (scoped — read before touching this)
//!
//! The *image output* is untouched: nothing here perturbs a pixel, and the
//! estimate is a pure function of the input shape plus the run's parameters. The
//! *pass/fail decision* is likewise machine-independent, because the default
//! budget is a **fixed constant** ([`DEFAULT_MAX_MEMORY_BYTES`]) rather than a
//! fraction of detected RAM. The one environment-dependent piece is the **warning
//! tier**, which compares the estimate against detected physical RAM
//! ([`detect_total_ram`]): the same input can warn on a small machine and stay
//! quiet on a large one — and on `convert`/`roll`/`estimate`, where `--strict`
//! promotes warnings, can therefore *exit* differently. (`inspect` has no
//! `--strict`, so there the warning is report-only.) That is deliberate — a small
//! machine deserves the warning — but it means "same input + params ⇒ same exit"
//! holds only up to `--strict` plus the warn tier.

use crate::io::decode::ImageShape;
use crate::pipeline::film_base;
use crate::types::{NcError, OutDepth, Result};

use serde::Serialize;

/// Bytes per sample of the `f32` working representation every pixel buffer in the
/// pipeline uses (design-spec §4: 32-bit float linear working space throughout).
const F32_BYTES: u64 = 4;

/// Channels every *working* buffer carries, independent of the input file's
/// channel count: [`crate::types::LinearImage`]'s `rgb` is `w*h*3` by invariant
/// (enforced by its constructor) and `io::encode` writes RGB unconditionally. Only
/// the source-depth read buffer is sized from the file's own channel count.
const WORKING_CHANNELS: u64 = 3;

/// Bytes per pixel charged to the AVIF encode phase beyond the retained f32
/// rendition (`RunProfile::HdrAvif`).
///
/// Covers, in one figure: nc's three 10-bit Y'/Cb/Cr `u16` planes (6 B/px),
/// libaom's own `aom_img_alloc` frame in `AOM_IMG_FMT_I44416` including its
/// alignment border (~6 B/px), libaom's internal all-intra working set for 10-bit
/// 4:4:4, the emitted codestream, and the assembled container held in memory
/// before it is staged.
///
/// **Calibrated, not derived** — it is one lumped constant because libaom's
/// internal allocation is not something nc can enumerate per buffer, and splitting
/// it into named terms would imply a precision the model does not have.
///
/// Measured on two real HDRi scans (macOS/aarch64, release, `--film-base`
/// explicit so nothing is sampled), solving `measured = px·(28 + X) + fixed`
/// across the pair:
///
/// | Scan | Pixels | Measured peak RSS |
/// |---|---|---|
/// | Phoenix frame | 18.66 MP | 1,472,397,312 B |
/// | `samples/largest` | 74.65 MP | 5,865,947,136 B |
///
/// The two points give **78.47 B/px** total with only ~7.9 MB fixed — clean linear
/// scaling — so the actual staging above the 28 B/px retained term is 50.47 B/px.
/// Pinning 48 leaves `accounted` 3.4–3.8% *under* measured, which
/// [`ALLOWANCE_PERCENT`] then covers with room to spare (its own justification is a
/// 12.9% worst case). That is the same convention the `Convert` profile uses:
/// `accounted` enumerates buffers, the allowance covers allocator overhead. Do not
/// "fix" the 3.8% gap by raising this — double-counting it pushed the 18.66 MP
/// estimate to 1.43x measured, which rejects runs the machine could serve.
const AVIF_STAGING_BYTES_PER_PX: u64 = 48;

/// Default memory budget when `--max-memory` is not given: 6 GiB.
///
/// Deliberately a **fixed** constant, not a fraction of detected RAM, so the
/// pass/fail decision is machine-independent (see the determinism note above) —
/// the same scan either fits the budget everywhere or nowhere.
///
/// Derived from the worst *real* workload, not a round number: a `convert` on the
/// largest scan on hand (74.65 MP HDRi) with a full-frame `--base-region` — the
/// measure-once-reuse-`Dmin` workflow design-spec §8 recommends — accounts 50 B/px
/// = 3.73 GB and estimates **4.43 GB** with the allowance. A 4 GiB budget
/// (4.29 GB) would reject that run even though it measures 3.74 GB and completes
/// fine; 6 GiB admits it with headroom while still catching the multi-GiB runaway
/// the old 4 GiB *input* limit permitted unchecked. (Before the film-base phase
/// was modelled this constant was 4 GiB, justified against a 3.40 GB estimate that
/// omitted the region sample.) A machine that wants a tighter or looser ceiling
/// sets `--max-memory`; the rejection message says so, and the RAM-aware warn tier
/// is what protects a small machine from a budget this size.
pub const DEFAULT_MAX_MEMORY_BYTES: u64 = 6 * 1024 * 1024 * 1024;

/// Proportional part of the allowance added to the accounted buffers, in percent.
/// Set above the **12.9%** worst measured overhead (see the module doc's
/// calibration table) so the estimate keeps a real margin — 2.1 percentage points
/// — rather than tracking one machine's allocator exactly: the gate must err
/// toward rejecting, never toward an OOM.
const ALLOWANCE_PERCENT: u64 = 15;

/// Fixed part of the allowance: the binary, static data, lcms2 profiles and
/// transforms, and the `tiff` writer's buffers — costs that don't scale with the
/// frame. 128 MiB.
///
/// Unconditional, so it is also a **floor** on every estimate: no input, however
/// small, can be admitted under a budget of 128 MiB or less. The rejection message
/// says so when the accounted buffers are the smaller part (a smaller frame cannot
/// help there).
const ALLOWANCE_FIXED_BYTES: u64 = 128 * 1024 * 1024;

/// Fraction of detected physical RAM (percent) above which an *within-budget*
/// estimate still warns. A run that wants most of the machine's memory will fight
/// the page cache and swap even though it is nominally allowed.
const RAM_WARN_PERCENT: u64 = 70;

/// Which pipeline a preflight is sizing. The commands allocate very differently —
/// `inspect`/`estimate` stop after decode, so gating them on the full-pipeline
/// peak would reject inputs they could handle fine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunProfile {
    /// `convert` / `roll`: decode → film-base → render → encode.
    Convert {
        /// Output depth — a `u16` encode stages a whole extra quantize buffer,
        /// `f32` writes the working buffer verbatim.
        depth: OutDepth,
        /// Whether `--export-ir` will stage an IR plane for writing too.
        export_ir: bool,
    },
    /// `ultra-hdr-v1`: both display renditions and the full-resolution gain
    /// buffers coexist before the two JPEGs are packaged.
    UltraHdrV1 {
        /// Whether a u16 IR TIFF is staged before the primary JPEG.
        export_ir: bool,
    },
    /// `hdr-pq` / `hdr-hlg`: one display rendition, then the AVIF encoder's own
    /// planes and libaom's internal working set.
    ///
    /// Cheaper than [`UltraHdrV1`](Self::UltraHdrV1) in f32 buffers — there is a
    /// single rendition, not two plus a gain map — but it pays for a native
    /// encoder that the JPEG path does not.
    HdrAvif {
        /// Whether a u16 IR TIFF is staged before the primary AVIF.
        export_ir: bool,
    },
    /// `hdr-linear-tiff`: one display rendition written verbatim as 32-bit float.
    ///
    /// The **cheapest** display profile, and the only one that adds **no staging
    /// term at all**: f32 is written as-is (no quantization buffer) and the `tiff`
    /// writer streams strips through the staged `BufWriter` under `Predictor::None`
    /// (no in-memory container), so encode holds strictly less than render did.
    ///
    /// Its peak is therefore the **render** phase — but it is *not* alone in that:
    /// [`HdrCodedTiff`](Self::HdrCodedTiff) peaks at render too, and so does
    /// [`UltraHdrV1`](Self::UltraHdrV1), whose four simultaneous display buffers
    /// outweigh its encode set. Which phase peaks is **per profile**; read it off
    /// `which_phase_peaks_is_per_profile_and_measured_not_assumed` rather than any
    /// sentence, this one included.
    HdrLinearTiff {
        /// Whether an f32 IR TIFF is staged before the primary TIFF.
        export_ir: bool,
    },
    /// `hdr-pq-tiff` / `hdr-hlg-tiff`: one display rendition plus the u16 code-value
    /// buffer it is quantized into.
    ///
    /// Between [`HdrLinearTiff`](Self::HdrLinearTiff) and [`HdrAvif`](Self::HdrAvif):
    /// it pays for a quantization buffer the linear TIFF does not need, but not for
    /// a native codec's working set.
    HdrCodedTiff {
        /// Whether a u16 IR TIFF is staged before the primary TIFF.
        export_ir: bool,
    },
    /// `inspect` / `estimate`: decode, then sample — no render, no encode.
    DecodeOnly,
}

/// Which rectangles a run's film-base / reference sampling will gather into
/// per-channel `f32` vectors (`film_base::region_channels`, 12 B per sampled
/// pixel), so the film-base phase can be sized before anything is decoded.
///
/// The three fields are **independent facts, not alternatives** — one `nc
/// estimate` run can auto-detect a base *and* sample a `--d-max-region` — so this
/// is deliberately a struct rather than an enum. They are gathered (and dropped)
/// one rectangle at a time, so the phase peaks at the **largest** of them, which
/// is what [`SamplePlan::sampled_pixels`] returns.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SamplePlan {
    /// The `auto` film-base path runs, sampling the frame interior
    /// ([`film_base::auto_interior_pixels`]).
    pub auto_interior: bool,
    /// Largest explicitly-bounded rectangle sampled, in pixels (`--base-region`,
    /// `--d-max-region`, `estimate --grid`'s rectangle). `0` for none.
    ///
    /// For `--grid` this is one *cell* of the rectangle
    /// ([`film_base::grid_cell_pixels`]), not the whole rectangle: the five cells
    /// are each ~1/16 of it and are gathered one at a time, so the rectangle would
    /// over-count ~16x.
    pub rect_pixels: u64,
    /// `estimate --grid` runs over the **whole frame** (no `--base-region`) — the
    /// one case whose rectangle isn't known until the frame is probed. Sized at
    /// resolve time as one grid *cell* ([`film_base::grid_cell_pixels`]), since the
    /// five cells are gathered and dropped one at a time.
    pub whole_frame_grid: bool,
}

impl SamplePlan {
    /// Nothing is sampled — an explicit `--film-base`, which reads no pixels.
    pub fn none() -> Self {
        Self::default()
    }

    /// The `auto` film-base path (its interior sample).
    pub fn auto() -> Self {
        Self {
            auto_interior: true,
            ..Self::default()
        }
    }

    /// One explicitly-bounded rectangle of `pixels` px.
    pub fn rect(pixels: u64) -> Self {
        Self {
            rect_pixels: pixels,
            ..Self::default()
        }
    }

    /// Add another explicitly-bounded rectangle (keeping the larger).
    pub fn with_rect(self, pixels: u64) -> Self {
        Self {
            rect_pixels: self.rect_pixels.max(pixels),
            ..self
        }
    }

    /// `estimate --grid` over the whole frame (no `--base-region`).
    pub fn with_whole_frame_grid(self) -> Self {
        Self {
            whole_frame_grid: true,
            ..self
        }
    }

    /// Pixels in the largest single rectangle this plan gathers, for `shape`.
    ///
    /// **Clamped to the frame.** `rect_pixels` comes from a user-supplied
    /// `--base-region` / `--d-max-region`, which this stage has no business
    /// validating: `film_base::region_channels` is the authority on whether a
    /// rectangle fits the image, and it rejects an out-of-bounds one as a *usage*
    /// error (exit 2). Estimating the raw `w*h` instead would let a typo'd
    /// `--base-region 0,0,999999,999999` be reported as a 12852 GiB **resource**
    /// rejection (exit 6) — or, near `u32::MAX`, as an overflow error blaming the
    /// image's own dimensions — burying the real mistake behind the wrong exit
    /// code. No legal sample can exceed the frame, so clamping cannot
    /// under-estimate a run that is actually going to happen; it just keeps the
    /// gate quiet about rectangles that were never valid. Same probe-is-permissive
    /// / decode-is-the-authority split this module already uses for color types.
    fn sampled_pixels(&self, shape: &ImageShape) -> u64 {
        let auto = if self.auto_interior {
            film_base::auto_interior_pixels(shape.width, shape.height)
        } else {
            0
        };
        let frame = if self.whole_frame_grid {
            film_base::grid_cell_pixels(shape.width, shape.height)
        } else {
            0
        };
        let frame_pixels = shape.width as u64 * shape.height as u64;
        auto.max(frame).max(self.rect_pixels).min(frame_pixels)
    }
}

/// The estimated peak allocation of a run, with the per-phase breakdown that
/// produced it. Serialized into the JSON report so the number the gate decided on
/// is auditable (and comparable against a measured peak RSS).
///
/// Which terms generalize beyond the shipped path: `decode_bytes`'s source read
/// buffer is the only term sized from the *file's* channel count and bit depth;
/// every other term is pinned to the working representation the pipeline actually
/// uses — 3-channel `f32` in, 3-channel `u16`/`f32` out ([`WORKING_CHANNELS`]) —
/// because that is an invariant of `LinearImage`/`io::encode`, not a property of
/// the input. A future non-3-channel input (a `Gray(16)` B&W scan) changes only
/// the read-buffer term.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct PeakEstimate {
    /// What the gate compares against the budget: [`accounted_bytes`] plus the
    /// allowance for allocator slack and fixed costs.
    ///
    /// [`accounted_bytes`]: Self::accounted_bytes
    pub estimated_peak_bytes: u64,
    /// The peak of the named full-frame buffers alone — no allowance.
    pub accounted_bytes: u64,
    /// Named-buffer total while decoding (`f32` image + the transient source-depth
    /// read buffer).
    pub decode_bytes: u64,
    /// Named-buffer total during film-base estimation: the decoded image plus the
    /// three `f32` channel vectors of the largest sampled rectangle ([`SamplePlan`]).
    /// Equals the decoded image alone when nothing is sampled (an explicit
    /// `--film-base`).
    pub film_base_bytes: u64,
    /// Named-buffer total during the render (decoded image + positive). Zero for
    /// [`RunProfile::DecodeOnly`], which never renders.
    pub render_bytes: u64,
    /// Named-buffer total during encode (decoded + rendered + quantize buffers).
    /// Zero for [`RunProfile::DecodeOnly`], which never encodes.
    pub encode_bytes: u64,
}

/// Where the active budget came from — reported so a rejection is traceable to
/// the flag or to the built-in default. The wire form of [`Budget`]'s
/// discriminant.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum BudgetSource {
    /// [`DEFAULT_MAX_MEMORY_BYTES`].
    Default,
    /// An explicit `--max-memory`.
    Flag,
}

/// The resolved memory budget for a run.
///
/// One enum rather than a `{ bytes, source }` pair: the pair could represent a
/// `Default` budget with a non-default byte count (and the rejection message
/// branches on the source while the comparison uses the bytes, so the two could
/// disagree about what rejected the run). This also keeps
/// [`DEFAULT_MAX_MEMORY_BYTES`] in exactly one place.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Budget {
    /// No `--max-memory`: the fixed [`DEFAULT_MAX_MEMORY_BYTES`].
    Default,
    /// An explicit `--max-memory` value, in bytes.
    Explicit(u64),
}

impl Budget {
    /// Resolve the budget from an optional `--max-memory` value.
    pub fn resolve(flag: Option<u64>) -> Self {
        match flag {
            Some(bytes) => Self::Explicit(bytes),
            None => Self::Default,
        }
    }

    /// The ceiling in bytes.
    pub fn bytes(self) -> u64 {
        match self {
            Self::Default => DEFAULT_MAX_MEMORY_BYTES,
            Self::Explicit(bytes) => bytes,
        }
    }

    /// The reported provenance of this budget.
    fn source(self) -> BudgetSource {
        match self {
            Self::Default => BudgetSource::Default,
            Self::Explicit(_) => BudgetSource::Flag,
        }
    }
}

/// The preflight's verdict for a run that is *allowed to proceed*. Over-budget
/// runs never produce a verdict — they fail with [`NcError::Resource`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    /// Within budget, and not a large fraction of the machine's RAM.
    Ok,
    /// Within budget, but above [`RAM_WARN_PERCENT`] of detected physical RAM.
    Warn,
}

/// The preflight decision for the JSON report: the estimate, the budget it was
/// checked against, the verdict, and the detected RAM the warn tier used.
///
/// `budget_bytes`/`budget_source` are the two flat keys the documented report
/// contract carries; both are derived from the [`Budget`] enum at construction.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct MemoryReport {
    #[serde(flatten)]
    pub estimate: PeakEstimate,
    pub budget_bytes: u64,
    pub budget_source: BudgetSource,
    pub decision: Verdict,
    /// Detected physical RAM, when the platform could report it. Absent rather
    /// than guessed — the warn tier simply doesn't fire without it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detected_total_ram_bytes: Option<u64>,
}

/// Run the preflight: size the run from its input shape, compare against the
/// budget, and either reject it loudly or return the report (with the warn-tier
/// verdict) for the orchestrator to record.
///
/// `total_ram` is injected rather than detected here so the whole decision stays
/// a pure function — the orchestrator passes [`detect_total_ram`].
///
/// **Must be called before the input is decoded.** Its whole purpose is to run
/// while nothing large is allocated yet; sizing a run from an already-decoded
/// image would OOM on exactly the inputs this gate exists to reject.
pub fn preflight(
    shape: &ImageShape,
    profile: RunProfile,
    sampling: SamplePlan,
    budget: Budget,
    total_ram: Option<u64>,
) -> Result<MemoryReport> {
    let estimate = estimate_peak(shape, profile, sampling)?;
    if estimate.estimated_peak_bytes > budget.bytes() {
        // When the frame's own buffers are the smaller part of the estimate, the
        // fixed allowance floor is what the budget can't fit — telling the user to
        // convert a smaller frame there would be impossible advice.
        let advice = if estimate.accounted_bytes < ALLOWANCE_FIXED_BYTES {
            format!(
                "raise it with --max-memory: every run costs a fixed {} on top of its \
                 image buffers (the binary, lcms2, the tiff writer), so no smaller \
                 frame fits this budget either",
                human_bytes(ALLOWANCE_FIXED_BYTES)
            )
        } else {
            "raise it with --max-memory or convert a smaller frame".to_string()
        };
        // Exact byte counts alongside the rounded forms: `human_bytes` keeps one
        // decimal, so a near-miss would otherwise print the self-contradictory
        // "estimated peak 1.0 GiB exceeds the default budget of 1.0 GiB" — and the
        // rejection path emits no JSON report, so this message is the only channel
        // an agent has for choosing a new `--max-memory`.
        return Err(NcError::Resource(format!(
            "estimated peak memory {} ({} bytes) exceeds the {} budget of {} ({} bytes) \
             ({}x{} px, {} channels, {}-bit{}); {advice}",
            human_bytes(estimate.estimated_peak_bytes),
            estimate.estimated_peak_bytes,
            match budget.source() {
                BudgetSource::Flag => "--max-memory",
                BudgetSource::Default => "default",
            },
            human_bytes(budget.bytes()),
            budget.bytes(),
            shape.width,
            shape.height,
            shape.channels,
            shape.bits_per_sample,
            if shape.ir_present { " + IR plane" } else { "" },
        )));
    }
    let decision = match total_ram {
        Some(ram) if estimate.estimated_peak_bytes > ram / 100 * RAM_WARN_PERCENT => Verdict::Warn,
        _ => Verdict::Ok,
    };
    Ok(MemoryReport {
        estimate,
        budget_bytes: budget.bytes(),
        budget_source: budget.source(),
        decision,
        detected_total_ram_bytes: total_ram,
    })
}

/// The `--strict`-promotable warning text for a [`Verdict::Warn`] report, or
/// `None` when there is nothing to warn about. Kept next to the model so the
/// threshold and its wording can't drift apart.
///
/// The `Warn` verdict is only reachable with a detected RAM figure, and the guard
/// lives *here* (rather than at the call site) so a second caller cannot forget it
/// and render "more than 70% of this machine's unknown of RAM".
pub fn warn_message(report: &MemoryReport) -> Option<String> {
    let (Verdict::Warn, Some(ram)) = (report.decision, report.detected_total_ram_bytes) else {
        return None;
    };
    Some(format!(
        "estimated peak memory {} is more than {RAM_WARN_PERCENT}% of this machine's {} of RAM; \
         the run may swap or be killed by the OS",
        human_bytes(report.estimate.estimated_peak_bytes),
        human_bytes(ram),
    ))
}

/// Size a run's peak allocation from its input shape — the single source of truth
/// for the preflight, the report, and (later) `io/streaming-tiled-io`'s go/no-go.
/// Pure; see the module doc for the model and its calibration.
///
/// Arithmetic is checked throughout: a corrupt header advertising absurd
/// dimensions must surface as a loud error here (before anything is allocated),
/// never wrap into a small, permissive estimate.
pub fn estimate_peak(
    shape: &ImageShape,
    profile: RunProfile,
    sampling: SamplePlan,
) -> Result<PeakEstimate> {
    let overflow = || {
        NcError::Other(format!(
            "image dimensions {}x{} overflow the memory sizing model",
            shape.width, shape.height
        ))
    };
    let mul = |a: u64, b: u64| a.checked_mul(b).ok_or_else(overflow);
    let sum = |a: u64, b: u64| a.checked_add(b).ok_or_else(overflow);

    let pixels = mul(shape.width as u64, shape.height as u64)?;
    let src_sample = shape.bits_per_sample.div_ceil(8) as u64;

    // One interleaved f32 RGB working buffer (always 3-channel), and the
    // source-depth read buffer it is built from — the one term that follows the
    // *file's* channel count (both live at once inside
    // `io::decode::read_plane_u16`).
    let rgb32 = mul(pixels, mul(WORKING_CHANNELS, F32_BYTES)?)?;
    let rgb_src = mul(pixels, mul(shape.channels as u64, src_sample)?)?;
    // The IR plane, same two forms (single-channel). `ir_src` uses IFD0's bit
    // depth, not the IR page's own — harmless only because `decode` requires both
    // pages to be 16-bit (and rejects the file otherwise).
    let (ir32, ir_src) = if shape.ir_present {
        (mul(pixels, F32_BYTES)?, mul(pixels, src_sample)?)
    } else {
        (0, 0)
    };

    // One fully-decoded image resident in f32: RGB + the carried IR plane.
    let image = sum(rgb32, ir32)?;

    // Decode, in two moments: reading RGB (f32 RGB buffer + the u16 read buffer it
    // is built from — the IR plane does not exist yet), then reading IR (f32 RGB
    // buffer + the IR read buffer + the f32 IR plane built from it). The RGB read
    // buffer is freed before the IR one is allocated, so the peak is the larger
    // moment — with `rgb32` (not the whole decoded image) as the common base.
    let decode_bytes = sum(rgb32, rgb_src.max(sum(ir_src, ir32)?))?;

    // The three f32 channel vectors `film_base::region_channels` materializes for
    // the largest rectangle this run samples. Rectangles are gathered one at a
    // time, so the largest sets the term (measured: sampling two full-frame
    // rectangles peaks identically to sampling one).
    let sampled = mul(
        sampling.sampled_pixels(shape),
        mul(WORKING_CHANNELS, F32_BYTES)?,
    )?;

    // Film base (stage 2): the decoded image plus that sample.
    let film_base_bytes = sum(image, sampled)?;

    let (render_bytes, encode_bytes) = match profile {
        RunProfile::DecodeOnly => (0, 0),
        RunProfile::Convert { depth, export_ir } => {
            // Render: the decoded image (held for `--export-ir` / the report) plus
            // the density→positive buffer and its cloned IR plane. The output
            // color transform is in place, so there is no third image.
            let render = mul(image, 2)?;
            // Encode: decoded + rendered, plus the u16 staging buffers (none at
            // f32 depth, which writes the working buffer verbatim). The output is
            // RGB whatever the input was, so these follow `WORKING_CHANNELS`.
            let quantize = match depth {
                OutDepth::U16 => {
                    let rgb = mul(pixels, mul(WORKING_CHANNELS, 2)?)?;
                    if export_ir && shape.ir_present {
                        sum(rgb, mul(pixels, 2)?)?
                    } else {
                        rgb
                    }
                }
                OutDepth::F32 => 0,
            };
            // `sampled` is added to both later phases, not competed against them:
            // the film-base vectors are freed before the render, but freed pages
            // stay resident, so they still occupy the process at every later peak.
            // This is the same retention rule the encode buffers are summed under —
            // one rule, applied consistently, rather than two ad-hoc choices.
            // Measured: a full-frame `--base-region` convert peaks at 50 B/px
            // (32 render + 6 quantize + 12 sampled), and treating the sample as a
            // *competing* phase instead under-estimated that run by 10%.
            (
                sum(render, sampled)?,
                sum(sum(mul(image, 2)?, quantize)?, sampled)?,
            )
        }
        RunProfile::UltraHdrV1 { export_ir } => {
            // Decoded + shared adjusted ACEScg, then SDR, BT.2020 HDR,
            // common-P3 HDR, and full-resolution ratios (four f32 RGB buffers).
            let display_buffers = mul(pixels, 4 * WORKING_CHANNELS * F32_BYTES)?;
            let render = sum(mul(image, 2)?, display_buffers)?;

            // Decoded + retained SDR/common-HDR/gain f32 buffers, plus u8 base,
            // half-resolution u8 gain, conservative raw-sized JPEG/package
            // buffers, and the optional u16 IR staging plane.
            let retained = sum(image, mul(pixels, 3 * WORKING_CHANNELS * F32_BYTES)?)?;
            // 20 B/px conservatively covers u8 base/map inputs, both compressed
            // JPEGs, libultrahdr's owned input copies and destination, the Rust
            // copy made before the native encoder is released, and — for
            // `Dialects::LegacyPlusIso` — `insert_baseline_iso_segment`'s second
            // full copy of the packaged JPEG, which exists alongside the first.
            // Nothing is under-approved today: that dialect has no CLI caller, and
            // the 20 B/px is deliberately loose against a package far smaller than
            // the raw frame. Whoever wires the dialect up must still re-check this
            // term, and `iso::encode_iso_gain_map` would add roughly 24 B/px of
            // full-frame buffers (f32 per-channel normalization plus its
            // deinterleaved planes) if it ever reaches a live path. Per CLAUDE.md,
            // nothing tests this model against the code.
            let byte_staging = mul(pixels, 20)?;
            let ir_export = if export_ir && shape.ir_present {
                mul(pixels, 2)?
            } else {
                0
            };
            (
                sum(render, sampled)?,
                sum(sum(sum(retained, byte_staging)?, ir_export)?, sampled)?,
            )
        }
        RunProfile::HdrAvif { export_ir } => {
            // Render: decoded (held for `--export-ir` / the report) plus the shared
            // adjusted ACEScg source, plus the BT.2020 rendition allocated beside
            // it. `encode_transfer` mutates that rendition in place, so the PQ/HLG
            // transfer costs nothing further here.
            //
            // The shared source is `image`-shaped, not RGB-only: reconstruction
            // carries the IR plane through `AcesCgImage` into
            // `AdjustedAcesCgImage`, so on an HDRi input it is 16 B/px like the
            // decoded image rather than 12. Counting it as one more `image` is what
            // `UltraHdrV1` above does for the same reason; modelling it as a bare
            // RGB buffer under-reported this phase by 4 B/px on every IR input.
            let rendition = mul(pixels, WORKING_CHANNELS * F32_BYTES)?;
            let render = sum(mul(image, 2)?, rendition)?;

            // Encode: decoded + the retained f32 rendition, plus the AVIF encoder's
            // own buffers. `AVIF_STAGING_BYTES_PER_PX` covers everything native.
            let retained = sum(image, mul(pixels, WORKING_CHANNELS * F32_BYTES)?)?;
            let avif_staging = mul(pixels, AVIF_STAGING_BYTES_PER_PX)?;
            let ir_export = if export_ir && shape.ir_present {
                mul(pixels, 2)?
            } else {
                0
            };
            (
                sum(render, sampled)?,
                sum(sum(sum(retained, avif_staging)?, ir_export)?, sampled)?,
            )
        }
        RunProfile::HdrCodedTiff { export_ir } => {
            // Render: identical to `HdrAvif` and `HdrLinearTiff` — they share
            // `render_linear`, and `encode_transfer` mutates the rendition in place.
            let rendition = mul(pixels, WORKING_CHANNELS * F32_BYTES)?;
            let render = sum(mul(image, 2)?, rendition)?;

            // Encode: decoded + the retained f32 rendition + the u16 code buffer it
            // is quantized into (3 channels x 2 B). Like `HdrLinearTiff` there is no
            // container staging — `tiff` streams strips — so the quantize buffer is
            // the only term the linear profile does not also pay.
            let quantize = mul(pixels, mul(WORKING_CHANNELS, 2)?)?;
            let retained = sum(sum(image, rendition)?, quantize)?;
            let ir_export = if export_ir && shape.ir_present {
                // u16 IR, matching this preset's resolved `OutDepth::U16`.
                mul(pixels, 2)?
            } else {
                0
            };
            (
                sum(render, sampled)?,
                sum(sum(retained, ir_export)?, sampled)?,
            )
        }
        // `export_ir` is deliberately not consulted — see the comment on
        // `ir_export` below. The field stays on the variant because the CLI sets it
        // uniformly for every profile and its *shape* should not depend on whether
        // this particular model happens to charge for it.
        RunProfile::HdrLinearTiff { export_ir: _ } => {
            // Render: identical to `HdrAvif` — decoded (held for `--export-ir` /
            // the report) plus the `image`-shaped shared adjusted ACEScg source
            // (16 B/px on an HDRi input, because reconstruction carries the IR
            // plane through `AcesCgImage`), plus the BT.2020 rendition beside it.
            // No `encode_transfer` runs on this path: the samples stay linear.
            let rendition = mul(pixels, WORKING_CHANNELS * F32_BYTES)?;
            let render = sum(mul(image, 2)?, rendition)?;

            // Encode: decoded + the rendition it writes, and **nothing else of
            // consequence**. This is the one display path with no staging term:
            //   * f32 is written verbatim, so there is no quantization buffer (the
            //     same reason `Convert`'s `OutDepth::F32` arm contributes 0); and
            //   * the file is not assembled in memory the way AVIF and the gain-map
            //     JPEGs are — `tiff`'s `write_data` writes one strip at a time
            //     straight into the staged `BufWriter` under the default
            //     `Predictor::None`, with no full-frame intermediate.
            //
            // So encode is strictly cheaper than render here, and `accounted_bytes`
            // legitimately lands on the render phase. If a future lossless TIFF
            // *compression* option is added, its codec buffers become a real term
            // and this comment stops being true — add it here rather than trusting
            // the allowance to absorb it. Per CLAUDE.md, nothing tests this model
            // against the code.
            // **`--export-ir` costs nothing here**, which is not an omission. This
            // preset resolves `OutDepth::F32`, and `export_ir_to_writer`'s f32 arms
            // hand `image.ir`'s existing `&[f32]` straight to `encode_planar` —
            // there is no `quantize_u16` and so no staging `Vec` to charge for. That
            // is exactly why `Convert`'s `OutDepth::F32` arm above contributes `0`
            // even when `export_ir` is set. Charging the u16 path's 2 B/px, or the
            // plane's own 4 B/px, would over-state the peak and reject runs the
            // machine could serve.
            let retained = sum(image, rendition)?;
            (sum(render, sampled)?, sum(retained, sampled)?)
        }
    };

    let accounted_bytes = decode_bytes
        .max(film_base_bytes)
        .max(render_bytes)
        .max(encode_bytes);
    let allowance = sum(
        accounted_bytes / 100 * ALLOWANCE_PERCENT,
        ALLOWANCE_FIXED_BYTES,
    )?;
    Ok(PeakEstimate {
        estimated_peak_bytes: sum(accounted_bytes, allowance)?,
        accounted_bytes,
        decode_bytes,
        film_base_bytes,
        render_bytes,
        encode_bytes,
    })
}

/// Physical RAM in bytes, when the platform can report it cheaply — used only by
/// the warn tier, never by the (deliberately fixed) default budget.
///
/// Fail-soft by design: an unsupported platform, a missing `/proc`, a failed
/// syscall, or an unparsable value returns `None`, which disables the warning
/// rather than failing a run that is within budget.
pub fn detect_total_ram() -> Option<u64> {
    // Bound to a `let` per platform rather than left as three cfg'd tail
    // expressions: with tails, adding any statement below would turn the surviving
    // block into a `#[must_use]` `Option` statement — an `unused_must_use` error
    // under `-D warnings` that could go red on one target while staying green on
    // the other. This shape cannot spring that trap.
    #[cfg(target_os = "linux")]
    let detected = linux_total_ram();
    #[cfg(target_os = "macos")]
    let detected = macos_total_ram();
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let detected = None;
    detected
}

/// Installed RAM from the `hw.memsize` sysctl — the documented source on Darwin,
/// which has no `/proc` equivalent. Read-only, allocation-free, fail-soft.
#[cfg(target_os = "macos")]
fn macos_total_ram() -> Option<u64> {
    let mut size: u64 = 0;
    let mut len = std::mem::size_of::<u64>();
    // SAFETY: `sysctlbyname` writes at most `len` bytes through `oldp` — a `u64`
    // this frame owns, correctly aligned and valid for the whole call — and reads
    // `name` as a NUL-terminated string (a `c"…"` literal, so NUL-terminated by
    // construction and `'static`). `newp`/`newlen` are the documented null/0 for a
    // read. Nothing is retained past the call and no unwinding crosses it.
    //
    // Note what this does NOT assume: `sysctl(3)` may copy out a partial value and
    // still return -1 (ENOMEM), so the result is trusted only when `rc == 0` *and*
    // the kernel reports having written exactly the 8 bytes a `uint64_t` needs.
    // `size` is pre-zeroed, so even a short copyout is a wrong value, never UB.
    let rc = unsafe {
        libc::sysctlbyname(
            c"hw.memsize".as_ptr(),
            (&raw mut size).cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    (rc == 0 && len == std::mem::size_of::<u64>() && size > 0).then_some(size)
}

/// Total RAM available to this process on Linux: `/proc/meminfo`'s `MemTotal`
/// capped by any cgroup memory limit.
///
/// `MemTotal` reports the **host's** RAM and ignores cgroup limits, so inside a
/// container capped well below the host the warn tier would compare against a
/// number the process can never reach — exactly where an OOM kill is likeliest.
/// Read the cgroup ceiling too (v2 `memory.max`, else v1
/// `memory/memory.limit_in_bytes`) and take the lower of the two. Fail-soft
/// throughout: any missing file or unparsable value degrades to the other source,
/// or to `None`, never to an error.
///
/// Deliberately a plain function rather than inline `#[cfg]` code so it is
/// type-checked on every target (the parse helpers are unit-tested everywhere too);
/// only Linux ever calls it.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn linux_total_ram() -> Option<u64> {
    let mem_total = std::fs::read_to_string("/proc/meminfo")
        .ok()
        .as_deref()
        .and_then(parse_meminfo_total);
    let cgroup = cgroup_limit_paths()
        .into_iter()
        .filter_map(|p| std::fs::read_to_string(p).ok())
        .find_map(|text| parse_cgroup_limit(&text));
    match (mem_total, cgroup) {
        (Some(total), Some(limit)) => Some(total.min(limit)),
        (total, None) => total,
        (None, limit) => limit,
    }
}

/// Candidate cgroup memory-limit files, most specific first.
///
/// The **process's own** cgroup is what bounds it, and under Kubernetes or a
/// systemd service that cgroup is nested — the limit lives at the path named in
/// `/proc/self/cgroup`, not at the root. Reading only the root files would report
/// `max` (or a permissive ancestor limit) in exactly the containerized setups the
/// cgroup check exists for, leaving the warn tier silent where OOM is likeliest.
/// So: derive the process's own v2 and v1 paths first, then walk *up* to the root
/// as a fallback, since a nested cgroup may itself declare no limit while an
/// ancestor does.
///
/// Not `cfg`-gated (like the parsers beside it): only its *caller* is Linux-only,
/// so the path logic still compiles and unit-tests on every target — this machine
/// can build only `aarch64-apple-darwin`, and Linux-only code that nothing here
/// compiles is code that first fails in CI.
fn cgroup_limit_paths() -> Vec<String> {
    let mut paths = Vec::new();
    if let Ok(text) = std::fs::read_to_string("/proc/self/cgroup") {
        let (v2, v1) = parse_self_cgroup(&text);
        // v2: a single unified hierarchy line, `0::<path>`.
        if let Some(rel) = v2 {
            for anc in ancestors_of(&rel) {
                paths.push(format!("/sys/fs/cgroup{anc}/memory.max"));
            }
        }
        // v1: the `memory` controller's own line.
        if let Some(rel) = v1 {
            for anc in ancestors_of(&rel) {
                paths.push(format!("/sys/fs/cgroup/memory{anc}/memory.limit_in_bytes"));
            }
        }
    }
    // Root fallbacks, for a non-nested cgroup or an unreadable /proc/self/cgroup.
    paths.push("/sys/fs/cgroup/memory.max".to_string());
    paths.push("/sys/fs/cgroup/memory/memory.limit_in_bytes".to_string());
    paths
}

/// The v2 and v1-`memory` relative cgroup paths from a `/proc/self/cgroup` body.
/// Lines are `hierarchy-ID:controller-list:path`; v2 is the `0::` line, and v1's
/// memory controller is whichever line lists `memory` among its controllers. A
/// root path (`/`) yields `None` — there is nothing nested to resolve.
fn parse_self_cgroup(text: &str) -> (Option<String>, Option<String>) {
    let (mut v2, mut v1) = (None, None);
    for line in text.lines() {
        let mut parts = line.splitn(3, ':');
        let (Some(id), Some(controllers), Some(path)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        if path == "/" || path.is_empty() {
            continue;
        }
        if id == "0" && controllers.is_empty() {
            v2 = Some(path.to_string());
        } else if controllers.split(',').any(|c| c == "memory") {
            v1 = Some(path.to_string());
        }
    }
    (v2, v1)
}

/// A cgroup path and each of its ancestors, most specific first, ending at the
/// root (`""`). `/kubepods/podXYZ` yields `["/kubepods/podXYZ", "/kubepods", ""]`.
fn ancestors_of(path: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = path.trim_end_matches('/');
    while !cur.is_empty() {
        out.push(cur.to_string());
        cur = match cur.rfind('/') {
            Some(0) | None => "",
            Some(i) => &cur[..i],
        };
    }
    out.push(String::new());
    out
}

/// Bytes of `MemTotal` (reported in kB) from a `/proc/meminfo` body.
fn parse_meminfo_total(text: &str) -> Option<u64> {
    let line = text.lines().find(|l| l.starts_with("MemTotal:"))?;
    let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    kb.checked_mul(1024)
}

/// Bytes of a cgroup memory-limit file body, or `None` when it declares no limit:
/// cgroup v2 writes the literal `max`, and v1 writes an enormous sentinel
/// (`u64::MAX` rounded down to a page multiple) — treating either as a real
/// ceiling would make the warn tier nonsense.
fn parse_cgroup_limit(text: &str) -> Option<u64> {
    let t = text.trim();
    if t == "max" {
        return None;
    }
    let bytes: u64 = t.parse().ok()?;
    // Anything at exbibyte scale is the "unlimited" sentinel, not a limit.
    (bytes > 0 && bytes < 1 << 60).then_some(bytes)
}

/// Format a byte count for a human-facing message: GiB/MiB/KiB with one decimal.
/// Report fields carry the exact byte counts, so this is presentation only.
fn human_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    let b = bytes as f64;
    if b >= KIB * KIB * KIB {
        format!("{:.1} GiB", b / (KIB * KIB * KIB))
    } else if b >= KIB * KIB {
        format!("{:.1} MiB", b / (KIB * KIB))
    } else if b >= KIB {
        format!("{:.1} KiB", b / KIB)
    } else {
        format!("{bytes} B")
    }
}

/// Parse a `--max-memory` value: a byte count with an optional unit suffix
/// (`4GiB`, `4G`, `4096MB`, `512M`, `2000000000`). Binary and decimal suffixes are
/// both accepted and mean what they say (`GiB` = 1024³, `GB` = 1000³).
///
/// Rejects zero and unparsable values loudly as usage errors (exit 2) — a
/// silently-ignored budget flag would leave the run unguarded, and a zero budget
/// can never admit any conversion.
pub fn parse_max_memory(s: &str) -> Result<u64> {
    let t = s.trim();
    let digits = t.find(|c: char| !c.is_ascii_digit()).unwrap_or(t.len());
    let (number, unit) = t.split_at(digits);
    let value: u64 = number.parse().map_err(|_| {
        NcError::Usage(format!(
            "invalid --max-memory {s:?}: expected a byte count with an optional \
             unit (e.g. 4GiB, 512MB, 2000000000)"
        ))
    })?;
    let multiplier: u64 = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "k" | "kib" => 1024,
        "kb" => 1000,
        "m" | "mib" => 1024 * 1024,
        "mb" => 1000 * 1000,
        "g" | "gib" => 1024 * 1024 * 1024,
        "gb" => 1000 * 1000 * 1000,
        "t" | "tib" => 1024_u64.pow(4),
        "tb" => 1000_u64.pow(4),
        other => {
            return Err(NcError::Usage(format!(
                "invalid --max-memory unit {other:?} in {s:?}: expected one of \
                 B, KiB/KB, MiB/MB, GiB/GB, TiB/TB"
            )));
        }
    };
    let bytes = value.checked_mul(multiplier).ok_or_else(|| {
        NcError::Usage(format!("--max-memory {s:?} overflows a 64-bit byte count"))
    })?;
    if bytes == 0 {
        return Err(NcError::Usage(
            "--max-memory must be greater than zero".into(),
        ));
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shipped input shape: full-resolution 16-bit RGB, optional IR plane.
    fn shape(width: u32, height: u32, ir: bool) -> ImageShape {
        ImageShape::new(width, height, 3, 16, ir).expect("valid shape")
    }

    fn convert_u16() -> RunProfile {
        RunProfile::Convert {
            depth: OutDepth::U16,
            export_ir: false,
        }
    }

    #[test]
    fn ultra_hdr_profile_counts_the_overlapping_display_and_gain_buffers() {
        let no_ir = estimate_peak(
            &shape(10, 10, false),
            RunProfile::UltraHdrV1 { export_ir: false },
            SamplePlan::none(),
        )
        .unwrap();
        assert_eq!(no_ir.render_bytes, 7_200);
        assert_eq!(no_ir.encode_bytes, 6_800);

        let with_ir_no_export = estimate_peak(
            &shape(10, 10, true),
            RunProfile::UltraHdrV1 { export_ir: false },
            SamplePlan::none(),
        )
        .unwrap();
        let with_ir = estimate_peak(
            &shape(10, 10, true),
            RunProfile::UltraHdrV1 { export_ir: true },
            SamplePlan::none(),
        )
        .unwrap();
        assert_eq!(with_ir.render_bytes, 8_000);
        assert_eq!(with_ir.encode_bytes, 7_400);
        assert_eq!(with_ir.encode_bytes - with_ir_no_export.encode_bytes, 200);
        // No separate Ultra HDR + IR-export RSS run has been recorded. Until it
        // is, the optional term remains the same conservative 2 B/px u16 plane
        // staging used by the TIFF export path; this assertion prevents it from
        // silently growing into an invented calibration claim.
    }

    #[test]
    fn hdr_avif_profile_counts_one_rendition_plus_the_codec_staging() {
        // 10x10, no IR: the post-decode `image` term is 12 B/px (f32 RGB), so
        // render = 12 + 24 (shared ACEScg + the BT.2020 rendition beside it) = 36
        // B/px; encode = 12 + 12 (retained rendition) + 48 (AVIF staging) = 72.
        let no_ir = estimate_peak(
            &shape(10, 10, false),
            RunProfile::HdrAvif { export_ir: false },
            SamplePlan::none(),
        )
        .unwrap();
        assert_eq!(no_ir.render_bytes, 3_600);
        assert_eq!(no_ir.encode_bytes, 7_200);
        // Encode is the peak phase, as it is for every convert-shaped profile.
        assert_eq!(no_ir.accounted_bytes, no_ir.encode_bytes);

        // A carried IR plane widens the post-decode `image` term to 16 B/px (the
        // f32 IR plane rides along), and only an actual export adds the 2 B/px u16
        // staging plane on top. Render pays for that plane *twice* — once in the
        // decoded image, once in the shared ACEScg source that carries it through —
        // so it is 2*16 + 12 = 44 B/px, not 40.
        let with_ir_no_export = estimate_peak(
            &shape(10, 10, true),
            RunProfile::HdrAvif { export_ir: false },
            SamplePlan::none(),
        )
        .unwrap();
        let with_ir = estimate_peak(
            &shape(10, 10, true),
            RunProfile::HdrAvif { export_ir: true },
            SamplePlan::none(),
        )
        .unwrap();
        assert_eq!(with_ir.encode_bytes - with_ir_no_export.encode_bytes, 200);
        assert_eq!(with_ir_no_export.render_bytes, 4_400);
        // The IR plane is counted in *both* live copies at render, so an HDRi
        // input costs 8 B/px more there than an IR-free one, not 4.
        assert_eq!(with_ir_no_export.render_bytes - no_ir.render_bytes, 800);

        // Cheaper than the gain-map path in f32 buffers (one rendition, no gain
        // map) but more expensive overall, because it pays for a native encoder.
        let gain_map = estimate_peak(
            &shape(10, 10, false),
            RunProfile::UltraHdrV1 { export_ir: false },
            SamplePlan::none(),
        )
        .unwrap();
        assert!(no_ir.render_bytes < gain_map.render_bytes);
        assert!(no_ir.encode_bytes > gain_map.encode_bytes);
    }

    #[test]
    fn hdr_linear_tiff_profile_peaks_at_render_with_no_staging_term() {
        // 10x10, no IR: render = 12 (decoded) + 24 (shared ACEScg + rendition) = 36
        // B/px — identical to `HdrAvif`, since the two share `render_linear`. Encode
        // is 12 + 12 = 24: f32 is written verbatim (no quantize buffer) and the
        // `tiff` writer streams strips (no in-memory container), so there is nothing
        // else to count.
        let no_ir = estimate_peak(
            &shape(10, 10, false),
            RunProfile::HdrLinearTiff { export_ir: false },
            SamplePlan::none(),
        )
        .unwrap();
        assert_eq!(no_ir.render_bytes, 3_600);
        assert_eq!(no_ir.encode_bytes, 2_400);

        // **The distinguishing property is the absent staging term, not the peak
        // phase.** No staging *is* unique to this profile. Peaking at render is not:
        // `HdrCodedTiff` and `UltraHdrV1` do too — see
        // `which_phase_peaks_is_per_profile_and_measured_not_assumed`, which is the
        // authority. (An earlier version of this comment claimed "every other
        // profile peaks at encode"; both halves were false against code in this
        // same file, which is the third time that claim has had to be corrected.)
        assert_eq!(no_ir.accounted_bytes, no_ir.render_bytes);
        assert!(no_ir.encode_bytes < no_ir.render_bytes);

        // Its render phase matches the AVIF profile exactly (same shared source,
        // same single rendition); only encode diverges.
        let avif = estimate_peak(
            &shape(10, 10, false),
            RunProfile::HdrAvif { export_ir: false },
            SamplePlan::none(),
        )
        .unwrap();
        assert_eq!(no_ir.render_bytes, avif.render_bytes);
        assert!(
            no_ir.encode_bytes < avif.encode_bytes,
            "the linear TIFF must be cheaper at encode than AVIF (no codec staging)"
        );
        // And therefore cheaper overall than every other display preset.
        let gain_map = estimate_peak(
            &shape(10, 10, false),
            RunProfile::UltraHdrV1 { export_ir: false },
            SamplePlan::none(),
        )
        .unwrap();
        assert!(no_ir.accounted_bytes < avif.accounted_bytes);
        assert!(no_ir.accounted_bytes < gain_map.accounted_bytes);
    }

    #[test]
    fn hdr_coded_tiff_profile_sits_between_the_linear_tiff_and_avif() {
        // 10x10, no IR: render is the shared 36 B/px every display profile pays;
        // encode is 12 (decoded) + 12 (rendition) + 6 (u16 codes) = 30 B/px.
        let coded = estimate_peak(
            &shape(10, 10, false),
            RunProfile::HdrCodedTiff { export_ir: false },
            SamplePlan::none(),
        )
        .unwrap();
        assert_eq!(coded.render_bytes, 3_600);
        assert_eq!(coded.encode_bytes, 3_000);

        let linear = estimate_peak(
            &shape(10, 10, false),
            RunProfile::HdrLinearTiff { export_ir: false },
            SamplePlan::none(),
        )
        .unwrap();
        let avif = estimate_peak(
            &shape(10, 10, false),
            RunProfile::HdrAvif { export_ir: false },
            SamplePlan::none(),
        )
        .unwrap();
        // Every display profile shares `render_linear`, so the render phase is
        // identical across all three; only encode distinguishes them.
        assert_eq!(coded.render_bytes, linear.render_bytes);
        assert_eq!(coded.render_bytes, avif.render_bytes);
        // The quantize buffer is exactly the 6 B/px the linear TIFF does not pay...
        assert_eq!(coded.encode_bytes - linear.encode_bytes, 600);
        // ...and it is far cheaper than a native codec's working set.
        assert!(coded.encode_bytes < avif.encode_bytes);
        // Render still wins overall here too, because the quantize buffer is smaller
        // than the shared source it replaces.
        assert_eq!(coded.accounted_bytes, coded.render_bytes);
    }

    #[test]
    fn hdr_coded_tiff_ir_export_stages_a_u16_plane() {
        // Unlike `HdrLinearTiff` (f32, 4 B/px) this preset resolves `OutDepth::U16`,
        // so its IR plane costs 2 B/px — the same as the AVIF and gain-map profiles.
        let with_ir_no_export = estimate_peak(
            &shape(10, 10, true),
            RunProfile::HdrCodedTiff { export_ir: false },
            SamplePlan::none(),
        )
        .unwrap();
        let with_ir = estimate_peak(
            &shape(10, 10, true),
            RunProfile::HdrCodedTiff { export_ir: true },
            SamplePlan::none(),
        )
        .unwrap();
        assert_eq!(with_ir.encode_bytes - with_ir_no_export.encode_bytes, 200);
        // Render pays for the IR plane twice (decoded + shared ACEScg source).
        assert_eq!(with_ir_no_export.render_bytes, 4_400);
    }

    #[test]
    fn hdr_linear_tiff_ir_export_is_free_because_f32_needs_no_staging() {
        // `export_ir_to_writer`'s f32 arms pass the existing `&[f32]` straight to the
        // writer — no `quantize_u16`, so no staging buffer — which is why `Convert`'s
        // `OutDepth::F32` arm also contributes 0. Charging anything here would
        // over-state the peak and reject runs that fit. Pinned in both directions:
        // the increment is zero, and the u16 profiles' is not.
        let with_ir_no_export = estimate_peak(
            &shape(10, 10, true),
            RunProfile::HdrLinearTiff { export_ir: false },
            SamplePlan::none(),
        )
        .unwrap();
        let with_ir = estimate_peak(
            &shape(10, 10, true),
            RunProfile::HdrLinearTiff { export_ir: true },
            SamplePlan::none(),
        )
        .unwrap();
        assert_eq!(
            with_ir.encode_bytes, with_ir_no_export.encode_bytes,
            "an f32 IR export allocates nothing, so it must cost nothing"
        );
        assert_eq!(with_ir_no_export.render_bytes, 4_400);

        // The same flag on a u16-depth profile *does* cost 2 B/px, so the zero above
        // is a property of the depth and not a dropped term.
        let coded_off = estimate_peak(
            &shape(10, 10, true),
            RunProfile::HdrCodedTiff { export_ir: false },
            SamplePlan::none(),
        )
        .unwrap();
        let coded_on = estimate_peak(
            &shape(10, 10, true),
            RunProfile::HdrCodedTiff { export_ir: true },
            SamplePlan::none(),
        )
        .unwrap();
        assert_eq!(coded_on.encode_bytes - coded_off.encode_bytes, 200);
        // And the legacy f32 path agrees — this is the established convention.
        let convert_f32_off = estimate_peak(
            &shape(10, 10, true),
            RunProfile::Convert {
                depth: OutDepth::F32,
                export_ir: false,
            },
            SamplePlan::none(),
        )
        .unwrap();
        let convert_f32_on = estimate_peak(
            &shape(10, 10, true),
            RunProfile::Convert {
                depth: OutDepth::F32,
                export_ir: true,
            },
            SamplePlan::none(),
        )
        .unwrap();
        assert_eq!(convert_f32_on.encode_bytes, convert_f32_off.encode_bytes);
    }

    #[test]
    fn which_phase_peaks_is_per_profile_and_measured_not_assumed() {
        // Pins the actual peak phase of every profile, because a plain-language
        // claim about it has now been wrong twice: the `hdr-linear-tiff` comment
        // first said it was the *only* render-peaking profile (its coded sibling is
        // too), and the correction then said every non-TIFF profile peaks at encode
        // — which this test immediately falsified for `ultra-hdr-v1`, whose four
        // display buffers make render (72 B/px) exceed encode (68 B/px).
        //
        // No category, then. Just the measured truth, so the next author reads it
        // off a test instead of a sentence.
        let peak_phase = |profile| {
            let e = estimate_peak(&shape(10, 10, false), profile, SamplePlan::none()).unwrap();
            if e.accounted_bytes == e.render_bytes {
                "render"
            } else if e.accounted_bytes == e.encode_bytes {
                "encode"
            } else {
                "neither"
            }
        };
        for (profile, expected) in [
            (RunProfile::HdrLinearTiff { export_ir: false }, "render"),
            (RunProfile::HdrCodedTiff { export_ir: false }, "render"),
            (RunProfile::UltraHdrV1 { export_ir: false }, "render"),
            (RunProfile::HdrAvif { export_ir: false }, "encode"),
            (
                RunProfile::Convert {
                    depth: OutDepth::U16,
                    export_ir: false,
                },
                "encode",
            ),
        ] {
            assert_eq!(peak_phase(profile), expected, "{profile:?}");
        }
    }

    /// The largest scan on hand (`largest.tif`), the calibration reference.
    fn big() -> ImageShape {
        shape(10368, 7200, true)
    }

    #[test]
    fn phase_totals_match_the_documented_bytes_per_pixel() {
        // The model, pinned per phase against the hand-derived per-pixel costs in
        // the module doc (HDRi: 18 / 32 / 38 B/px, film-base 16 + 12·s). A change
        // here is a change to what the gate promises, so it must be deliberate.
        let px = 1000u64 * 1000;
        let e = estimate_peak(&shape(1000, 1000, true), convert_u16(), SamplePlan::none()).unwrap();
        assert_eq!(e.decode_bytes, 18 * px);
        // Nothing sampled (explicit base): the decoded image alone.
        assert_eq!(e.film_base_bytes, 16 * px);
        assert_eq!(e.render_bytes, 32 * px);
        assert_eq!(e.encode_bytes, 38 * px);
        assert_eq!(e.accounted_bytes, 38 * px);

        // Without an IR plane every phase loses its IR buffers: 18 / 12 / 24 / 30.
        let e =
            estimate_peak(&shape(1000, 1000, false), convert_u16(), SamplePlan::none()).unwrap();
        assert_eq!(e.decode_bytes, 18 * px);
        assert_eq!(e.film_base_bytes, 12 * px);
        assert_eq!(e.render_bytes, 24 * px);
        assert_eq!(e.encode_bytes, 30 * px);
    }

    #[test]
    fn film_base_phase_counts_the_sampled_rectangle() {
        // The film-base phase is the decoded image plus 12 B per sampled pixel —
        // the omission that let `inspect`/`estimate` exceed their own estimate.
        let px = 1000u64 * 1000;
        let s = shape(1000, 1000, true);

        // `auto`: the interior rectangle `select_auto_base` samples. Sized from
        // `film_base`'s own rule so the two cannot drift.
        let interior = film_base::auto_interior_pixels(1000, 1000);
        assert_eq!(interior, 800 * 800, "10% scan depth on each side");
        let auto = estimate_peak(&s, RunProfile::DecodeOnly, SamplePlan::auto()).unwrap();
        assert_eq!(auto.film_base_bytes, 16 * px + 12 * interior);
        // …and it is the peak for a decode-only run (above decode's 18 B/px).
        assert_eq!(auto.accounted_bytes, auto.film_base_bytes);

        // A full-frame rectangle (`--base-region 0,0,w,h`) is the worst case: 28 B/px.
        let whole = estimate_peak(&s, RunProfile::DecodeOnly, SamplePlan::rect(px)).unwrap();
        assert_eq!(whole.film_base_bytes, 28 * px);

        // …and it does NOT leave `convert` alone. The sample is freed before the
        // render, but freed pages stay resident, so it is retained into the later
        // phases: render 32 + 12, encode 38 + 12 = 50 B/px. Pinned because the
        // first version of this model let the phase merely *compete* with encode
        // and under-estimated a real full-frame-region convert by 10.2%.
        let convert = estimate_peak(&s, convert_u16(), SamplePlan::rect(px)).unwrap();
        assert_eq!(convert.render_bytes, 44 * px);
        assert_eq!(convert.encode_bytes, 50 * px);
        assert_eq!(convert.accounted_bytes, 50 * px);
        // With nothing sampled, the retained term is zero and encode is 38 again.
        let explicit = estimate_peak(&s, convert_u16(), SamplePlan::none()).unwrap();
        assert_eq!(explicit.encode_bytes, 38 * px);

        // Several rectangles (e.g. `estimate --grid` plus `--d-max-region`) are
        // gathered one at a time, so the largest wins — measured: sampling two
        // full-frame rectangles peaks identically to sampling one.
        let plan = SamplePlan::auto().with_rect(px).with_rect(10_000);
        let combined = estimate_peak(&s, RunProfile::DecodeOnly, plan).unwrap();
        assert_eq!(combined.film_base_bytes, 28 * px, "largest rectangle wins");

        // `--grid` counts one cell (~1/16 of its rectangle), not the rectangle.
        let cell = film_base::grid_cell_pixels(1000, 1000);
        assert_eq!(cell, 250 * 250);
        let grid = estimate_peak(
            &s,
            RunProfile::DecodeOnly,
            SamplePlan::none().with_whole_frame_grid(),
        )
        .unwrap();
        assert_eq!(grid.film_base_bytes, 16 * px + 12 * cell);

        // A frame too small to scan samples no interior at all — and must not
        // become a spurious rejection.
        assert_eq!(film_base::auto_interior_pixels(4, 4), 0);
        let tiny = estimate_peak(
            &shape(4, 4, true),
            RunProfile::DecodeOnly,
            SamplePlan::auto(),
        )
        .unwrap();
        assert_eq!(tiny.film_base_bytes, 16 * 16);
    }

    #[test]
    fn f32_output_skips_the_quantize_buffer_and_export_ir_adds_one() {
        let px = 1000u64 * 1000;
        // f32 writes the working buffer verbatim — encode allocates nothing extra,
        // so the two overlapping images (32 B/px) are the peak.
        let f32_out = estimate_peak(
            &shape(1000, 1000, true),
            RunProfile::Convert {
                depth: OutDepth::F32,
                export_ir: false,
            },
            SamplePlan::none(),
        )
        .unwrap();
        assert_eq!(f32_out.encode_bytes, 32 * px);
        assert_eq!(f32_out.accounted_bytes, 32 * px);

        // --export-ir stages a u16 IR plane on top of the u16 RGB buffer.
        let with_ir = estimate_peak(
            &shape(1000, 1000, true),
            RunProfile::Convert {
                depth: OutDepth::U16,
                export_ir: true,
            },
            SamplePlan::none(),
        )
        .unwrap();
        assert_eq!(with_ir.encode_bytes, 40 * px);

        // …but only when there is an IR plane to export.
        let no_plane = estimate_peak(
            &shape(1000, 1000, false),
            RunProfile::Convert {
                depth: OutDepth::U16,
                export_ir: true,
            },
            SamplePlan::none(),
        )
        .unwrap();
        assert_eq!(no_plane.encode_bytes, 30 * px);
    }

    #[test]
    fn working_buffers_are_sized_from_the_working_channel_count() {
        // Only the source read buffer follows the file's channel count; every
        // working buffer is 3-channel by `LinearImage`/`io::encode` invariant. A
        // hypothetical 1-channel 16-bit input (`decode` rejects it today) must not
        // shrink the f32/u16 buffers to a third.
        let px = 1000u64 * 1000;
        let gray = ImageShape::new(1000, 1000, 1, 16, false).unwrap();
        let e = estimate_peak(&gray, convert_u16(), SamplePlan::none()).unwrap();
        // rgb32 (12) + the 1-channel u16 read buffer (2).
        assert_eq!(e.decode_bytes, 14 * px);
        // Two 3-channel f32 images + the 3-channel u16 quantize buffer.
        assert_eq!(e.render_bytes, 24 * px);
        assert_eq!(e.encode_bytes, 30 * px);
    }

    #[test]
    fn decode_only_profile_counts_no_render_or_encode() {
        // `inspect`/`estimate` must not be gated on a render they never run.
        let e = estimate_peak(
            &shape(1000, 1000, true),
            RunProfile::DecodeOnly,
            SamplePlan::none(),
        )
        .unwrap();
        assert_eq!(e.render_bytes, 0);
        assert_eq!(e.encode_bytes, 0);
        assert!(
            e.accounted_bytes
                < estimate_peak(&shape(1000, 1000, true), convert_u16(), SamplePlan::none())
                    .unwrap()
                    .accounted_bytes
        );
    }

    #[test]
    fn allowance_is_applied_on_top_of_the_accounted_buffers() {
        let e = estimate_peak(&shape(1000, 1000, true), convert_u16(), SamplePlan::none()).unwrap();
        let expected =
            e.accounted_bytes + e.accounted_bytes / 100 * ALLOWANCE_PERCENT + ALLOWANCE_FIXED_BYTES;
        assert_eq!(e.estimated_peak_bytes, expected);
        assert!(e.estimated_peak_bytes > e.accounted_bytes);
    }

    #[test]
    fn estimate_pins_the_model_output_for_the_calibration_shapes() {
        // Regression pin on the model itself: the exact estimate for the two known
        // shapes. Unlike the measured comparison below (both sides frozen
        // literals), this fails the moment the model's output changes for a shape
        // we have documented numbers for — which is what makes a model change
        // deliberate.
        let standard = shape(5184, 3599, true); // a roll frame, 18.66 MP HDRi
        let ultra_hdr = shape(5184, 3600, true); // measured Ultra HDR scan, 18.66 MP HDRi
        for (shape, profile, sampling, accounted, estimated) in [
            // 74.65 MP: encode 38 B/px, decode-only 18 B/px (explicit base).
            (
                big(),
                convert_u16(),
                SamplePlan::none(),
                2_836_684_800u64,
                3_396_405_248u64,
            ),
            (
                big(),
                RunProfile::DecodeOnly,
                SamplePlan::none(),
                1_343_692_800,
                1_679_464_448,
            ),
            // 18.66 MP.
            (
                standard,
                convert_u16(),
                SamplePlan::none(),
                708_974_208,
                949_538_066,
            ),
            (
                standard,
                RunProfile::DecodeOnly,
                SamplePlan::none(),
                335_829_888,
                520_422_086,
            ),
            // Recorded gain-map run used an explicit film base and no IR export.
            (
                ultra_hdr,
                RunProfile::UltraHdrV1 { export_ir: false },
                SamplePlan::none(),
                1_492_992_000,
                1_851_158_528,
            ),
        ] {
            let e = estimate_peak(&shape, profile, sampling).unwrap();
            assert_eq!(e.accounted_bytes, accounted, "{profile:?}");
            assert_eq!(e.estimated_peak_bytes, estimated, "{profile:?}");
        }

        // The auto path's interior sample on the big frame: 8928x5760 px x 12 B on
        // top of the 16 B/px decoded image. Derived, not measured — flagged in the
        // module doc as an open calibration item.
        let auto = estimate_peak(&big(), RunProfile::DecodeOnly, SamplePlan::auto()).unwrap();
        assert_eq!(auto.film_base_bytes, 1_811_496_960);
        assert_eq!(auto.accounted_bytes, 1_811_496_960);
        assert_eq!(auto.estimated_peak_bytes, 2_217_439_223);
    }

    #[test]
    fn estimate_stays_conservative_against_the_measured_peaks() {
        // The calibration set from the module doc (macOS/aarch64 peak RSS on the
        // release binary). The model must never come in *under* a measured peak —
        // an under-estimate would let the gate approve a run that then OOMs.
        //
        // Documentation of intent, not a live check: both sides are frozen
        // literals, so this can never fail on any target. The regression pin on the
        // model's own output is `estimate_pins_the_model_output_for_the_calibration_shapes`.
        // Every measured conversion used an explicit `--film-base`; keep the
        // corresponding no-sampling plan alongside each frozen case.
        let standard = shape(5184, 3599, true); // a roll frame, 18.66 MP HDRi
        let ultra_hdr = shape(5184, 3600, true); // recorded gain-map calibration scan
        let export_ir_u16 = RunProfile::Convert {
            depth: OutDepth::U16,
            export_ir: true,
        };
        let hdr_out = RunProfile::Convert {
            depth: OutDepth::F32,
            export_ir: false,
        };
        let cases: [(ImageShape, RunProfile, SamplePlan, u64); 9] = [
            (
                big(),
                RunProfile::DecodeOnly,
                SamplePlan::none(),
                1_503_330_304,
            ),
            (big(), convert_u16(), SamplePlan::none(), 3_145_760_768),
            (big(), export_ir_u16, SamplePlan::none(), 3_145_711_616),
            (big(), hdr_out, SamplePlan::none(), 2_697_887_744),
            (standard, convert_u16(), SamplePlan::none(), 681_164_800),
            (
                standard,
                RunProfile::DecodeOnly,
                SamplePlan::none(),
                328_286_208,
            ),
            (
                ultra_hdr,
                RunProfile::UltraHdrV1 { export_ir: false },
                SamplePlan::none(),
                1_681_408_000,
            ),
            // The two `hdr-pq` calibration runs behind
            // `AVIF_STAGING_BYTES_PER_PX`. Both used an explicit `--film-base`.
            (
                ultra_hdr,
                RunProfile::HdrAvif { export_ir: false },
                SamplePlan::none(),
                1_472_397_312,
            ),
            (
                shape(10368, 7200, true),
                RunProfile::HdrAvif { export_ir: false },
                SamplePlan::none(),
                5_865_947_136,
            ),
        ];
        for (shape, profile, sampling, measured) in cases {
            let e = estimate_peak(&shape, profile, sampling).unwrap();
            assert!(
                e.estimated_peak_bytes >= measured,
                "{profile:?} at {}x{}: estimate {} under measured {measured}",
                shape.width,
                shape.height,
                e.estimated_peak_bytes
            );
        }

        // …and not wildly over, where being over actually matters: on the full-size
        // frames (the only ones near a plausible budget) the estimate stays within
        // 25% of measured. Small frames are deliberately looser — the fixed
        // allowance dominates them (see the module doc).
        for (profile, measured) in [
            (RunProfile::DecodeOnly, 1_503_330_304u64),
            (convert_u16(), 3_145_760_768),
            (export_ir_u16, 3_145_711_616),
            (hdr_out, 2_697_887_744),
        ] {
            let e = estimate_peak(&big(), profile, SamplePlan::none()).unwrap();
            assert!(
                e.estimated_peak_bytes < measured / 4 * 5,
                "{profile:?}: estimate {} more than 25% over measured {measured}",
                e.estimated_peak_bytes
            );
        }
    }

    #[test]
    fn absurd_dimensions_overflow_loudly_instead_of_wrapping() {
        // A corrupt header must not wrap into a small, permissive estimate.
        let huge = shape(u32::MAX, u32::MAX, true);
        let err = estimate_peak(&huge, convert_u16(), SamplePlan::none()).unwrap_err();
        assert_eq!(err.exit_code(), 1);
        assert!(err.to_string().contains("overflow"), "{err}");
    }

    #[test]
    fn over_budget_is_a_resource_error_naming_both_numbers() {
        let budget = Budget::resolve(Some(1024 * 1024 * 1024)); // 1 GiB
        let err = preflight(&big(), convert_u16(), SamplePlan::none(), budget, None).unwrap_err();
        assert_eq!(err.exit_code(), 6);
        let msg = err.to_string();
        assert!(msg.contains("--max-memory"), "{msg}");
        assert!(msg.contains("1.0 GiB"), "{msg}");
        assert!(msg.contains("10368x7200"), "{msg}");
        assert!(msg.contains("smaller frame"), "{msg}");
    }

    #[test]
    fn minimum_viable_budget_is_the_fixed_allowance() {
        // The fixed allowance is unconditional, so it is a floor: a 1x1 image
        // estimates at 128 MiB + its 30 bytes, and any budget at or below the floor
        // rejects *every* possible input. Documented here (and in the rejection
        // message) so the floor isn't a surprise.
        let tiny = shape(1, 1, false);
        let e = estimate_peak(&tiny, convert_u16(), SamplePlan::none()).unwrap();
        assert_eq!(e.estimated_peak_bytes, ALLOWANCE_FIXED_BYTES + 30);

        let err = preflight(
            &tiny,
            convert_u16(),
            SamplePlan::none(),
            Budget::resolve(Some(ALLOWANCE_FIXED_BYTES)),
            None,
        )
        .unwrap_err();
        assert_eq!(err.exit_code(), 6);
        let msg = err.to_string();
        // The advice must not be the impossible "convert a smaller frame" — it must
        // name the fixed floor and say a smaller frame cannot help.
        assert!(!msg.contains("or convert a smaller frame"), "{msg}");
        assert!(msg.contains("no smaller frame fits"), "{msg}");
        assert!(msg.contains("128.0 MiB"), "{msg}");

        // One byte more than the floor plus the frame admits it.
        assert!(
            preflight(
                &tiny,
                convert_u16(),
                SamplePlan::none(),
                Budget::resolve(Some(ALLOWANCE_FIXED_BYTES + 30)),
                None,
            )
            .is_ok()
        );
    }

    #[test]
    fn default_budget_admits_the_largest_real_scan() {
        // The whole point of the fixed default: the biggest scan on hand must pass
        // it, on every machine, without a flag — including on the auto path, whose
        // interior sample the first version of this model forgot.
        for sampling in [SamplePlan::none(), SamplePlan::auto()] {
            let report =
                preflight(&big(), convert_u16(), sampling, Budget::resolve(None), None).unwrap();
            assert_eq!(report.budget_source, BudgetSource::Default);
            assert_eq!(report.budget_bytes, DEFAULT_MAX_MEMORY_BYTES);
            assert_eq!(report.decision, Verdict::Ok);
        }
    }

    #[test]
    fn budget_reports_its_own_provenance() {
        // The enum is the single source of both wire keys, so a "default" budget
        // can't report a non-default byte count.
        assert_eq!(Budget::resolve(None).bytes(), DEFAULT_MAX_MEMORY_BYTES);
        assert_eq!(Budget::resolve(None).source(), BudgetSource::Default);
        assert_eq!(Budget::resolve(Some(1234)).bytes(), 1234);
        assert_eq!(Budget::resolve(Some(1234)).source(), BudgetSource::Flag);
    }

    #[test]
    fn warn_tier_fires_only_against_detected_ram() {
        let budget = Budget::resolve(Some(8 * 1024 * 1024 * 1024));

        // A 4 GiB machine: the estimate is over 70% of RAM → warn (but proceed).
        let small = preflight(
            &big(),
            convert_u16(),
            SamplePlan::none(),
            budget,
            Some(4 * 1024 * 1024 * 1024),
        )
        .unwrap();
        assert_eq!(small.decision, Verdict::Warn);
        let msg = warn_message(&small).expect("a warn verdict must carry a message");
        assert!(msg.contains("70%"), "{msg}");
        assert!(msg.contains("4.0 GiB"), "{msg}");

        // A 48 GiB machine: nowhere near the threshold.
        let large = preflight(
            &big(),
            convert_u16(),
            SamplePlan::none(),
            budget,
            Some(48 * 1024 * 1024 * 1024),
        )
        .unwrap();
        assert_eq!(large.decision, Verdict::Ok);
        assert_eq!(warn_message(&large), None);

        // Undetectable RAM disables the tier rather than guessing — and the
        // message guard lives inside `warn_message`, so no caller can render an
        // "unknown of RAM" warning.
        let unknown = preflight(&big(), convert_u16(), SamplePlan::none(), budget, None).unwrap();
        assert_eq!(unknown.decision, Verdict::Ok);
        assert_eq!(unknown.detected_total_ram_bytes, None);
        assert_eq!(warn_message(&unknown), None);
    }

    #[test]
    fn max_memory_parses_units_and_rejects_nonsense() {
        assert_eq!(parse_max_memory("4GiB").unwrap(), 4 * 1024 * 1024 * 1024);
        assert_eq!(parse_max_memory("4G").unwrap(), 4 * 1024 * 1024 * 1024);
        assert_eq!(parse_max_memory("4GB").unwrap(), 4_000_000_000);
        assert_eq!(parse_max_memory(" 512 MiB ").unwrap(), 512 * 1024 * 1024);
        assert_eq!(parse_max_memory("2000000000").unwrap(), 2_000_000_000);
        assert_eq!(parse_max_memory("1b").unwrap(), 1);

        for bad in ["", "GiB", "4 GiBs", "-1", "4.5GiB", "0", "0GiB"] {
            let err = parse_max_memory(bad).unwrap_err();
            assert_eq!(err.exit_code(), 2, "{bad:?} should be a usage error");
        }
        assert_eq!(parse_max_memory("99999999TiB").unwrap_err().exit_code(), 2);
    }

    #[test]
    fn detected_ram_is_plausible_or_absent() {
        // Fail-soft: `None` is allowed (unsupported platform), but a reported
        // value must be sane — a bogus tiny number would make the warn tier fire
        // on every run.
        if let Some(ram) = detect_total_ram() {
            assert!(
                ram >= 256 * 1024 * 1024,
                "implausible detected RAM: {ram} bytes"
            );
        }
        // The Linux path is compiled on every target; off Linux the files are
        // absent, so it must degrade to `None` rather than misreport.
        if !cfg!(target_os = "linux") {
            assert_eq!(linux_total_ram(), None);
        }
    }

    #[test]
    fn linux_ram_parsers_are_fail_soft() {
        // MemTotal is read in kB and may not be the first line.
        assert_eq!(
            parse_meminfo_total("MemFree:  1 kB\nMemTotal:       16384 kB\n"),
            Some(16384 * 1024)
        );
        for bad in [
            "",
            "MemTotal:\n",
            "MemTotal: lots kB\n",
            "SwapTotal: 4 kB\n",
        ] {
            assert_eq!(parse_meminfo_total(bad), None, "{bad:?}");
        }

        // A cgroup ceiling is honored; "no limit" in either cgroup dialect is not.
        assert_eq!(
            parse_cgroup_limit("2147483648\n"),
            Some(2 * 1024 * 1024 * 1024)
        );
        assert_eq!(parse_cgroup_limit("max\n"), None, "cgroup v2 unlimited");
        assert_eq!(
            parse_cgroup_limit("9223372036854771712\n"),
            None,
            "cgroup v1 unlimited sentinel"
        );
        for bad in ["", "0", "not-a-number"] {
            assert_eq!(parse_cgroup_limit(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn cgroup_paths_resolve_the_process_s_own_nested_cgroup() {
        // The limit that binds a containerized run lives at the process's *own*
        // cgroup, which under Kubernetes/systemd is nested. Reading only the root
        // files reports `max` there and leaves the warn tier silent in exactly the
        // environment it exists for. Runs on every target — this logic is Linux-only
        // in effect but not in compilation, so CI is not the first place it builds.
        let k8s = "0::/kubepods.slice/kubepods-burstable.slice/pod123/container456\n";
        let (v2, v1) = parse_self_cgroup(k8s);
        assert_eq!(
            v2.as_deref(),
            Some("/kubepods.slice/kubepods-burstable.slice/pod123/container456")
        );
        assert_eq!(v1, None);

        // v1: the memory controller's own line, among others.
        let v1_text = "12:memory:/docker/abc\n11:cpu,cpuacct:/docker/abc\n0::/\n";
        let (v2, v1) = parse_self_cgroup(v1_text);
        assert_eq!(v2, None, "a root v2 path is not nested");
        assert_eq!(v1.as_deref(), Some("/docker/abc"));

        // Ancestors, most specific first, ending at the root.
        assert_eq!(
            ancestors_of("/kubepods/pod1/ctr"),
            vec!["/kubepods/pod1/ctr", "/kubepods/pod1", "/kubepods", ""]
        );
        assert_eq!(ancestors_of("/"), vec![""]);

        // The candidate list puts the process's own file first and still ends with
        // the root fallbacks, so a non-nested host keeps working.
        let paths = cgroup_limit_paths();
        assert!(
            paths.last().map(String::as_str) == Some("/sys/fs/cgroup/memory/memory.limit_in_bytes"),
            "root v1 fallback must remain last: {paths:?}"
        );
        assert!(
            paths.contains(&"/sys/fs/cgroup/memory.max".to_string()),
            "root v2 fallback must be present: {paths:?}"
        );
    }

    #[test]
    fn an_out_of_bounds_rectangle_cannot_become_a_resource_rejection() {
        // A typo'd `--base-region 0,0,999999,999999` is a *usage* error that
        // `film_base::region_channels` reports (exit 2). Estimating its raw `w*h`
        // would instead reject the run as a 12852 GiB resource overrun (exit 6), or
        // overflow the model and blame the image's own dimensions. The sample is
        // clamped to the frame, so the gate stays quiet and the real error surfaces.
        let s = shape(502, 462, false);
        let frame = 502u64 * 462;
        let absurd = estimate_peak(
            &s,
            RunProfile::DecodeOnly,
            SamplePlan::rect(999_999 * 999_999),
        )
        .unwrap();
        let clamped = estimate_peak(&s, RunProfile::DecodeOnly, SamplePlan::rect(frame)).unwrap();
        assert_eq!(
            absurd.film_base_bytes, clamped.film_base_bytes,
            "an oversized rectangle must estimate as at most the whole frame"
        );
        // …including one big enough to overflow the model if left unclamped.
        let huge =
            estimate_peak(&s, RunProfile::DecodeOnly, SamplePlan::rect(u64::MAX / 2)).unwrap();
        assert_eq!(huge.film_base_bytes, clamped.film_base_bytes);
    }

    #[test]
    fn report_serializes_flat_with_the_documented_keys() {
        let report = preflight(
            &shape(1000, 1000, true),
            convert_u16(),
            SamplePlan::auto(),
            Budget::resolve(None),
            Some(48 * 1024 * 1024 * 1024),
        )
        .unwrap();
        let json = serde_json::to_value(report).unwrap();
        for key in [
            "estimated_peak_bytes",
            "accounted_bytes",
            "decode_bytes",
            "film_base_bytes",
            "render_bytes",
            "encode_bytes",
            "budget_bytes",
            "budget_source",
            "decision",
            "detected_total_ram_bytes",
        ] {
            assert!(json.get(key).is_some(), "missing report key {key}");
        }
        assert_eq!(json["budget_source"], "default");
        assert_eq!(json["decision"], "ok");
    }
}
