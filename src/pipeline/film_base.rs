//! `Dmin` / film-base estimation (pure).
//!
//! The film base is the unexposed leader/rebate of the negative: the area of
//! minimum density, hence **maximum transmission** — nothing on the negative
//! scans brighter than clean base. Its per-channel transmission is the `Dmin`
//! anchor the `density` algorithm divides by (`D = -log10(scan / Dmin)`), so a
//! good estimate matters.
//!
//! The source of the base is a single mutually-exclusive choice carried by
//! [`FilmBaseSource`] (resolved from the flags/recipe in `cli.rs`): an explicit
//! per-channel override, a user-supplied region to sample, or auto-detection of
//! the unexposed rebate. This stage just honors whichever the caller selected.
//! (The opt-in content-based source, ladder tier 3, lives in the separate
//! `film-base/content-fallback` task — auto only *suggests* it on refusal.)
//!
//! Auto detection models the real scan layout — `dark film holder → thin
//! unexposed rebate → exposed picture` — by marching 1-px strips inward from
//! each edge and looking for a bright, uniform band sitting **behind** a dark
//! holder run ([`rebate_candidates`]). Requiring the holder outside the band is
//! the corroborating signal that defeats the classic false positive (a bright,
//! uniform scene region bleeding to the frame edge has no holder outside it),
//! and "highest-transmission candidate wins" is physically grounded: the rebate
//! is `Dmin` (per-channel maximum transmission), so no genuine picture area can
//! out-transmit it. (In this detector "bright" is the *raw-scan transmission*
//! domain — the rebate is scan-brightest yet renders to scene-black; see
//! design-spec §4 "Terminology & value domains".) Gates stay deliberately strict
//! — auto is a convenience tier (design-spec §9 ladder), so a refused detection
//! is acceptable and a wrong one is not.
//!
//! **Known residual false positive.** One case the strict RGB gates still can't
//! catch: a flat, bright *scene* region that happens to sit behind the holder on
//! a rebate-less / cropped scan (e.g. sky along one edge) satisfies every gate
//! (holder-backed, uniform, transitions to picture before the cap, brighter than
//! the interior) and, as the sole surviving candidate, is taken as the base — a
//! wrong `Dmin`. Telling it from a genuine thin rebate needs signals a
//! single-frame RGB pass doesn't have: colour-independent corroboration
//! (`auto-base-neutral-stock`) or opacity-based film-boundary detection
//! (`ir-holder-detection`). Neither blunt remedy is acceptable here — rejecting
//! all thin uniform holder-backed bands would drop genuine rebates, and requiring
//! cross-edge corroboration would reject legitimate single-edge rebate (common,
//! and tested). The failure is bounded: a wrong base is a *correctable global
//! per-channel cast* (design-spec §8), never a crossover, and pinning via
//! `--base-region` / `--film-base` avoids it — which is the recommended path for
//! work you're keeping.
//!
//! **Known limitation — shallow-holder rebate exclusion (IR mask, deferred).** The
//! chromogenic IR holder mask classifies each along-edge segment from a *shallow*
//! near-edge probe band ([`holder_probe_depth`] / [`median_ir_probe`]) and excludes
//! every IR-dark segment from the rebate search ([`film_along_ranges`]). So a thin
//! opaque holder margin at the very edge — IR-dark only within that shallow probe —
//! with a genuine rebate sitting *directly behind* it is excluded along with the
//! holder, and auto-base can miss a rebate the RGB-only path (which scans the full
//! depth over the whole edge) would have found. This is a **deliberate,
//! user-accepted trade-off**, not a bug: the shallow probe is exactly what lets the
//! mask separate a thin holder from the bright film behind it in the common case
//! (see [`ir_holder_mask`] and the `shallow_probe_reads_a_thin_holder_over_bright_film`
//! test). The failure is bounded the same way as above — auto either refuses loudly
//! (no surviving candidate) or, if it anchors on another band, yields a correctable
//! global per-channel cast, never a crossover (design-spec §8) — and
//! `--base-region` / `--film-base` is the workaround. The roadmap fix is a
//! **depth-aware occlusion classification**: exclude a span only when it reads
//! IR-dark through the full scan depth, not merely the shallow probe (an
//! `ir-holder-detection` follow-up).

use serde::Serialize;

use crate::types::{FilmBase, FilmBaseSource, FilmType, LinearImage, NcError, Result};

/// Percentile used to summarize a region per channel. A high percentile (rather
/// than the raw max) resists hot pixels / dust sparkles while still landing on
/// the high-transmission film base. Design task suggests 95th–99th; 97th is the middle.
const SAMPLE_PERCENTILE: f32 = 0.97;

/// Low percentile paired with [`SAMPLE_PERCENTILE`] for the uniformity check.
const LOW_PERCENTILE: f32 = 0.10;

/// Max acceptable per-channel relative spread `(p_high - p_low) / p_high` for a
/// strip / band / region to count as near-uniform unexposed base. Applied to
/// **all** channels (the strict gate): real rebate is flat in every channel.
const MAX_RELATIVE_SPREAD: f32 = 0.15;

/// Fraction of the shorter image dimension the inward scan marches from each
/// edge. The rebate is a thin inset band, so ~10% is plenty; deeper "bands" are
/// picture content.
const REBATE_SCAN_FRAC: f32 = 0.10;

/// A strip whose per-channel high percentile is below this transmission on
/// every channel is the dark film holder. Real holders measure ≈ 0.01; the
/// dimmest real rebate channel measured ≈ 0.14 (blue), so 0.05 splits them with
/// margin on both sides.
const HOLDER_MAX_TRANSMISSION: f32 = 0.05;

/// Minimum band thickness (consecutive uniform strips) for a rebate candidate.
/// One lone strip is too noise-prone to anchor a whole conversion on.
const MIN_BAND_STRIPS: u32 = 2;

/// Max per-channel relative step between adjacent strips inside one band. Splits
/// the rebate from an adjacent flat picture region of a different value (both
/// are individually "uniform"), so the band never straddles the rebate/picture
/// boundary.
const STRIP_CONTINUITY_TOL: f32 = 0.10;

/// A candidate must have higher transmission than the frame-interior median by this
/// factor on **every** channel (the rebate is per-channel minimum density ⇒
/// per-channel maximum transmission). All-channel with a 5% margin replaces the
/// Step-1 heuristic's lenient any-channel 2% gate, which a high-transmission
/// surround could pass.
const INTERIOR_BRIGHTNESS_MARGIN: f32 = 1.05;

/// Cross-edge agreement tolerance: per-channel relative difference above which
/// surviving candidates on different edges are reported as disagreeing (a
/// warning — the highest-transmission candidate still wins, but the ambiguity is surfaced and
/// `--strict` can refuse it).
const CROSS_EDGE_AGREE_TOL: f32 = 0.15;

/// IR transmission at or below which a near-edge segment reads as the opaque film
/// holder. Chromogenic film (base, rebate, picture, even fully-exposed leader) is
/// IR-transparent and reads bright — measured ≈ 0.6–0.7 on real HDRi scans — while
/// the holder blocks IR and reads dark (≈ 0.02), a ~25× separation, so 0.1 splits
/// them with wide margin on both sides (`ir-holder-detection` verification data:
/// Phoenix holder IR 0.023, Ektar fully-exposed film IR 0.587).
const IR_HOLDER_MAX_TRANSMISSION: f32 = 0.1;

/// Number of along-edge segments the IR holder mask splits each edge into. A
/// holder can occlude only *part* of an edge (a partially-covered edge splits into
/// holder vs film runs — e.g. Phoenix `933` right), so a single per-edge mean is
/// too coarse. ~24 segments give enough resolution to isolate a partial holder
/// while staying cheap and noise-robust (each segment pools many pixels).
const IR_HOLDER_SEGMENTS: u32 = 24;

/// Depth (perpendicular to the edge) the holder classifier probes inward, as a
/// fraction of the short dimension. The opaque holder occludes the film **from the
/// very edge inward**, so its darkness shows in a *shallow* near-edge band; probing
/// the whole rebate-scan window ([`REBATE_SCAN_FRAC`], ~10%) instead would dilute a
/// real holder band with the bright film sitting behind it and misread the edge as
/// film. On real HDRi scans (`ir-holder-detection` verification) a ~0.5% band
/// cleanly reads Phoenix `933` top/right as holder (near-edge IR ≈ 0.02) and
/// bottom/left as film (≈ 0.65), while the whole-window median washed the holder
/// out. Floored at a few pixels so tiny synthetic frames still probe a real band.
const IR_HOLDER_PROBE_FRAC: f32 = 0.005;

/// Minimum holder probe depth in pixels — floors [`IR_HOLDER_PROBE_FRAC`] so a
/// small image still samples more than a single noisy row.
const IR_HOLDER_PROBE_MIN: u32 = 2;

/// Shared recovery advice appended to every auto-detection refusal, naming the
/// fallback options. Kept in one place so the too-small and no-band errors stay
/// consistent. Content-based estimation (`--base-content`) is only *suggested*
/// here — it is owned by the separate `film-base/content-fallback` task and is
/// never a silent fallback (design-spec §9 ladder tier 3).
const RECOVERY_ADVICE: &str = "pass --film-base or --base-region (design-spec §9: measure once \
     from an unexposed reference and reuse it). For a cropped scan with no unexposed \
     film visible, content-based estimation is planned but not yet available (the \
     --base-content flag is owned by the film-base/content-fallback task); until it \
     ships, use --film-base or --base-region";

/// Fraction of the grid rectangle's width/height used for each grid cell, so the
/// five cells cover the corners and center with clear gaps between them.
const GRID_CELL_FRAC: f32 = 0.25;

/// Max acceptable per-channel relative spread (`(max - min) / max`) across the
/// grid cells for them to count as agreeing. An unexposed reference frame is
/// physically uniform base, so cells should match to within a few percent;
/// larger spread indicates light leaks, scanner illumination falloff, or dust —
/// a diagnostic the caller must surface loudly, not average away.
pub const GRID_MAX_RELATIVE_SPREAD: f32 = 0.05;

/// A resolved film base plus any non-fatal quality warnings the estimation
/// raised (e.g. a non-uniform `--base-region`, cross-edge disagreement). The
/// orchestrator folds the warnings into the JSON report, where `--strict`
/// promotes them — the value itself is never silently altered.
#[derive(Clone, Debug, PartialEq)]
pub struct BaseEstimate {
    pub base: FilmBase,
    pub warnings: Vec<String>,
}

impl BaseEstimate {
    /// An estimate with no warnings attached.
    fn clean(base: FilmBase) -> Self {
        Self {
            base,
            warnings: Vec::new(),
        }
    }
}

/// Which image edge a rebate candidate was found on. Serializes lowercase into
/// the `inspect` report.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Edge {
    Top,
    Bottom,
    Left,
    Right,
}

/// A candidate unexposed-rebate band found by the inward-scan detector: a
/// uniform, holder-backed strip run on one edge. **Brightness relative to the
/// frame is not gated here** — that check lives in [`select_auto_base`], so a
/// candidate darker than the interior can still be listed (and `nc inspect`
/// reports candidates even when selection then refuses). Reported so a user (or
/// a future UI) can confirm a region instead of measuring one — `region` drops
/// directly into `--base-region`.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RebateCandidate {
    /// The edge the band was found on.
    pub edge: Edge,
    /// The band rectangle `[x, y, w, h]`, usable verbatim as `--base-region`.
    pub region: [u32; 4],
    /// Per-channel high-percentile transmission over the band — the base value
    /// this candidate proposes.
    pub base: [f32; 3],
    /// Worst per-channel relative spread over the band — the confidence figure
    /// (lower is more uniform; gated at [`MAX_RELATIVE_SPREAD`]).
    pub spread: f32,
}

/// Resolve the film base for `image` from the selected [`FilmBaseSource`]:
/// return the explicit override, sample the given region, auto-detect the
/// unexposed rebate. Region bounds and auto-detection confidence are checked
/// here (the image isn't available at the CLI boundary), failing loudly rather
/// than returning a silently-wrong anchor.
///
/// Whatever the source, the resolved base is guaranteed **finite and positive
/// on every channel** ([`guard_base`]) before it is returned — the base anchors
/// the density divide `D = -log10(scan / base)`, so a zero / negative /
/// non-finite channel is unusable and errors loudly here rather than poisoning
/// the render (or, worse, being printed by `nc estimate` as a trustworthy Dmin
/// the user bakes into a recipe). This is the "reject degenerate bases at birth"
/// guard the film-base gotcha in `CLAUDE.md` called for; the per-algo guards in
/// `algo/*` remain as defense-in-depth.
/// Takes an already-**resolved** [`FilmBaseSource`], not the params object: since
/// `film_base.source` has no default, "unset" is an orchestration state the CLI
/// resolves (reject for `convert`/`roll`, `Auto` for the measurement commands),
/// and a pure stage should only ever receive a decision.
pub fn estimate(
    image: &LinearImage,
    source: &FilmBaseSource,
    film_type: FilmType,
) -> Result<BaseEstimate> {
    let est = match *source {
        FilmBaseSource::Explicit(rgb) => BaseEstimate::clean(FilmBase::from(rgb)),
        FilmBaseSource::Region(rect) => sample_region(image, rect)?,
        FilmBaseSource::Auto => {
            let candidates = rebate_candidates(image, film_type)?;
            select_auto_base(image, &candidates)?
        }
    };
    guard_base(&est.base, source)?;
    Ok(est)
}

/// Error loudly if any channel of a resolved base is non-finite or `<= 0` — such
/// a base cannot anchor the density divide. The message names the source so a
/// caller knows which knob produced the degenerate value and how to recover.
fn guard_base(base: &FilmBase, source: &FilmBaseSource) -> Result<()> {
    let rgb = <[f32; 3]>::from(*base);
    if rgb.iter().all(|v| v.is_finite() && *v > 0.0) {
        return Ok(());
    }
    let advice = match source {
        // A degenerate region base means the sampled pixels had no usable signal
        // on some channel (e.g. a region on the dark holder).
        FilmBaseSource::Region(_) => {
            "the sampled region has no usable signal on some channel (e.g. it sits on \
             the dark holder) — sample a brighter rebate patch or pass --film-base"
        }
        // Auto's brightness gate guarantees positivity, so this is unreachable
        // in practice; keep the message consistent with the other refusals.
        FilmBaseSource::Auto => RECOVERY_ADVICE,
        // Explicit is CLI-validated before it ever reaches here.
        FilmBaseSource::Explicit(_) => "pass a --film-base transmission in (0, 1]",
    };
    Err(NcError::Other(format!(
        "resolved film base {rgb:?} is not finite and positive on every channel; \
         it cannot anchor the density divide — {advice}"
    )))
}

/// Per-channel high-percentile transmission over the rectangle `[x, y, w, h]`,
/// plus a uniformity warning when the rectangle is not flat (per-channel spread
/// above [`MAX_RELATIVE_SPREAD`] on any channel). A mixed rebate/image rectangle
/// otherwise yields a plausible-looking bad base with no signal; the warning —
/// not an error, since a human may legitimately sample an odd patch — surfaces
/// it in the report, and `--strict` can refuse it. The sampled value itself is
/// unchanged by the check.
fn sample_region(image: &LinearImage, rect: [u32; 4]) -> Result<BaseEstimate> {
    let mut chans = region_channels(image, rect)?;
    let (hi, spread) = channel_stats(&mut chans);
    let mut est = BaseEstimate::clean(FilmBase::from(hi));
    if spread > MAX_RELATIVE_SPREAD {
        let [x, y, w, h] = rect;
        est.warnings.push(format!(
            "base-region [{x},{y},{w},{h}] is not uniform (worst per-channel relative \
             spread {spread:.2} > {MAX_RELATIVE_SPREAD:.2}); the rectangle may mix \
             unexposed rebate with image content — verify it with `nc inspect`"
        ));
    }
    Ok(est)
}

/// Scan all four edges for unexposed-rebate candidates: on each edge, march
/// 1-px strips inward (up to [`REBATE_SCAN_FRAC`] of the short dimension) and
/// keep the first uniform, value-continuous band that sits **behind** a
/// contiguous dark-holder run. Strips are trimmed by the scan depth at both
/// ends so the perpendicular edges' holder margins can't contaminate them.
/// Returns at most one candidate per edge (or per contiguous IR film run when the
/// edge is masked); an empty result means no confident band exists anywhere.
/// Candidates are **not** transmission-gated here — that is
/// [`select_auto_base`]'s job, which must be called on the **same image** these
/// candidates came from (it recomputes the scan depth and interior median from
/// it). Errors only when the image is too small to scan.
///
/// When the film is chromogenic and the scan carries an IR plane, each edge's
/// inward scan is restricted to its **film** segments (the IR holder mask) — the
/// opaque holder's segments are excluded so their pixels never contaminate the
/// rebate search (`ir-holder-detection`), and a partially-covered edge contributes
/// one candidate per contiguous film run. With no usable IR mask (silver /
/// `unknown` film, or no IR plane) every edge is scanned over its full trimmed
/// extent exactly as before (byte-identical). [`select_auto_base`] ranks whatever
/// candidates result.
pub fn rebate_candidates(image: &LinearImage, film_type: FilmType) -> Result<Vec<RebateCandidate>> {
    let cap = scan_depth(image)?;
    let mask = ir_holder_mask(image, film_type)?;
    let mut found = Vec::new();
    for edge in [Edge::Top, Edge::Bottom, Edge::Left, Edge::Right] {
        for (lo, hi) in film_along_ranges(mask.as_deref(), edge, image, cap) {
            if let Some(c) = edge_candidate(image, edge, cap, lo, hi)? {
                found.push(c);
            }
        }
    }
    Ok(found)
}

/// Pick the film base from the detector's candidates: filter to bands with
/// higher transmission than the frame-interior median by
/// [`INTERIOR_BRIGHTNESS_MARGIN`] on **every** channel (the rebate is per-channel
/// `Dmin` = maximum transmission), then take the highest-transmission survivor
/// — nothing genuine can out-transmit clean base, so a uniform low-transmission
/// picture band can never out-rank a real rebate. Disagreement between any two
/// surviving candidates — across *or* within an edge (one edge can yield several,
/// one per IR film run) — beyond [`CROSS_EDGE_AGREE_TOL`] is surfaced as a warning
/// rather than silently ignored. Fails loudly, naming every recovery flag, when no candidate
/// survives.
///
/// `candidates` **must** have been produced by [`rebate_candidates`] on this
/// same `image`: the scan depth and interior median are recomputed from `image`
/// here, so candidates from a different image would be measured against the
/// wrong interior.
pub fn select_auto_base(
    image: &LinearImage,
    candidates: &[RebateCandidate],
) -> Result<BaseEstimate> {
    if candidates.is_empty() {
        return Err(NcError::Other(format!(
            "auto film-base detection found no uniform unexposed rebate band behind \
             the film holder on any edge; {RECOVERY_ADVICE}"
        )));
    }

    let cap = scan_depth(image)?;
    let (w, h) = (image.width, image.height);
    let interior = sample_region_at(image, [cap, cap, w - 2 * cap, h - 2 * cap], 0.5)?;
    let interior = <[f32; 3]>::from(interior);
    let survivors: Vec<&RebateCandidate> = candidates
        .iter()
        .filter(|c| {
            c.base
                .iter()
                .zip(interior)
                .all(|(&b, i)| b > i * INTERIOR_BRIGHTNESS_MARGIN)
        })
        .collect();
    let Some(best) = survivors
        .iter()
        .copied()
        // Strictly-greater keeps the first (fixed edge order) on ties, so the
        // choice is deterministic.
        .fold(None::<&RebateCandidate>, |best, c| match best {
            Some(b) if mean(&c.base) <= mean(&b.base) => Some(b),
            _ => Some(c),
        })
    else {
        return Err(NcError::Other(format!(
            "auto film-base detection found candidate band(s) but none with higher \
             transmission than the frame interior on every channel (the unexposed \
             rebate is per-channel minimum density, i.e. maximum transmission); \
             {RECOVERY_ADVICE}"
        )));
    };

    let mut est = BaseEstimate::clean(FilmBase::from(best.base));
    // Compare the chosen base against every *other* surviving candidate — excluded
    // by identity (pointer), not by edge. Since `ir-holder-detection` a single edge
    // can now yield multiple candidates (one per IR film run), and two materially
    // different bases from the same edge's runs are just as ambiguous as a
    // cross-edge disagreement; the old `other.edge != best.edge` filter silently
    // dropped them. Skipping only `best` itself keeps same-edge siblings in view.
    for &other in survivors.iter().filter(|&&c| !std::ptr::eq(c, best)) {
        let diff = best
            .base
            .iter()
            .zip(other.base)
            .map(|(&a, b)| (a - b).abs() / a.max(f32::MIN_POSITIVE))
            .fold(0.0f32, f32::max);
        if diff > CROSS_EDGE_AGREE_TOL {
            est.warnings.push(format!(
                "auto film-base candidates disagree: chose {:?} {:?} (region {:?}) but \
                 {:?} {:?} (region {:?}) reads a relative difference {diff:.2} > \
                 {CROSS_EDGE_AGREE_TOL:.2}; verify with `nc inspect` / --base-region",
                best.edge, best.base, best.region, other.edge, other.base, other.region
            ));
        }
    }
    Ok(est)
}

/// Mean of the three channels — the mean transmission used to rank candidates.
fn mean(rgb: &[f32; 3]) -> f32 {
    (rgb[0] + rgb[1] + rgb[2]) / 3.0
}

/// The inward scan depth (and strip end-trim) for a `width`x`height` frame:
/// [`REBATE_SCAN_FRAC`] of the shorter dimension, at least deep enough for a
/// holder strip plus a minimal band. `None` when the frame can't fit the scan
/// plus an interior.
///
/// Dimensions-only (rather than `&LinearImage`) so the memory sizing model can
/// call it before a pixel exists — see [`auto_interior_pixels`].
fn scan_depth_for(width: u32, height: u32) -> Option<u32> {
    let cap =
        ((width.min(height) as f32 * REBATE_SCAN_FRAC).round() as u32).max(MIN_BAND_STRIPS + 1);
    (2 * cap < width && 2 * cap < height).then_some(cap)
}

/// [`scan_depth_for`] on an image, erroring loudly when it is too small to scan.
fn scan_depth(image: &LinearImage) -> Result<u32> {
    let (w, h) = (image.width, image.height);
    scan_depth_for(w, h).ok_or_else(|| {
        NcError::Other(format!(
            "image {w}x{h} is too small for auto film-base detection; \
             {RECOVERY_ADVICE}"
        ))
    })
}

/// Pixel count of the frame-interior rectangle [`select_auto_base`] materializes
/// when ranking candidates — the one full-frame-scale allocation the auto path
/// makes (`[cap, cap, w - 2*cap, h - 2*cap]`, ~69% of a 3:2 frame; the per-edge
/// bands it also samples are at most `cap` thick, so they never dominate).
///
/// Exists so `pipeline::memory` can size the film-base phase from the **same**
/// rule the sampler uses instead of a second copy of it that could drift. `0`
/// when the frame is too small to scan at all: auto detection then fails inside
/// [`rebate_candidates`] before any interior sample is gathered, so the model must
/// not invent an allocation (nor turn a too-small frame into a spurious preflight
/// rejection).
pub fn auto_interior_pixels(width: u32, height: u32) -> u64 {
    match scan_depth_for(width, height) {
        Some(cap) => (width - 2 * cap) as u64 * (height - 2 * cap) as u64,
        None => 0,
    }
}

/// Pixel count of **one** [`estimate_grid`] cell over a `w`x`h` rectangle —
/// [`GRID_CELL_FRAC`] per axis, so ~6.25% of the rectangle.
///
/// One cell, not five: `estimate_grid` samples the cells **sequentially** and each
/// `sample_region_at` drops its channel vectors before the next is gathered, so
/// only one is ever live. Exposed for the same reason as
/// [`auto_interior_pixels`] — `pipeline::memory` must size the grid path from the
/// sampler's own cell rule rather than a second copy of it.
pub fn grid_cell_pixels(w: u32, h: u32) -> u64 {
    let cw = ((w as f32 * GRID_CELL_FRAC).round() as u32).clamp(1, w.max(1));
    let ch = ((h as f32 * GRID_CELL_FRAC).round() as u32).clamp(1, h.max(1));
    cw as u64 * ch as u64
}

/// What one inward strip looks like to the detector.
#[derive(Clone, Copy, Debug, PartialEq)]
enum StripClass {
    /// Very dark on every channel: the film holder.
    Holder,
    /// Near-uniform along the strip on every channel (and not holder): a
    /// potential slice of unexposed rebate. Carries the per-channel high
    /// percentile.
    Uniform([f32; 3]),
    /// Anything else — varying picture content.
    Other,
}

/// The 1-px strip rectangle at `depth` pixels in from `edge`, spanning the
/// along-edge range `[along_lo, along_hi)`. The range is trimmed by `cap` at both
/// ends for the full-edge scan (the corners belong to the perpendicular edges'
/// holder), or narrowed to one IR film run for a partially-occluded edge.
fn strip_rect(
    image: &LinearImage,
    edge: Edge,
    depth: u32,
    along_lo: u32,
    along_hi: u32,
) -> [u32; 4] {
    let (w, h) = (image.width, image.height);
    let along = along_hi - along_lo;
    match edge {
        Edge::Top => [along_lo, depth, along, 1],
        Edge::Bottom => [along_lo, h - 1 - depth, along, 1],
        Edge::Left => [depth, along_lo, 1, along],
        Edge::Right => [w - 1 - depth, along_lo, 1, along],
    }
}

/// The band rectangle covering strip depths `[start, end)` on `edge`, spanning
/// the along-edge range `[along_lo, along_hi)`.
fn band_rect(
    image: &LinearImage,
    edge: Edge,
    start: u32,
    end: u32,
    along_lo: u32,
    along_hi: u32,
) -> [u32; 4] {
    let (w, h) = (image.width, image.height);
    let along = along_hi - along_lo;
    let thick = end - start;
    match edge {
        Edge::Top => [along_lo, start, along, thick],
        Edge::Bottom => [along_lo, h - end, along, thick],
        Edge::Left => [start, along_lo, thick, along],
        Edge::Right => [w - end, along_lo, thick, along],
    }
}

/// Classify the strip at `depth` in from `edge` over the along-edge range
/// `[along_lo, along_hi)`.
fn classify_strip(
    image: &LinearImage,
    edge: Edge,
    depth: u32,
    along_lo: u32,
    along_hi: u32,
) -> Result<StripClass> {
    let mut chans = region_channels(image, strip_rect(image, edge, depth, along_lo, along_hi))?;
    let (hi, spread) = channel_stats(&mut chans);
    if hi.iter().all(|&v| v < HOLDER_MAX_TRANSMISSION) {
        Ok(StripClass::Holder)
    } else if spread <= MAX_RELATIVE_SPREAD {
        Ok(StripClass::Uniform(hi))
    } else {
        Ok(StripClass::Other)
    }
}

/// Find the rebate candidate on one edge over the along-edge range
/// `[along_lo, along_hi)`, if any: a contiguous holder run at the very edge, then
/// a run of uniform, value-continuous strips at least [`MIN_BAND_STRIPS`] thick.
/// The whole band is then re-measured as one region and must itself pass the
/// uniformity gate (defense against a slow drift the per-strip checks can't see).
/// A high-transmission band **at** the edge (no holder outside it) is rejected —
/// that is the bright-surround false positive, or a crop with no holder, and both
/// belong to `--base-region`, not auto. The along-edge range is the full trimmed
/// extent for an ordinary scan, or one IR film run when the holder occludes only
/// part of the edge.
fn edge_candidate(
    image: &LinearImage,
    edge: Edge,
    cap: u32,
    along_lo: u32,
    along_hi: u32,
) -> Result<Option<RebateCandidate>> {
    // Contiguous holder run from depth 0.
    let mut depth = 0;
    while depth < cap
        && classify_strip(image, edge, depth, along_lo, along_hi)? == StripClass::Holder
    {
        depth += 1;
    }
    if depth == 0 || depth >= cap {
        return Ok(None); // no holder at the edge, or holder all the way down
    }

    // Uniform, value-continuous band immediately behind the holder.
    let start = depth;
    let mut prev: Option<[f32; 3]> = None;
    while depth < cap {
        let StripClass::Uniform(hi) = classify_strip(image, edge, depth, along_lo, along_hi)?
        else {
            break;
        };
        if let Some(p) = prev {
            let step = hi
                .iter()
                .zip(p)
                .map(|(&a, b)| (a - b).abs() / b.max(f32::MIN_POSITIVE))
                .fold(0.0f32, f32::max);
            if step > STRIP_CONTINUITY_TOL {
                break; // value jump: an adjacent flat region, not more rebate
            }
        }
        prev = Some(hi);
        depth += 1;
    }
    // A genuine thin rebate transitions into picture within the scan window. A
    // uniform run that reaches the scan cap without ever hitting picture is far
    // more likely uniform scene content (sky / wall) sitting behind the holder —
    // refuse it rather than anchor the roll on a guess. Auto must fail loudly when
    // there is no confident *thin* rebate; the user can still `--base-region` it.
    if depth == cap {
        return Ok(None);
    }
    if depth - start < MIN_BAND_STRIPS {
        return Ok(None);
    }

    // Re-measure the band as one region; the whole band must be uniform too.
    let region = band_rect(image, edge, start, depth, along_lo, along_hi);
    let mut chans = region_channels(image, region)?;
    let (base, spread) = channel_stats(&mut chans);
    if spread > MAX_RELATIVE_SPREAD {
        return Ok(None);
    }
    Ok(Some(RebateCandidate {
        edge,
        region,
        base,
        spread,
    }))
}

// ---------------------------------------------------------------------------
// IR film-holder mask (chromogenic only) — the `ir-holder-detection` masking
// pre-step that feeds the RGB rebate search above.
// ---------------------------------------------------------------------------
//
// Chromogenic dye film is transparent to infrared, so all film (base, rebate,
// picture, even fully-exposed leader) reads bright in IR while the opaque scanner
// holder reads dark — a content-independent holder signal RGB cannot produce
// (holder and dense film are both dark in RGB). This mask classifies each edge's
// along-edge segments as holder vs film from the IR plane, and `rebate_candidates`
// runs the RGB inward scan only over the film runs, so a partially-occluded edge
// contributes only its film part and holder pixels never enter the rebate search.
//
// **Second consumer (follow-up, not wired here):** the same mask is what
// `bw-support` (PR #21, finding 4) needs to exclude the dark holder/dust border
// from the auto-`Dmax` anchor statistics (an uncropped holder can capture the
// 99.5th-percentile anchor and dim the render). That reuse is left for the
// auto-`Dmax` work; here the mask only feeds the film-base search.

/// IR-based holder classification of one along-edge segment.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum HolderClass {
    /// Opaque scanner holder (dark in IR) — occludes the film; excluded from the
    /// rebate search.
    Holder,
    /// Actual film (bright in IR: base, rebate, picture, or leader) — searched for
    /// the unexposed rebate.
    Film,
}

/// One along-edge segment of the IR holder mask: the along-edge pixel span it
/// covers, its holder/film class, and the representative IR transmission behind
/// the class. Serialized into `nc inspect` so a user can see which parts of which
/// edges the holder occludes.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct HolderSegment {
    /// Along-edge pixel range `[start, end)` — columns for top/bottom, rows for
    /// left/right.
    pub span: [u32; 2],
    /// Holder (dark IR) or film (bright IR).
    pub class: HolderClass,
    /// Median IR transmission over the segment's near-edge probe band — the value
    /// classified against [`IR_HOLDER_MAX_TRANSMISSION`].
    pub ir: f32,
}

/// The IR film-holder classification of one edge: its along-edge segments in
/// order. A fully-film or fully-holder edge is the degenerate all-segments-agree
/// case (every segment the same class).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct EdgeHolderMask {
    pub edge: Edge,
    pub segments: Vec<HolderSegment>,
}

/// Build the IR film-holder mask, or `None` when the IR path does not apply.
///
/// Produced **only** when the film is chromogenic ([`FilmType::ir_transparent`] —
/// silver B&W blocks IR and would misread dense silver as holder), the scan
/// actually carries an IR plane (HDR 48-bit has none), **and** that IR plane is
/// marker-verified ([`LinearImage::ir_verified`] — a shape-only grayscale page must
/// not be thresholded as IR, or a stray page could corrupt the base). All three
/// gate the path per design-spec §6.1 and the `ir-holder-detection` task. Every
/// other input returns `None` and the caller falls back to the RGB-only rebate
/// search. Pure over the decoded IR plane.
///
/// Per edge, the along-edge extent is split into [`IR_HOLDER_SEGMENTS`] segments;
/// each segment's **shallow** near-edge probe band (depth
/// `0..holder_probe_depth`, [`IR_HOLDER_PROBE_FRAC`]) is reduced to its median IR
/// transmission and classified holder (dark) or film (bright) against
/// [`IR_HOLDER_MAX_TRANSMISSION`]. Segmenting *along* the edge — not one per-edge
/// mean — is what lets a partially-covered edge split into holder vs film runs.
/// (`scan_depth` is still consulted so the feature refuses too-small images the
/// same way the rebate search does.)
pub fn ir_holder_mask(
    image: &LinearImage,
    film_type: FilmType,
) -> Result<Option<Vec<EdgeHolderMask>>> {
    let Some(ir) = image.ir.as_deref() else {
        return Ok(None);
    };
    if !film_type.ir_transparent() {
        return Ok(None);
    }
    // Trust the IR plane only when its provenance is marker-verified. A shape-only
    // grayscale page (accepted by the decoder as IR by shape, with a warning) must
    // not be thresholded as IR — that could corrupt the film base — so fall back to
    // the RGB-only search. The orchestrator emits the user-facing note.
    if !image.ir_verified {
        return Ok(None);
    }
    scan_depth(image)?; // same too-small guard as the rebate search
    let probe = holder_probe_depth(image);
    let mut masks = Vec::with_capacity(4);
    for edge in [Edge::Top, Edge::Bottom, Edge::Left, Edge::Right] {
        masks.push(EdgeHolderMask {
            edge,
            segments: edge_holder_segments(image, ir, edge, probe),
        });
    }
    Ok(Some(masks))
}

/// The shallow near-edge depth the holder classifier probes:
/// [`IR_HOLDER_PROBE_FRAC`] of the short dimension, floored at
/// [`IR_HOLDER_PROBE_MIN`]. Shallow on purpose — see [`IR_HOLDER_PROBE_FRAC`].
fn holder_probe_depth(image: &LinearImage) -> u32 {
    ((image.width.min(image.height) as f32 * IR_HOLDER_PROBE_FRAC).round() as u32)
        .max(IR_HOLDER_PROBE_MIN)
}

/// The along-edge length of `edge` (columns for top/bottom, rows for left/right).
fn along_len(image: &LinearImage, edge: Edge) -> u32 {
    match edge {
        Edge::Top | Edge::Bottom => image.width,
        Edge::Left | Edge::Right => image.height,
    }
}

/// Classify each along-edge segment of `edge` as holder or film from the IR plane.
/// The along-edge extent is divided into [`IR_HOLDER_SEGMENTS`] roughly-equal
/// segments; each is classified by the median IR over its shallow near-edge probe
/// band (depth `0..probe`).
fn edge_holder_segments(
    image: &LinearImage,
    ir: &[f32],
    edge: Edge,
    probe: u32,
) -> Vec<HolderSegment> {
    let along = along_len(image, edge);
    // Segment width rounds down but is at least 1 px; because the width is floored,
    // an edge whose length isn't a multiple of it ends in a smaller leftover
    // segment (`[start, along)`, narrower than the rest) rather than the last one
    // growing — so the whole edge is still covered.
    let seg = (along / IR_HOLDER_SEGMENTS).max(1);
    let mut segments = Vec::new();
    let mut start = 0u32;
    while start < along {
        let end = (start + seg).min(along);
        let ir_med = median_ir_probe(image, ir, edge, start, end, probe);
        let class = if ir_med <= IR_HOLDER_MAX_TRANSMISSION {
            HolderClass::Holder
        } else {
            HolderClass::Film
        };
        segments.push(HolderSegment {
            span: [start, end],
            class,
            ir: ir_med,
        });
        start = end;
    }
    segments
}

/// Median IR transmission over the near-edge probe band of one along-edge segment:
/// the `probe`-deep strip from `edge` inward, spanning along-edge `[along_lo,
/// along_hi)`. The median resists dust sparkles / hot IR pixels while cleanly
/// separating the uniformly dark holder from bright film.
fn median_ir_probe(
    image: &LinearImage,
    ir: &[f32],
    edge: Edge,
    along_lo: u32,
    along_hi: u32,
    probe: u32,
) -> f32 {
    let (w, h) = (image.width, image.height);
    // The probe band: `probe` deep from the edge, spanning the segment along-edge.
    let [x, y, rw, rh] = match edge {
        Edge::Top => [along_lo, 0, along_hi - along_lo, probe],
        Edge::Bottom => [along_lo, h - probe, along_hi - along_lo, probe],
        Edge::Left => [0, along_lo, probe, along_hi - along_lo],
        Edge::Right => [w - probe, along_lo, probe, along_hi - along_lo],
    };
    let mut vals = Vec::with_capacity((rw as usize) * (rh as usize));
    for row in y..y + rh {
        let row_start = row as usize * w as usize;
        for col in x..x + rw {
            vals.push(ir[row_start + col as usize]);
        }
    }
    percentile(&mut vals, 0.5)
}

/// The along-edge pixel ranges to run the rebate inward-scan over on `edge`,
/// trimmed to the scan window's along-edge extent `[cap, along-cap)`. Without a
/// holder mask this is the single full trimmed extent (the RGB-only path,
/// unchanged); with a mask it is one range per contiguous run of **film**
/// segments — holder runs are excluded so their pixels never enter the rebate
/// search, and a partially-covered edge contributes only its film runs.
fn film_along_ranges(
    mask: Option<&[EdgeHolderMask]>,
    edge: Edge,
    image: &LinearImage,
    cap: u32,
) -> Vec<(u32, u32)> {
    // `scan_depth` guarantees `along > 2*cap`, so the trimmed extent is non-empty.
    let (trim_lo, trim_hi) = (cap, along_len(image, edge) - cap);
    let Some(edge_mask) = mask.and_then(|m| m.iter().find(|m| m.edge == edge)) else {
        return vec![(trim_lo, trim_hi)];
    };

    // Merge contiguous film segments into runs, then clip each to the trimmed
    // extent. A run fully outside `[trim_lo, trim_hi)`, or clipped empty, is
    // dropped (its along-edge span is entirely in the corner trim).
    let mut ranges = Vec::new();
    let mut run: Option<(u32, u32)> = None;
    let flush = |run: &mut Option<(u32, u32)>, ranges: &mut Vec<(u32, u32)>| {
        if let Some((lo, hi)) = run.take() {
            let (lo, hi) = (lo.max(trim_lo), hi.min(trim_hi));
            if lo < hi {
                ranges.push((lo, hi));
            }
        }
    };
    for s in &edge_mask.segments {
        match s.class {
            HolderClass::Film => match &mut run {
                Some((_, end)) => *end = s.span[1],
                None => run = Some((s.span[0], s.span[1])),
            },
            HolderClass::Holder => flush(&mut run, &mut ranges),
        }
    }
    flush(&mut run, &mut ranges);
    ranges
}

/// Per-channel high percentile and the worst per-channel relative spread
/// `(p_hi - p_lo) / p_hi` over gathered channel samples. A zero/negative high
/// percentile yields spread 1.0 (maximally non-uniform) so degenerate data can
/// never look confident.
fn channel_stats(chans: &mut [Vec<f32>; 3]) -> ([f32; 3], f32) {
    let mut hi = [0.0f32; 3];
    let mut spread = 0.0f32;
    for (c, samples) in chans.iter_mut().enumerate() {
        let h = percentile(samples, SAMPLE_PERCENTILE);
        let l = percentile(samples, LOW_PERCENTILE);
        hi[c] = h;
        spread = spread.max(if h > 0.0 { (h - l) / h } else { 1.0 });
    }
    (hi, spread)
}

/// Gather the rectangle `[x, y, w, h]` into per-channel sample vectors. The
/// rectangle must lie within the image; an out-of-bounds or empty region is a
/// usage error rather than a clamp, so a bad `--base-region` fails loudly.
fn region_channels(image: &LinearImage, [x, y, w, h]: [u32; 4]) -> Result<[Vec<f32>; 3]> {
    if w == 0 || h == 0 {
        return Err(NcError::Usage(format!(
            "base-region must be non-empty (got {w}x{h})"
        )));
    }
    // Use u64 for the right edge so a region near u32::MAX can't wrap.
    let (right, bottom) = (x as u64 + w as u64, y as u64 + h as u64);
    if right > image.width as u64 || bottom > image.height as u64 {
        return Err(NcError::Usage(format!(
            "base-region [{x},{y},{w},{h}] is outside the {}x{} image",
            image.width, image.height
        )));
    }

    let mut chans: [Vec<f32>; 3] = [Vec::new(), Vec::new(), Vec::new()];
    let cap = (w as usize) * (h as usize);
    for c in &mut chans {
        c.reserve(cap);
    }
    for row in y..y + h {
        let row_start = (row as usize * image.width as usize + x as usize) * 3;
        for col in 0..w as usize {
            let i = row_start + col * 3;
            chans[0].push(image.rgb[i]);
            chans[1].push(image.rgb[i + 1]);
            chans[2].push(image.rgb[i + 2]);
        }
    }
    Ok(chans)
}

/// Per-channel `p`-quantile transmission over the rectangle `[x, y, w, h]`.
///
/// `pub(crate)` so the roll-fixed `Dmax` reference measurement
/// (`cli::run_estimate` → `algo::density::reference_dmax`) can sample the
/// **median** (`p = 0.5`) transmission of a fully-exposed reference region: unlike
/// the film base (which wants the region's *maximum* transmission, a high
/// percentile), the `Dmax` reference wants its *typical* transmission, and the
/// median is robust to dust/hot pixels without a uniformity gate — relative spread
/// on near-opaque (near-zero) transmissions is dominated by sensor noise and would
/// false-alarm, so the median's outlier-resistance is the right guard here.
pub(crate) fn sample_region_at(image: &LinearImage, rect: [u32; 4], p: f32) -> Result<FilmBase> {
    let mut chans = region_channels(image, rect)?;
    Ok(FilmBase {
        r: percentile(&mut chans[0], p),
        g: percentile(&mut chans[1], p),
        b: percentile(&mut chans[2], p),
    })
}

// ---------------------------------------------------------------------------
// Grid / multi-region sampling (unexposed-frame calibration, design-spec §9
// ladder tier 1)
// ---------------------------------------------------------------------------

/// One grid cell: the rectangle sampled and the base it measured. Serialized
/// into the JSON report so a disagreement is diagnosable per cell.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct GridCell {
    /// The sampled rectangle `[x, y, w, h]`.
    pub region: [u32; 4],
    /// Per-channel high-percentile transmission of this cell.
    pub base: FilmBase,
}

/// Result of grid sampling: the combined base plus the per-cell values and
/// their spread, so agreement failure can be reported *with* the evidence
/// rather than averaged away. Serialize-only — it feeds the JSON report.
///
/// `base`, `spread`, `tolerance`, and `agreement` are all **derived from
/// `cells`**; construct only via [`estimate_grid`] so they stay consistent.
///
/// **Known limitation — `agreement: bool` conflates two conditions.** A `false`
/// verdict means either the cells genuinely *disagree* (light leak / scanner
/// illumination falloff / dust) or the sample is *degenerate* (all-zero / dark,
/// e.g. a region on the holder). It can't tell which, because the `spread`
/// sentinel is overloaded: a degenerate all-zero channel and a genuine full-range
/// disagreement both read ~`1.0`. The CLI (`cli::run_estimate`) therefore
/// re-derives which case it is by re-inspecting the combined `base` (channel
/// `<= 0` ⇒ degenerate ⇒ hard error; otherwise disagreement ⇒ warning). Replacing
/// this bool + overloaded sentinel with a self-describing verdict, so the
/// estimate reports its own outcome and the CLI stops re-deriving it, is carried
/// by the `film-base/tiling-uniformity-validator` follow-up — which retires
/// `--grid` and this struct along with it.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct GridEstimate {
    /// Combined base: the per-channel **median** across cells (robust to one
    /// bad cell — e.g. a dust patch — while staying deterministic).
    pub base: FilmBase,
    /// The five sampled cells, in fixed order: top-left, top-right,
    /// bottom-left, bottom-right, center.
    pub cells: [GridCell; 5],
    /// Per-channel relative spread across cells, `(max - min) / max`
    /// (`1.0` when the max is non-positive — a degenerate sample,
    /// indistinguishable from a genuine full-range spread by this field
    /// alone; the combined `base` disambiguates).
    pub spread: [f32; 3],
    /// The documented agreement tolerance ([`GRID_MAX_RELATIVE_SPREAD`]) the
    /// spread was judged against, echoed so the report is self-contained.
    pub tolerance: f32,
    /// Whether every channel's spread is within the tolerance. `false` is
    /// diagnostic — light leaks, illumination falloff, or dust.
    pub agreement: bool,
}

/// Sample a fixed 5-cell grid (corners + center) over `rect` and combine the
/// per-cell film-base measurements. For an unexposed reference frame the whole
/// rectangle is clean base, so the cells double as an agreement check: their
/// spread diagnoses light leaks and scanner illumination falloff (reported, and
/// judged against [`GRID_MAX_RELATIVE_SPREAD`] — the caller surfaces failure
/// loudly). Deterministic: fixed layout ([`GRID_CELL_FRAC`] of the rectangle
/// per cell), fixed percentile ([`SAMPLE_PERCENTILE`]).
pub fn estimate_grid(image: &LinearImage, rect: [u32; 4]) -> Result<GridEstimate> {
    let [x, y, w, h] = rect;
    // Validate the whole rectangle up front so a bad `--base-region` reports
    // itself, not a derived cell. (Empty / out-of-bounds checks match
    // `sample_region_at`; the u64 arithmetic prevents wrap near u32::MAX.)
    if w == 0 || h == 0 {
        return Err(NcError::Usage(format!(
            "grid region must be non-empty (got {w}x{h})"
        )));
    }
    if x as u64 + w as u64 > image.width as u64 || y as u64 + h as u64 > image.height as u64 {
        return Err(NcError::Usage(format!(
            "grid region [{x},{y},{w},{h}] is outside the {}x{} image",
            image.width, image.height
        )));
    }

    // Cell size: a fixed fraction of the rectangle, at least 1 px. On a tiny
    // rectangle the cells overlap; that is harmless and still deterministic.
    let cw = ((w as f32 * GRID_CELL_FRAC).round() as u32).clamp(1, w);
    let ch = ((h as f32 * GRID_CELL_FRAC).round() as u32).clamp(1, h);
    let origins = [
        (x, y),                               // top-left
        (x + w - cw, y),                      // top-right
        (x, y + h - ch),                      // bottom-left
        (x + w - cw, y + h - ch),             // bottom-right
        (x + (w - cw) / 2, y + (h - ch) / 2), // center
    ];

    let mut sampled = Vec::with_capacity(origins.len());
    for (cx, cy) in origins {
        let region = [cx, cy, cw, ch];
        sampled.push(GridCell {
            region,
            base: sample_region_at(image, region, SAMPLE_PERCENTILE)?,
        });
    }
    // Infallible: one cell per origin, and `origins` is a 5-element array.
    let cells: [GridCell; 5] = sampled.try_into().expect("one grid cell per origin");

    // Per-channel median (combined value) and relative spread across cells.
    let mut base = [0.0f32; 3];
    let mut spread = [0.0f32; 3];
    for c in 0..3 {
        // Exactly `cells.len()` (== 5) values — a fixed-size stack array, no heap.
        let mut vals = [0.0f32; 5];
        for (i, cell) in cells.iter().enumerate() {
            vals[i] = <[f32; 3]>::from(cell.base)[c];
        }
        vals.sort_by(f32::total_cmp);
        base[c] = vals[vals.len() / 2];
        let (lo, hi) = (vals[0], vals[vals.len() - 1]);
        spread[c] = if hi > 0.0 { (hi - lo) / hi } else { 1.0 };
    }
    let agreement = spread.iter().all(|s| *s <= GRID_MAX_RELATIVE_SPREAD);

    Ok(GridEstimate {
        base: FilmBase::from(base),
        cells,
        spread,
        tolerance: GRID_MAX_RELATIVE_SPREAD,
        agreement,
    })
}

/// The `p`-quantile (0.0–1.0) of `values` by rounded rank `round((n-1)·p)` over
/// the finite values, no interpolation, in O(n). (Not the textbook nearest-rank
/// `⌈p·n⌉`: for `[0.1,0.2,0.3,0.4]` at p=0.5 this returns `0.3`, not `0.2`.)
///
/// Non-finite samples (`NaN`, `±inf`) are dropped first, so a stray non-finite
/// pixel can never be returned as the base (which would poison the density
/// divide downstream); the rank is then an order statistic
/// (`select_nth_unstable_by` under the `f32::total_cmp` total order), whose
/// value is independent of tie order — deterministic by construction. Empty /
/// all-non-finite input yields `0.0`. In practice decoded samples are always
/// finite `[0, 1]`; this just makes the helper sound if reused.
fn percentile(values: &mut Vec<f32>, p: f32) -> f32 {
    values.retain(|v| v.is_finite());
    if values.is_empty() {
        return 0.0;
    }
    // f64 for the index: a region can exceed 2^24 samples (a 24 MP interior),
    // above which an `as f32` rank cast loses integer precision and would pick a
    // slightly wrong order statistic. f64 is exact here with no measurable cost.
    let k = ((values.len() - 1) as f64 * p.clamp(0.0, 1.0) as f64).round() as usize;
    *values.select_nth_unstable_by(k, |a, b| a.total_cmp(b)).1
}

/// The **frozen** synthetic scan the `pipeline_version` drift gate fingerprints
/// stage 2 over (`crate::version`).
///
/// `film_base.source` defaults to [`FilmBaseSource::Auto`], so every default
/// `nc convert` runs the inward-scan rebate detector over real pixels — a stage the
/// render fingerprint (which is handed a hardcoded base) cannot see, and the recipe
/// fingerprint sees only as the string `"auto"`. Retuning [`SAMPLE_PERCENTILE`],
/// [`REBATE_SCAN_FRAC`], or the band gates changes every default conversion, so it
/// needs a fingerprint of its own.
///
/// **Do not edit [`golden::scan`].** It is a frozen input, not a test convenience:
/// the gate hashes [`estimate`]'s result over it, so changing the pixels moves the
/// fingerprint exactly as changing the detector would. It is deliberately
/// self-contained rather than reusing `tests::scan_with_rebate`, which is a
/// *parameterized* helper free to evolve with the tests that use it.
///
/// **Why hashing this is safe on both macOS/aarch64 and x86_64 Linux** (CLAUDE.md's
/// cross-platform determinism rule, design-spec §8): the whole stage-2 path is
/// integer indexing, comparisons, `+`/`-`/`*`/`/` on IEEE floats, and nearest-rank
/// order-statistic selection ([`percentile`], whose result is the k-th smallest
/// *value* and so is independent of tie order). There is **no transcendental**
/// anywhere in this module — no `powf`, `10^`, `log10`, `exp`, or `sqrt` — which is
/// precisely why the ~1-ULP libm divergence that rules out a whole-frame
/// reconstruct hash does not apply here.
#[cfg(test)]
pub(crate) mod golden {
    use super::*;

    /// Measured rebate transmission of the user's real film stock, and a
    /// representative opaque-holder transmission.
    const REBATE: [f32; 3] = [0.53, 0.26, 0.16];
    const HOLDER: [f32; 3] = [0.01, 0.01, 0.01];

    /// The frozen 100×100 layout: dark holder ring (3 px) → thin unexposed rebate
    /// band (4 px, bottom **and** left, so cross-edge agreement is exercised) →
    /// varied gradient picture interior. Mirrors the real `dark holder → thin inset
    /// rebate → picture` geometry documented in CLAUDE.md.
    pub(crate) fn scan() -> LinearImage {
        let (w, h) = (100u32, 100u32);
        let mut buf = Vec::with_capacity((w * h * 3) as usize);
        for y in 0..h {
            for x in 0..w {
                let t = (x + y) as f32 / (w + h) as f32;
                buf.extend_from_slice(&[0.05 + 0.35 * t, 0.03 + 0.20 * t, 0.02 + 0.10 * t]);
            }
        }
        let mut img = LinearImage::new(w, h, buf, None).unwrap();
        for rect in [
            [0, 0, w, 3],
            [0, h - 3, w, 3],
            [0, 0, 3, h],
            [w - 3, 0, 3, h],
        ] {
            fill(&mut img, rect, HOLDER);
        }
        // Bands ripple **along** the edge (bottom by x, left by y), so every strip
        // perpendicular to the edge carries the same distribution — flat to the
        // strip-continuity check, textured to the percentile.
        for x in 0..w {
            for y in h - 7..h - 3 {
                set(&mut img, x, y, rebate_at(x));
            }
        }
        for y in 0..h {
            for x in 3..7 {
                set(&mut img, x, y, rebate_at(y));
            }
        }
        img
    }

    /// The rebate transmission at along-edge position `step`.
    ///
    /// The band is deliberately **not** flat. A perfectly uniform band returns the
    /// same value for *any* percentile, so retuning [`SAMPLE_PERCENTILE`] — one of
    /// the exact changes this fingerprint exists to catch — would leave the hash
    /// unmoved. A 7% along-edge ripple over ten levels keeps each strip well inside
    /// [`MAX_RELATIVE_SPREAD`] (0.15) and [`STRIP_CONTINUITY_TOL`] while putting the
    /// 97th percentile and, say, the 90th on different levels.
    fn rebate_at(step: u32) -> [f32; 3] {
        let f = 0.93 + 0.07 * (step % 10) as f32 / 9.0;
        [REBATE[0] * f, REBATE[1] * f, REBATE[2] * f]
    }

    fn fill(img: &mut LinearImage, [x, y, w, h]: [u32; 4], rgb: [f32; 3]) {
        for yy in y..y + h {
            for xx in x..x + w {
                set(img, xx, yy, rgb);
            }
        }
    }

    fn set(img: &mut LinearImage, x: u32, y: u32, rgb: [f32; 3]) {
        let i = ((y * img.width + x) * 3) as usize;
        img.rgb[i..i + 3].copy_from_slice(&rgb);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build an `w`x`h` image filled with a flat RGB color.
    fn solid(w: u32, h: u32, rgb: [f32; 3]) -> LinearImage {
        let mut buf = Vec::with_capacity((w * h * 3) as usize);
        for _ in 0..w * h {
            buf.extend_from_slice(&rgb);
        }
        LinearImage::new(w, h, buf, None).unwrap()
    }

    /// Set one pixel's RGB in place.
    fn set_px(img: &mut LinearImage, x: u32, y: u32, rgb: [f32; 3]) {
        let i = ((y * img.width + x) * 3) as usize;
        img.rgb[i..i + 3].copy_from_slice(&rgb);
    }

    /// Fill a rectangle with a flat RGB color.
    fn fill_rect(img: &mut LinearImage, [x, y, w, h]: [u32; 4], rgb: [f32; 3]) {
        for yy in y..y + h {
            for xx in x..x + w {
                set_px(img, xx, yy, rgb);
            }
        }
    }

    /// The measured rebate transmission of the user's real film stock
    /// (`48bit-full/1` bottom edge ≈ `48bit-full/2` left edge) — the value the
    /// synthetic layouts below are built around.
    const REBATE: [f32; 3] = [0.53, 0.26, 0.16];
    const HOLDER: [f32; 3] = [0.01, 0.01, 0.01];

    /// A synthetic real-scan layout: dark holder ring → thin unexposed rebate
    /// band on the given edges → varied (high-spread) picture interior.
    /// 100x100, scan depth cap = 10; holder is 3 px, the rebate 4 px (depths
    /// 3..7).
    fn scan_with_rebate(edges: &[Edge]) -> LinearImage {
        let (w, h) = (100u32, 100u32);
        // Varied picture interior: a diagonal gradient, darker than the rebate,
        // spread far beyond the uniformity gate.
        let mut buf = Vec::with_capacity((w * h * 3) as usize);
        for y in 0..h {
            for x in 0..w {
                let t = (x + y) as f32 / (w + h) as f32; // 0..1 gradient
                buf.extend_from_slice(&[0.05 + 0.35 * t, 0.03 + 0.20 * t, 0.02 + 0.10 * t]);
            }
        }
        let mut img = LinearImage::new(w, h, buf, None).unwrap();
        // Dark holder ring, 3 px on all edges.
        fill_rect(&mut img, [0, 0, w, 3], HOLDER);
        fill_rect(&mut img, [0, h - 3, w, 3], HOLDER);
        fill_rect(&mut img, [0, 0, 3, h], HOLDER);
        fill_rect(&mut img, [w - 3, 0, 3, h], HOLDER);
        // Rebate band, 4 px, inset behind the holder on the requested edges.
        for &e in edges {
            let rect = match e {
                Edge::Top => [0, 3, w, 4],
                Edge::Bottom => [0, h - 7, w, 4],
                Edge::Left => [3, 0, 4, h],
                Edge::Right => [w - 7, 0, 4, h],
            };
            fill_rect(&mut img, rect, REBATE);
        }
        img
    }

    fn assert_close(base: FilmBase, want: [f32; 3], tol: f32) {
        for (got, want) in <[f32; 3]>::from(base).iter().zip(want) {
            assert!((got - want).abs() < tol, "got {base:?}, want {want:?}");
        }
    }

    #[test]
    fn explicit_source_returns_value_verbatim() {
        // A tiny dark image that auto-detection would reject still resolves,
        // because the explicit value is returned verbatim without sampling.
        let img = solid(4, 4, [0.1, 0.1, 0.1]);
        let est = estimate(
            &img,
            &FilmBaseSource::Explicit([0.9, 0.55, 0.42]),
            FilmType::Unknown,
        )
        .unwrap();
        assert_eq!(est.base, FilmBase::from([0.9, 0.55, 0.42]));
        assert!(est.warnings.is_empty());
    }

    #[test]
    fn region_source_samples_the_rectangle() {
        // Bright interior region, dark border: sampling the region must pick the
        // region's value rather than the surrounding frame.
        let mut img = solid(10, 10, [0.2, 0.2, 0.2]);
        fill_rect(&mut img, [4, 4, 2, 2], [0.8, 0.6, 0.5]);
        let est = estimate(
            &img,
            &FilmBaseSource::Region([4, 4, 2, 2]),
            FilmType::Unknown,
        )
        .unwrap();
        assert_close(est.base, [0.8, 0.6, 0.5], 1e-6);
        // A flat rectangle raises no uniformity warning.
        assert!(est.warnings.is_empty(), "{:?}", est.warnings);
    }

    #[test]
    fn mixed_region_warns_but_keeps_the_value() {
        // A rectangle straddling rebate and picture yields a plausible-looking
        // p97 — the uniformity warning is the only signal, and the value must
        // not be silently altered by the check.
        let mut img = solid(20, 20, [0.2, 0.1, 0.05]);
        fill_rect(&mut img, [0, 0, 20, 6], REBATE); // top: fake rebate
        let mixed = estimate(
            &img,
            &FilmBaseSource::Region([0, 0, 20, 12]),
            FilmType::Unknown,
        )
        .unwrap();
        assert!(
            mixed.warnings.iter().any(|w| w.contains("not uniform")),
            "mixed rectangle must warn: {:?}",
            mixed.warnings
        );
        assert_close(mixed.base, REBATE, 1e-6); // p97 lands on the bright part, unchanged
        // The clean sub-rectangle does not warn.
        let clean = estimate(
            &img,
            &FilmBaseSource::Region([0, 0, 20, 6]),
            FilmType::Unknown,
        )
        .unwrap();
        assert!(clean.warnings.is_empty(), "{:?}", clean.warnings);
    }

    #[test]
    fn auto_detects_rebate_behind_holder_on_one_edge() {
        // The real layout: holder → thin rebate (bottom edge only) → picture.
        let img = scan_with_rebate(&[Edge::Bottom]);
        let est = estimate(&img, &FilmBaseSource::Auto, FilmType::Unknown).unwrap();
        assert_close(est.base, REBATE, 0.02);
        assert!(est.warnings.is_empty(), "{:?}", est.warnings);
    }

    #[test]
    fn the_frozen_drift_gate_scan_resolves_cleanly_and_is_percentile_sensitive() {
        // The stage-2 drift fingerprint (`crate::version::PipelineFingerprint`)
        // hashes `estimate` over `golden::scan`. Two properties make that hash
        // meaningful, and neither is self-evident from the fixture:
        let img = golden::scan();

        // (1) auto resolves it cleanly — a fixture that errored, or that warned,
        //     would fingerprint the failure path instead of the detector.
        let est = estimate(&img, &FilmBaseSource::Auto, FilmType::Unknown).unwrap();
        assert!(est.warnings.is_empty(), "{:?}", est.warnings);

        // (2) the rebate band is textured, so the CHOSEN percentile is observable.
        //     A flat band returns the same value for every percentile, and retuning
        //     SAMPLE_PERCENTILE — one of the changes the gate advertises catching —
        //     would then leave the hash unmoved.
        let band = [0, 93, 100, 4];
        let p90 = sample_region_at(&img, band, 0.90).unwrap();
        let p97 = sample_region_at(&img, band, SAMPLE_PERCENTILE).unwrap();
        assert_ne!(
            <[f32; 3]>::from(p90),
            <[f32; 3]>::from(p97),
            "the frozen rebate band must not be flat, or the fingerprint cannot see a \
             retuned SAMPLE_PERCENTILE"
        );
    }

    #[test]
    fn auto_detects_agreeing_rebate_on_two_edges() {
        let img = scan_with_rebate(&[Edge::Bottom, Edge::Left]);
        let est = estimate(&img, &FilmBaseSource::Auto, FilmType::Unknown).unwrap();
        assert_close(est.base, REBATE, 0.02);
        // Same stock on both edges → no cross-edge disagreement warning.
        assert!(est.warnings.is_empty(), "{:?}", est.warnings);
    }

    #[test]
    fn auto_rejects_bright_band_at_the_edge_without_holder() {
        // The bright-surround false positive: a uniform bright margin bleeding
        // to the frame edge passed the Step-1 gates and mis-anchored the base.
        // With no dark holder outside it, the redesigned detector must refuse.
        let mut img = solid(100, 100, [0.25, 0.20, 0.18]);
        // Bright uniform ring at the very edge (no holder outside it).
        fill_rect(&mut img, [0, 0, 100, 6], [0.92, 0.55, 0.42]);
        fill_rect(&mut img, [0, 94, 100, 6], [0.92, 0.55, 0.42]);
        fill_rect(&mut img, [0, 0, 6, 100], [0.92, 0.55, 0.42]);
        fill_rect(&mut img, [94, 0, 6, 100], [0.92, 0.55, 0.42]);
        let err = estimate(&img, &FilmBaseSource::Auto, FilmType::Unknown).unwrap_err();
        assert!(matches!(err, NcError::Other(_)));
        let msg = err.to_string();
        for flag in ["--film-base", "--base-region", "--base-content"] {
            assert!(msg.contains(flag), "error must name {flag}: {msg}");
        }
    }

    #[test]
    fn auto_rejects_uniform_band_spanning_the_scan_window() {
        // Holder then a uniform-bright run that never transitions to picture
        // within the 10% scan window (a sky/wall bleeding behind the holder) is
        // scene content, not a thin rebate — the detector must produce no
        // candidate for that edge rather than anchor the roll on it.
        let (w, h) = (100u32, 100u32); // scan cap = 10
        let mut buf = Vec::with_capacity((w * h * 3) as usize);
        for y in 0..h {
            for x in 0..w {
                let t = (x + y) as f32 / (w + h) as f32; // varied interior
                buf.extend_from_slice(&[0.06 + 0.34 * t, 0.03 + 0.20 * t, 0.02 + 0.10 * t]);
            }
        }
        let mut img = LinearImage::new(w, h, buf, None).unwrap();
        fill_rect(&mut img, [0, 0, w, 3], HOLDER); // top holder, 3 px
        fill_rect(&mut img, [0, 3, w, 7], REBATE); // uniform rows 3..10 → reaches cap
        let cands = rebate_candidates(&img, FilmType::Unknown).unwrap();
        assert!(
            !cands.iter().any(|c| c.edge == Edge::Top),
            "a cap-spanning uniform band must not be a candidate: {cands:?}"
        );
    }

    #[test]
    fn auto_prefers_genuine_rebate_over_darker_uniform_band() {
        // A uniform dark band behind the holder on one edge (flat picture
        // region) must not out-rank the genuine, brighter rebate on another.
        let mut img = scan_with_rebate(&[Edge::Bottom]);
        fill_rect(&mut img, [0, 3, 100, 4], [0.20, 0.10, 0.05]); // top: flat dark band
        let est = estimate(&img, &FilmBaseSource::Auto, FilmType::Unknown).unwrap();
        assert_close(est.base, REBATE, 0.02);
    }

    #[test]
    fn auto_fails_loudly_without_a_rebate() {
        // Holder → picture directly, no rebate anywhere: auto must error with an
        // actionable message naming the recovery flags, never return a silent
        // wrong base.
        let img = scan_with_rebate(&[]);
        let err = estimate(&img, &FilmBaseSource::Auto, FilmType::Unknown).unwrap_err();
        assert!(matches!(err, NcError::Other(_)));
        let msg = err.to_string();
        for flag in ["--film-base", "--base-region", "--base-content"] {
            assert!(msg.contains(flag), "error must name {flag}: {msg}");
        }
    }

    #[test]
    fn auto_fails_on_a_uniform_image() {
        // A flat image has no holder run, hence no candidate.
        let img = solid(100, 100, [0.5, 0.5, 0.5]);
        assert!(matches!(
            estimate(&img, &FilmBaseSource::Auto, FilmType::Unknown).unwrap_err(),
            NcError::Other(_)
        ));
    }

    #[test]
    fn auto_rejects_band_darker_than_interior() {
        // A holder-backed uniform band that is *darker* than the interior median
        // is not a rebate (the rebate is maximum transmission): candidates exist
        // but none survives the interior-brightness gate.
        let mut img = solid(100, 100, [0.6, 0.6, 0.6]);
        fill_rect(&mut img, [0, 0, 100, 3], HOLDER);
        fill_rect(&mut img, [0, 3, 100, 4], [0.30, 0.30, 0.30]); // dark band
        let err = estimate(&img, &FilmBaseSource::Auto, FilmType::Unknown).unwrap_err();
        assert!(
            err.to_string().contains("higher transmission"),
            "should fail the transmission gate: {err}"
        );
    }

    #[test]
    fn auto_warns_on_disagreeing_edges() {
        // Two holder-backed bands, both higher-transmission than the interior but with
        // clearly different values: the highest-transmission wins, and the ambiguity is
        // surfaced as a warning (--strict can then refuse it).
        let mut img = scan_with_rebate(&[Edge::Bottom]);
        fill_rect(&mut img, [0, 3, 100, 4], [0.30, 0.20, 0.12]); // top: bright but different
        let est = estimate(&img, &FilmBaseSource::Auto, FilmType::Unknown).unwrap();
        assert_close(est.base, REBATE, 0.02); // highest-transmission (the rebate) still wins
        assert!(
            est.warnings.iter().any(|w| w.contains("disagree")),
            "expected a cross-edge disagreement warning: {:?}",
            est.warnings
        );
    }

    #[test]
    fn auto_does_not_warn_when_edges_agree_within_tolerance() {
        // Two bands within CROSS_EDGE_AGREE_TOL of each other: the winner is
        // chosen but no disagreement warning fires (guards the relative-diff
        // denominator — a wrong one would spuriously warn on real scans).
        let mut img = scan_with_rebate(&[Edge::Bottom]);
        // Top band ~8% brighter than REBATE per channel — inside the 15% tol.
        fill_rect(&mut img, [0, 3, 100, 4], [0.573, 0.281, 0.173]);
        let est = estimate(&img, &FilmBaseSource::Auto, FilmType::Unknown).unwrap();
        assert!(
            est.warnings.is_empty(),
            "edges within tolerance must not warn: {:?}",
            est.warnings
        );
    }

    #[test]
    fn auto_is_too_small_error_on_sliver_images() {
        // 6x6 with the minimum scan depth of 3 leaves no interior at all.
        let img = solid(6, 6, [0.5, 0.5, 0.5]);
        let err = estimate(&img, &FilmBaseSource::Auto, FilmType::Unknown).unwrap_err();
        assert!(err.to_string().contains("too small"), "{err}");
    }

    #[test]
    fn auto_warns_on_two_disagreeing_runs_on_one_edge() {
        // Since `ir-holder-detection` a single edge can yield multiple candidates
        // (one per IR film run), so two materially different bases from the SAME
        // edge must still surface as a disagreement — the old `other.edge !=
        // best.edge` filter silently dropped them.
        let mut img = scan_with_rebate(&[Edge::Bottom]);
        // Give the bottom rebate two clearly different values along the edge (> the
        // 15% CROSS_EDGE_AGREE_TOL apart). Rebate rows behind the bottom holder are
        // 93..97; the right half reads a brighter band.
        const REBATE2: [f32; 3] = [0.70, 0.36, 0.24];
        fill_rect(&mut img, [50, 93, 50, 4], REBATE2);
        // Split the bottom edge into two film runs with an IR-dark holder gap in the
        // middle (bottom probe band = 2 rows; 4 px segments, so [40, 60) is clean).
        let mut img = with_uniform_ir(img, IR_FILM);
        fill_ir_rect(&mut img, [40, 98, 20, 2], IR_HOLDER);

        // Two candidates on the one (bottom) edge, one per film run.
        let candidates = rebate_candidates(&img, FilmType::Chromogenic).unwrap();
        assert_eq!(
            candidates.iter().filter(|c| c.edge == Edge::Bottom).count(),
            2,
            "two film runs must yield two bottom candidates: {candidates:?}"
        );
        // The brighter run wins, and the same-edge ambiguity is surfaced.
        let est = select_auto_base(&img, &candidates).unwrap();
        assert_close(est.base, REBATE2, 0.02);
        assert!(
            est.warnings.iter().any(|w| w.contains("disagree")),
            "expected a same-edge disagreement warning: {:?}",
            est.warnings
        );
    }

    #[test]
    fn rebate_candidates_report_region_and_confidence() {
        // The inspect surface: candidates carry the edge, a rectangle usable as
        // --base-region, the proposed base, and the spread (confidence).
        let img = scan_with_rebate(&[Edge::Left]);
        let cands = rebate_candidates(&img, FilmType::Unknown).unwrap();
        assert_eq!(cands.len(), 1);
        let c = &cands[0];
        assert_eq!(c.edge, Edge::Left);
        // Depths 3..7 behind the left holder, trimmed by the scan depth (10).
        assert_eq!(c.region, [3, 10, 4, 80]);
        assert!(c.spread <= MAX_RELATIVE_SPREAD);
        for (got, want) in c.base.iter().zip(REBATE) {
            assert!((got - want).abs() < 0.02, "candidate base {:?}", c.base);
        }
        // The reported region re-samples to the same base it proposed.
        let est = estimate(&img, &FilmBaseSource::Region(c.region), FilmType::Unknown).unwrap();
        assert_close(est.base, c.base, 1e-6);
        assert!(est.warnings.is_empty(), "{:?}", est.warnings);

        // Bottom edge exercises the mirrored `h - end` band arithmetic (Left
        // above only covers the `start`-relative form): rebate depths 3..7 →
        // rows 93..97, so the band rect is [cap, h-end, w-2cap, thick].
        let img = scan_with_rebate(&[Edge::Bottom]);
        let cands = rebate_candidates(&img, FilmType::Unknown).unwrap();
        let c = cands.iter().find(|c| c.edge == Edge::Bottom).unwrap();
        assert_eq!(c.region, [10, 93, 80, 4]);
        let est = estimate(&img, &FilmBaseSource::Region(c.region), FilmType::Unknown).unwrap();
        assert_close(est.base, c.base, 1e-6);
    }

    #[test]
    fn high_percentile_resists_hot_pixels() {
        // A handful of blown-out pixels in the region must not pull the estimate
        // up to the max — the 97th percentile stays near the true base.
        let mut img = solid(10, 10, [0.5, 0.5, 0.5]);
        for x in 0..3 {
            set_px(&mut img, x, 0, [9.0, 9.0, 9.0]);
        }
        let est = sample_region(&img, [0, 0, 10, 10]).unwrap();
        assert!(est.base.r < 1.0, "hot pixels leaked in: {}", est.base.r);
        assert!((est.base.r - 0.5).abs() < 1e-6);
    }

    #[test]
    fn out_of_bounds_region_is_usage_error() {
        let img = solid(8, 8, [0.5, 0.5, 0.5]);
        let err = estimate(
            &img,
            &FilmBaseSource::Region([4, 4, 8, 8]),
            FilmType::Unknown,
        )
        .unwrap_err();
        assert!(matches!(err, NcError::Usage(_)));
        // Empty region is also rejected (defense-in-depth; cli.rs rejects it too).
        assert!(matches!(
            estimate(
                &img,
                &FilmBaseSource::Region([0, 0, 0, 4]),
                FilmType::Unknown
            )
            .unwrap_err(),
            NcError::Usage(_)
        ));
    }

    #[test]
    fn grid_agrees_on_a_uniform_frame() {
        // A flat unexposed-reference frame: five cells, tiny spread, agreement,
        // combined value equal to the flat color.
        let img = solid(40, 40, [0.9, 0.55, 0.42]);
        let grid = estimate_grid(&img, [0, 0, 40, 40]).unwrap();
        assert_eq!(grid.cells.len(), 5);
        assert!(
            grid.agreement,
            "uniform frame must agree: {:?}",
            grid.spread
        );
        assert!(grid.spread.iter().all(|s| *s < 1e-6));
        assert_eq!(grid.tolerance, GRID_MAX_RELATIVE_SPREAD);
        assert!((grid.base.r - 0.9).abs() < 1e-6);
        assert!((grid.base.g - 0.55).abs() < 1e-6);
        assert!((grid.base.b - 0.42).abs() < 1e-6);
        // Fixed layout: 25% cells at the corners and center of the rectangle.
        assert_eq!(grid.cells[0].region, [0, 0, 10, 10]);
        assert_eq!(grid.cells[3].region, [30, 30, 10, 10]);
        assert_eq!(grid.cells[4].region, [15, 15, 10, 10]);
    }

    #[test]
    fn grid_disagreement_is_reported_not_averaged_away() {
        // Darken one corner (a light leak / falloff): agreement must fail with
        // the spread visible, while the median combined value resists the one
        // bad cell.
        let mut img = solid(40, 40, [0.8, 0.8, 0.8]);
        for y in 0..10 {
            for x in 0..10 {
                set_px(&mut img, x, y, [0.4, 0.4, 0.4]);
            }
        }
        let grid = estimate_grid(&img, [0, 0, 40, 40]).unwrap();
        assert!(!grid.agreement, "a dark corner must break agreement");
        assert!(grid.spread[0] > GRID_MAX_RELATIVE_SPREAD);
        // Median of [0.4, 0.8, 0.8, 0.8, 0.8] stays on the true base.
        assert!((grid.base.r - 0.8).abs() < 1e-6);
        // The bad cell is identifiable in the per-cell report.
        assert!((grid.cells[0].base.r - 0.4).abs() < 1e-6);
    }

    #[test]
    fn grid_respects_the_given_rectangle() {
        // Grid over a sub-rectangle must ignore pixels outside it.
        let mut img = solid(40, 40, [0.1, 0.1, 0.1]);
        for y in 10..30 {
            for x in 10..30 {
                set_px(&mut img, x, y, [0.7, 0.6, 0.5]);
            }
        }
        let grid = estimate_grid(&img, [10, 10, 20, 20]).unwrap();
        assert!(grid.agreement);
        assert!((grid.base.r - 0.7).abs() < 1e-6);
        assert!((grid.base.b - 0.5).abs() < 1e-6);
    }

    #[test]
    fn grid_cells_land_in_bounds_on_an_odd_non_square_rect() {
        // An odd, non-square rectangle exercises the `round(w*GRID_CELL_FRAC)`
        // cell sizing and the `(w-cw)/2` integer center origin (the square/even
        // cases above hide the rounding). The five cells must land exactly where
        // that arithmetic puts them and none may spill past the rect bounds.
        let img = solid(83, 47, [0.5, 0.4, 0.3]);
        let rect = [7, 5, 61, 29]; // odd width and height, non-square, offset
        let grid = estimate_grid(&img, rect).unwrap();

        let [x, y, w, h] = rect;
        // round(61*0.25)=round(15.25)=15 ; round(29*0.25)=round(7.25)=7
        let cw = 15u32;
        let ch = 7u32;
        let expect = [
            [x, y, cw, ch],                               // top-left
            [x + w - cw, y, cw, ch],                      // top-right
            [x, y + h - ch, cw, ch],                      // bottom-left
            [x + w - cw, y + h - ch, cw, ch],             // bottom-right
            [x + (w - cw) / 2, y + (h - ch) / 2, cw, ch], // center
        ];
        for (cell, want) in grid.cells.iter().zip(expect) {
            assert_eq!(cell.region, want, "cell region mismatch");
            let [cx, cy, ccw, cch] = cell.region;
            assert!(
                cx + ccw <= x + w && cy + cch <= y + h,
                "cell {:?} spills past rect {rect:?}",
                cell.region
            );
        }
        // Center origin is the floored midpoint: (61-15)/2=23, (29-7)/2=11.
        assert_eq!(grid.cells[4].region, [7 + 23, 5 + 11, 15, 7]);
    }

    #[test]
    fn grid_degenerate_base_is_reported_but_estimate_grid_does_not_error() {
        // `estimate_grid` reports a degenerate combined base (all-dark cells) via
        // its spread sentinel + failed agreement rather than erroring — the hard
        // error is the *caller's* job (`cli::run_estimate`, after emitting the
        // report). This pins that division of responsibility so the e2e test in
        // `tests/` owns the exit-code assertion.
        let img = solid(40, 40, [0.0, 0.0, 0.0]);
        let grid = estimate_grid(&img, [0, 0, 40, 40]).unwrap();
        assert!(!grid.agreement);
        assert_eq!(<[f32; 3]>::from(grid.base), [0.0, 0.0, 0.0]);
    }

    #[test]
    fn grid_single_channel_disagreement_drives_the_verdict() {
        // Only ONE channel's cells disagree (a corner darkened on red only) while
        // green and blue stay flat. Agreement must fail, driven solely by red —
        // isolating the per-channel `spread.iter().all(...)` verdict from an
        // all-channel disagreement.
        let mut img = solid(40, 40, [0.8, 0.8, 0.8]);
        for y in 0..10 {
            for x in 0..10 {
                set_px(&mut img, x, y, [0.4, 0.8, 0.8]); // red-only dip
            }
        }
        let grid = estimate_grid(&img, [0, 0, 40, 40]).unwrap();
        assert!(!grid.agreement, "a single-channel dip must break agreement");
        assert!(
            grid.spread[0] > GRID_MAX_RELATIVE_SPREAD,
            "red must exceed tol"
        );
        assert!(grid.spread[1] <= GRID_MAX_RELATIVE_SPREAD, "green agrees");
        assert!(grid.spread[2] <= GRID_MAX_RELATIVE_SPREAD, "blue agrees");
    }

    #[test]
    fn grid_rejects_bad_rectangles() {
        let img = solid(8, 8, [0.5, 0.5, 0.5]);
        assert!(matches!(
            estimate_grid(&img, [0, 0, 0, 8]).unwrap_err(),
            NcError::Usage(_)
        ));
        assert!(matches!(
            estimate_grid(&img, [4, 4, 8, 8]).unwrap_err(),
            NcError::Usage(_)
        ));
        // A tiny rectangle still works (cells clamp to >= 1 px and may overlap).
        assert!(estimate_grid(&img, [0, 0, 2, 2]).is_ok());
    }

    #[test]
    fn grid_degenerate_all_black_frame_uses_the_spread_sentinel() {
        // An all-black rectangle (e.g. a region on the dark holder reading 0):
        // the spread guard must yield the 1.0 sentinel — not 0/0 = NaN, which
        // would serialize as `null` and break the report schema — and the
        // agreement verdict must fail closed.
        let img = solid(40, 40, [0.0, 0.0, 0.0]);
        let grid = estimate_grid(&img, [0, 0, 40, 40]).unwrap();
        assert_eq!(grid.spread, [1.0, 1.0, 1.0]);
        assert!(
            !grid.agreement,
            "degenerate sample must not count as agreeing"
        );
        assert_eq!(<[f32; 3]>::from(grid.base), [0.0, 0.0, 0.0]);
    }

    #[test]
    fn non_finite_samples_never_become_the_base() {
        // A NaN in the sampled region must be excluded from the rank, not returned
        // as the base (a NaN/inf Dmin would poison the density divide downstream).
        let mut img = solid(10, 10, [0.5, 0.5, 0.5]);
        set_px(&mut img, 0, 0, [f32::NAN, f32::INFINITY, f32::NEG_INFINITY]);
        let est = estimate(
            &img,
            &FilmBaseSource::Region([0, 0, 10, 10]),
            FilmType::Unknown,
        )
        .unwrap();
        let base = est.base;
        assert!(base.r.is_finite() && base.g.is_finite() && base.b.is_finite());
        assert_close(base, [0.5, 0.5, 0.5], 1e-6);
    }

    #[test]
    fn percentile_is_rounded_rank_over_finite_values() {
        // round((4-1)*0.5) = round(1.5) = 2 → the 3rd finite value (0.3), no
        // interpolation; non-finite values are excluded from the rank.
        let mut v = vec![f32::NAN, 0.1, 0.2, 0.3, 0.4, f32::INFINITY];
        assert_eq!(percentile(&mut v, 0.5), 0.3);
        let mut empty: Vec<f32> = vec![f32::NAN];
        assert_eq!(percentile(&mut empty, 0.5), 0.0);
    }

    #[test]
    fn sample_region_at_takes_the_requested_percentile() {
        // The roll-fixed `Dmax` reference samples the MEDIAN (`p = 0.5`), unlike the
        // film base's high percentile. On a NON-uniform region the median must land
        // strictly between the low and high percentiles — a uniform fixture (all
        // channels equal) could not catch a regression back to `p = 0.995`.
        let n = 1000u32;
        let mut buf = Vec::with_capacity((n * 3) as usize);
        for i in 0..n {
            let v = i as f32 / (n - 1) as f32; // distinct values 0.0 ..= 1.0
            buf.extend_from_slice(&[v, v, v]);
        }
        let img = LinearImage::new(n, 1, buf, None).unwrap();
        let rect = [0, 0, n, 1];
        let median = sample_region_at(&img, rect, 0.5).unwrap();
        let hi = sample_region_at(&img, rect, 0.995).unwrap();
        let lo = sample_region_at(&img, rect, 0.005).unwrap();
        // Median matches the nearest-rank index round((n-1)·0.5) exactly.
        let want_median = ((n - 1) as f32 * 0.5).round() / (n - 1) as f32;
        for c in <[f32; 3]>::from(median) {
            assert!((c - want_median).abs() < 1e-6, "median chan {c}");
        }
        // ...and is distinctly between the low and high percentiles (≈ 0.5).
        assert!(
            lo.r < median.r && median.r < hi.r,
            "median {} must sit between lo {} and hi {}",
            median.r,
            lo.r,
            hi.r
        );
        assert!(
            (median.r - 0.5).abs() < 0.01,
            "median ≈ 0.5, got {}",
            median.r
        );
    }

    #[test]
    fn candidate_serializes_with_lowercase_edge_and_region_array() {
        // The `nc inspect` machine contract (a future UI / agent consumes this):
        // `edge` is a bare lowercase string, `region` an [x,y,w,h] array. A lost
        // `#[serde(rename_all)]` on `Edge` or a field rename would ship silently.
        let c = RebateCandidate {
            edge: Edge::Left,
            region: [3, 10, 4, 80],
            base: [0.53, 0.26, 0.16],
            spread: 0.05,
        };
        let v = serde_json::to_value(&c).unwrap();
        assert_eq!(v["edge"], "left");
        assert_eq!(v["region"], serde_json::json!([3, 10, 4, 80]));
        // `base` is a 3-element number array (exact f32 values are precision-noisy).
        let base = v["base"].as_array().expect("base is an array");
        assert_eq!(base.len(), 3);
        assert!(base.iter().all(|x| x.is_number()));
        assert!(v["spread"].is_number());
    }

    #[test]
    fn degenerate_region_base_errors_loudly() {
        // A `--base-region` on the dark holder yields a zero channel; `estimate`
        // must reject it at birth (not print a poison Dmin `nc estimate` would
        // echo back), naming a recovery flag.
        let mut img = solid(50, 50, [0.4, 0.3, 0.2]);
        fill_rect(&mut img, [0, 0, 10, 10], [0.0, 0.0, 0.0]);
        let err = estimate(
            &img,
            &FilmBaseSource::Region([0, 0, 10, 10]),
            FilmType::Unknown,
        )
        .unwrap_err();
        assert!(matches!(err, NcError::Other(_)), "got {err:?}");
        assert!(
            err.to_string().contains("--film-base"),
            "degenerate-base error must name a recovery flag: {err}"
        );
    }

    // --- IR film-holder mask (`ir-holder-detection`) -------------------------

    /// Attach a flat, **marker-verified** IR plane of value `v` (helper for the mask
    /// tests — the mask only consumes a verified plane; shape-only is covered
    /// explicitly by `ir_holder_mask_requires_a_marker_verified_ir_plane`).
    fn with_uniform_ir(mut img: LinearImage, v: f32) -> LinearImage {
        img.ir = Some(vec![v; (img.width * img.height) as usize]);
        img.ir_verified = true;
        img
    }

    /// Set one IR pixel in place (the image must already carry an IR plane).
    fn set_ir(img: &mut LinearImage, x: u32, y: u32, v: f32) {
        let i = (y * img.width + x) as usize;
        img.ir.as_mut().expect("image has an IR plane")[i] = v;
    }

    /// Fill a rectangle of the IR plane with `v`.
    fn fill_ir_rect(img: &mut LinearImage, [x, y, w, h]: [u32; 4], v: f32) {
        for yy in y..y + h {
            for xx in x..x + w {
                set_ir(img, xx, yy, v);
            }
        }
    }

    /// IR transmission of film vs the opaque holder on a chromogenic scan
    /// (measured ≈ 0.6 vs ≈ 0.02 — see [`IR_HOLDER_MAX_TRANSMISSION`]).
    const IR_FILM: f32 = 0.6;
    const IR_HOLDER: f32 = 0.02;

    #[test]
    fn ir_holder_mask_only_for_chromogenic_with_an_ir_plane() {
        // The gate is film chemistry AND IR presence, per design-spec §6.1.
        // Chromogenic but no IR plane (HDR 48-bit) → None (RGB-only fallback).
        let no_ir = scan_with_rebate(&[Edge::Bottom]);
        assert!(no_ir.ir.is_none());
        assert!(
            ir_holder_mask(&no_ir, FilmType::Chromogenic)
                .unwrap()
                .is_none()
        );
        // IR present but silver / unknown → None (silver blocks IR and would
        // misread dense silver as holder; unknown is the safe default off).
        let with_ir = with_uniform_ir(scan_with_rebate(&[Edge::Bottom]), IR_FILM);
        assert!(
            ir_holder_mask(&with_ir, FilmType::Silver)
                .unwrap()
                .is_none()
        );
        assert!(
            ir_holder_mask(&with_ir, FilmType::Unknown)
                .unwrap()
                .is_none()
        );
        // Chromogenic + a (verified) IR plane → the mask is built.
        assert!(
            ir_holder_mask(&with_ir, FilmType::Chromogenic)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn ir_holder_mask_requires_a_marker_verified_ir_plane() {
        // The decoder accepts a same-dimension 16-bit grayscale page as IR by shape
        // alone (NewSubfileType=4 marker absent) and flags it `ir_verified = false`.
        // Such a plane must NOT be thresholded as IR — a stray grayscale page could
        // corrupt the base — so a chromogenic scan carrying only a shape-only plane
        // falls back to the RGB-only search (mask None). A marker-verified plane of
        // the same pixels builds the mask.
        let mut shape_only = scan_with_rebate(&[Edge::Bottom]);
        shape_only.ir = Some(vec![
            IR_FILM;
            (shape_only.width * shape_only.height) as usize
        ]);
        shape_only.ir_verified = false; // shape-only provenance
        assert!(
            ir_holder_mask(&shape_only, FilmType::Chromogenic)
                .unwrap()
                .is_none(),
            "a shape-only IR plane must not be trusted for the holder mask"
        );

        let mut verified = shape_only.clone();
        verified.ir_verified = true; // same pixels, marker-verified
        assert!(
            ir_holder_mask(&verified, FilmType::Chromogenic)
                .unwrap()
                .is_some(),
            "a marker-verified IR plane must build the mask"
        );
    }

    #[test]
    fn ir_holder_mask_labels_a_fully_occluded_and_a_fully_film_edge() {
        // A whole-edge label is the degenerate all-segments-agree case: the top
        // edge is entirely holder (dark in IR), the bottom entirely film (bright).
        let mut img = with_uniform_ir(solid(100, 100, [0.2, 0.1, 0.05]), IR_FILM);
        // holder_probe_depth(100x100) = 2, so the classifier probes a shallow 2 px
        // near-edge band; occlude the top 10 px to cover it with margin.
        fill_ir_rect(&mut img, [0, 0, 100, 10], IR_HOLDER);
        let mask = ir_holder_mask(&img, FilmType::Chromogenic)
            .unwrap()
            .unwrap();

        let top = mask.iter().find(|m| m.edge == Edge::Top).unwrap();
        assert!(
            top.segments.iter().all(|s| s.class == HolderClass::Holder),
            "fully-occluded top edge must read all-holder: {top:?}"
        );
        let bottom = mask.iter().find(|m| m.edge == Edge::Bottom).unwrap();
        assert!(
            bottom.segments.iter().all(|s| s.class == HolderClass::Film),
            "clear bottom edge must read all-film: {bottom:?}"
        );
        // Segments tile the whole edge in order (no gaps).
        assert_eq!(top.segments.first().unwrap().span[0], 0);
        assert_eq!(top.segments.last().unwrap().span[1], 100);
    }

    #[test]
    fn ir_mask_recovers_the_rebate_on_a_partially_occluded_edge() {
        // The Phoenix `933` right-edge case: a holder covers only part of the edge.
        // The near-edge full-width RGB strip would mix the top-half holder border
        // with the bottom-half rebate (high spread → no candidate), but IR splits
        // the edge so only the film run is scanned and the rebate is recovered.
        let (w, h) = (100u32, 100u32); // scan_depth = 10, segment = 4 px
        let split = 48u32; // a segment boundary, so the split is clean
        let mut buf = Vec::with_capacity((w * h * 3) as usize);
        for y in 0..h {
            for x in 0..w {
                let t = (x + y) as f32 / (w + h) as f32; // varied (high-spread) picture
                buf.extend_from_slice(&[0.05 + 0.35 * t, 0.03 + 0.20 * t, 0.02 + 0.10 * t]);
            }
        }
        let mut img = LinearImage::new(w, h, buf, None).unwrap();
        // A dark RGB border down the whole right edge, and a rebate behind it only
        // on the bottom half — RGB alone can't tell the top-half border (holder)
        // from the bottom-half border (dense film in front of the rebate).
        fill_rect(&mut img, [w - 3, 0, 3, h], HOLDER);
        fill_rect(&mut img, [w - 7, split, 4, h - split], REBATE);
        // IR: film-bright everywhere except the opaque holder occluding the
        // top-right (dark IR over the near-edge probe band).
        img = with_uniform_ir(img, IR_FILM);
        fill_ir_rect(&mut img, [w - 10, 0, 10, split], IR_HOLDER);

        // The mask splits the right edge into a holder run (top) and a film run.
        let mask = ir_holder_mask(&img, FilmType::Chromogenic)
            .unwrap()
            .unwrap();
        let right = mask.iter().find(|m| m.edge == Edge::Right).unwrap();
        assert!(
            right
                .segments
                .iter()
                .any(|s| s.class == HolderClass::Holder)
                && right.segments.iter().any(|s| s.class == HolderClass::Film),
            "the partially-occluded right edge must split into holder and film \
             segments: {right:?}"
        );

        // RGB-only: the mixed full-width strip yields no clean right-edge candidate.
        let rgb_only = rebate_candidates(&img, FilmType::Unknown).unwrap();
        assert!(
            !rgb_only.iter().any(|c| c.edge == Edge::Right),
            "RGB-only must not find a right-edge candidate on the mixed edge: {rgb_only:?}"
        );

        // With the IR mask the film run is scanned on its own and finds the rebate.
        let with_ir = rebate_candidates(&img, FilmType::Chromogenic).unwrap();
        let c = with_ir
            .iter()
            .find(|c| c.edge == Edge::Right)
            .expect("IR mask must recover the right-edge rebate");
        for (got, want) in c.base.iter().zip(REBATE) {
            assert!(
                (got - want).abs() < 0.03,
                "recovered rebate base {:?}",
                c.base
            );
        }
        // And the full estimate resolves to that rebate under the chromogenic path,
        // while the RGB-only path fails loudly (no candidate anywhere).
        let est = estimate(&img, &FilmBaseSource::Auto, FilmType::Chromogenic).unwrap();
        assert_close(est.base, REBATE, 0.03);
        assert!(estimate(&img, &FilmBaseSource::Auto, FilmType::Unknown).is_err());
    }

    #[test]
    fn chromogenic_without_ir_matches_the_rgb_only_path() {
        // An HDR 48-bit scan (no IR plane) declared chromogenic must behave exactly
        // like the RGB-only path — the mask never applies, so the base is identical.
        let img = scan_with_rebate(&[Edge::Bottom, Edge::Left]);
        let rgb = estimate(&img, &FilmBaseSource::Auto, FilmType::Unknown).unwrap();
        let chromo = estimate(&img, &FilmBaseSource::Auto, FilmType::Chromogenic).unwrap();
        assert_eq!(rgb.base, chromo.base);
    }

    #[test]
    fn ir_mask_all_film_edge_matches_the_rgb_only_candidate() {
        // A synthetic uniformly-IR-bright film (no holder anywhere): every edge
        // reads all-film, so the chromogenic scan is scanned over the same full
        // extent as RGB-only and yields the same rebate candidate. (This models the
        // exposed picture area, not real-scan frame edges: the real Ektar `1009`
        // leader genuinely sits in a holder on all its edges — near-edge IR ≈ 0.02,
        // see the ir-holder-detection progress notes — so any correct opacity
        // detector reads those edges as holder, not film.)
        let img = with_uniform_ir(scan_with_rebate(&[Edge::Bottom]), IR_FILM);
        let mask = ir_holder_mask(&img, FilmType::Chromogenic)
            .unwrap()
            .unwrap();
        assert!(
            mask.iter()
                .all(|m| m.segments.iter().all(|s| s.class == HolderClass::Film)),
            "an all-film scan must read every segment as film: {mask:?}"
        );
        let rgb = rebate_candidates(&img, FilmType::Unknown).unwrap();
        let chromo = rebate_candidates(&img, FilmType::Chromogenic).unwrap();
        assert_eq!(rgb, chromo);
    }

    #[test]
    fn ir_holder_classification_pins_the_threshold_boundary() {
        // The classifier splits holder from film at IR_HOLDER_MAX_TRANSMISSION
        // (0.1): `ir_med <= 0.1` is holder. Probe uniform IR just either side of it
        // so a regression that moves the constant out of [0.09, 0.11) flips the
        // label and fails — a far tighter pin than the coarse 0.02 / 0.6 the other
        // tests sit at.
        let just_below = with_uniform_ir(solid(100, 100, [0.2, 0.1, 0.05]), 0.09);
        let mask = ir_holder_mask(&just_below, FilmType::Chromogenic)
            .unwrap()
            .unwrap();
        assert!(
            mask.iter()
                .all(|m| m.segments.iter().all(|s| s.class == HolderClass::Holder)),
            "IR median 0.09 (≤ 0.1) must classify as holder: {mask:?}"
        );

        let just_above = with_uniform_ir(solid(100, 100, [0.2, 0.1, 0.05]), 0.11);
        let mask = ir_holder_mask(&just_above, FilmType::Chromogenic)
            .unwrap()
            .unwrap();
        assert!(
            mask.iter()
                .all(|m| m.segments.iter().all(|s| s.class == HolderClass::Film)),
            "IR median 0.11 (> 0.1) must classify as film: {mask:?}"
        );
    }

    #[test]
    fn shallow_probe_reads_a_thin_holder_over_bright_film() {
        // holder_probe_depth(100x100) = 2, so the classifier probes a shallow 2 px
        // near-edge band. A 2 px IR-dark holder band with IR-bright film directly
        // behind it must still read holder — this pins the shallow-probe rationale:
        // a deep probe (e.g. the ~10 px rebate-scan window) would average in the
        // bright film behind the thin band and misread the edge as film.
        let mut img = with_uniform_ir(solid(100, 100, [0.2, 0.1, 0.05]), IR_FILM);
        fill_ir_rect(&mut img, [0, 0, 100, 2], IR_HOLDER); // 2 px dark band, top edge
        let mask = ir_holder_mask(&img, FilmType::Chromogenic)
            .unwrap()
            .unwrap();
        let top = mask.iter().find(|m| m.edge == Edge::Top).unwrap();
        assert!(
            top.segments.iter().all(|s| s.class == HolderClass::Holder),
            "the thin holder band over bright film must read all-holder: {top:?}"
        );
        // It samples the dark band's value (≈ IR_HOLDER), not the bright film behind
        // it — proof the probe stayed shallow.
        assert!(
            top.segments
                .iter()
                .all(|s| s.ir <= IR_HOLDER_MAX_TRANSMISSION),
            "probe must sample the dark band, not the film behind it: {top:?}"
        );
    }

    #[test]
    fn a_film_holder_film_edge_yields_two_film_runs_and_two_candidates() {
        // The bottom edge carries the rebate full-width, but an opaque holder
        // occludes its along-edge middle (IR-dark), splitting it into two film runs.
        // This exercises `film_along_ranges` emitting >1 range (the mid-loop
        // holder-flush branch) and `rebate_candidates` producing >1 candidate on a
        // single edge.
        let mut img = with_uniform_ir(scan_with_rebate(&[Edge::Bottom]), IR_FILM);
        // Bottom-edge probe band is the bottom holder_probe_depth = 2 rows (y 98..100);
        // occlude the along-edge middle [40, 60). The 24-way split of 100 gives 4 px
        // segments, so 40 and 60 are clean segment boundaries.
        fill_ir_rect(&mut img, [40, 98, 20, 2], IR_HOLDER);

        // Two contiguous film runs on the bottom edge (holder middle excluded), each
        // clipped to the corner-trimmed [cap, along-cap) = [10, 90) extent.
        let cap = scan_depth(&img).unwrap();
        let mask = ir_holder_mask(&img, FilmType::Chromogenic).unwrap();
        let runs = film_along_ranges(mask.as_deref(), Edge::Bottom, &img, cap);
        assert_eq!(
            runs,
            vec![(10, 40), (60, 90)],
            "expected two film runs on the split bottom edge: {runs:?}"
        );

        // Each film run finds the rebate → two bottom-edge candidates.
        let candidates = rebate_candidates(&img, FilmType::Chromogenic).unwrap();
        let bottom: Vec<_> = candidates
            .iter()
            .filter(|c| c.edge == Edge::Bottom)
            .collect();
        assert_eq!(
            bottom.len(),
            2,
            "the split bottom edge must yield one candidate per film run: {candidates:?}"
        );
        for c in &bottom {
            for (got, want) in c.base.iter().zip(REBATE) {
                assert!((got - want).abs() < 0.02, "candidate base {:?}", c.base);
            }
        }
        // And the estimate resolves to the rebate under the chromogenic path.
        let est = estimate(&img, &FilmBaseSource::Auto, FilmType::Chromogenic).unwrap();
        assert_close(est.base, REBATE, 0.02);
    }

    #[test]
    fn auto_interior_pixels_matches_the_rectangle_the_selector_samples() {
        // `pipeline::memory` sizes the film-base phase from this helper, so it must
        // stay the same rectangle `select_auto_base` materializes
        // (`[cap, cap, w - 2*cap, h - 2*cap]`) — the two must not drift.
        for (w, h) in [(100u32, 100u32), (502, 462), (10368, 7200)] {
            let cap = scan_depth_for(w, h).expect("scannable");
            assert_eq!(
                auto_interior_pixels(w, h),
                (w - 2 * cap) as u64 * (h - 2 * cap) as u64,
                "{w}x{h}"
            );
        }
        // Too small to scan: detection errors before any interior sample exists, so
        // the model must count nothing (and not fabricate a rejection).
        assert_eq!(scan_depth_for(6, 6), None);
        assert_eq!(auto_interior_pixels(6, 6), 0);
    }

    #[test]
    fn all_holder_frame_drives_the_loud_empty_candidates_error() {
        // Every edge reads holder in IR (an all-opaque-carrier frame), so every edge
        // has no film run, `rebate_candidates` is empty, and both `select_auto_base`
        // and the `auto` `estimate` fail loudly rather than inventing a base.
        let img = with_uniform_ir(solid(100, 100, [0.2, 0.1, 0.05]), IR_HOLDER);
        let candidates = rebate_candidates(&img, FilmType::Chromogenic).unwrap();
        assert!(
            candidates.is_empty(),
            "an all-holder frame yields no candidates: {candidates:?}"
        );
        let err = select_auto_base(&img, &candidates).unwrap_err();
        assert!(matches!(err, NcError::Other(_)), "got {err:?}");
        assert!(
            err.to_string().contains("no uniform unexposed rebate band"),
            "empty-candidates error must be the loud no-band message: {err}"
        );
        let err = estimate(&img, &FilmBaseSource::Auto, FilmType::Chromogenic).unwrap_err();
        assert!(matches!(err, NcError::Other(_)), "got {err:?}");
    }

    #[test]
    fn holder_mask_serializes_with_lowercase_class() {
        // The `nc inspect` holder output is a machine contract: `class` must be a
        // bare lowercase string and the segment fields stable, so a lost
        // `#[serde(rename_all)]` on `HolderClass` (or a field rename) can't ship
        // Capitalized JSON silently — the mirror of the `RebateCandidate` guard.
        let mask = EdgeHolderMask {
            edge: Edge::Right,
            segments: vec![
                HolderSegment {
                    span: [0, 24],
                    class: HolderClass::Holder,
                    ir: 0.02,
                },
                HolderSegment {
                    span: [24, 48],
                    class: HolderClass::Film,
                    ir: 0.6,
                },
            ],
        };
        let v = serde_json::to_value(&mask).unwrap();
        assert_eq!(v["edge"], "right");
        assert_eq!(v["segments"][0]["class"], "holder");
        assert_eq!(v["segments"][1]["class"], "film");
        assert_eq!(v["segments"][0]["span"], serde_json::json!([0, 24]));
        assert!(v["segments"][0]["ir"].is_number());
    }
}
