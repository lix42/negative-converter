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
//! `film-base-content-fallback` task — auto only *suggests* it on refusal.)
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

use serde::Serialize;

use crate::types::{FilmBase, FilmBaseParams, FilmBaseSource, LinearImage, NcError, Result};

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

/// Shared recovery advice appended to every auto-detection refusal, naming the
/// fallback options. Kept in one place so the too-small and no-band errors stay
/// consistent. Content-based estimation (`--base-content`) is only *suggested*
/// here — it is owned by the separate `film-base-content-fallback` task and is
/// never a silent fallback (design-spec §9 ladder tier 3).
const RECOVERY_ADVICE: &str = "pass --film-base or --base-region (design-spec §9: measure once \
     from an unexposed reference and reuse it). For a cropped scan with no unexposed \
     film visible, content-based estimation is planned but not yet available (the \
     --base-content flag is owned by the film-base-content-fallback task); until it \
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
pub fn estimate(image: &LinearImage, params: &FilmBaseParams) -> Result<BaseEstimate> {
    let est = match params.source {
        FilmBaseSource::Explicit(rgb) => BaseEstimate::clean(FilmBase::from(rgb)),
        FilmBaseSource::Region(rect) => sample_region(image, rect)?,
        FilmBaseSource::Auto => {
            let candidates = rebate_candidates(image)?;
            select_auto_base(image, &candidates)?
        }
    };
    guard_base(&est.base, &params.source)?;
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
/// Returns one candidate per edge at most; an empty result means no confident
/// band exists anywhere. Candidates are **not** transmission-gated here — that is
/// [`select_auto_base`]'s job, which must be called on the **same image** these
/// candidates came from (it recomputes the scan depth and interior median from
/// it). Errors only when the image is too small to scan.
pub fn rebate_candidates(image: &LinearImage) -> Result<Vec<RebateCandidate>> {
    let cap = scan_depth(image)?;
    let mut found = Vec::new();
    for edge in [Edge::Top, Edge::Bottom, Edge::Left, Edge::Right] {
        if let Some(c) = edge_candidate(image, edge, cap)? {
            found.push(c);
        }
    }
    Ok(found)
}

/// Pick the film base from the detector's candidates: filter to bands with
/// higher transmission than the frame-interior median by
/// [`INTERIOR_BRIGHTNESS_MARGIN`] on **every** channel (the rebate is per-channel
/// `Dmin` = maximum transmission), then take the highest-transmission survivor
/// — nothing genuine can out-transmit clean base, so a uniform low-transmission
/// picture band can never out-rank a real rebate. Disagreement between surviving edges
/// (beyond [`CROSS_EDGE_AGREE_TOL`]) is surfaced as a warning rather than
/// silently ignored. Fails loudly, naming every recovery flag, when no candidate
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
    for other in survivors.iter().filter(|c| c.edge != best.edge) {
        let diff = best
            .base
            .iter()
            .zip(other.base)
            .map(|(&a, b)| (a - b).abs() / a.max(f32::MIN_POSITIVE))
            .fold(0.0f32, f32::max);
        if diff > CROSS_EDGE_AGREE_TOL {
            est.warnings.push(format!(
                "auto film-base candidates disagree across edges: chose {:?} {:?} but \
                 {:?} reads {:?} (relative difference {diff:.2} > \
                 {CROSS_EDGE_AGREE_TOL:.2}); verify with `nc inspect` / --base-region",
                best.edge, best.base, other.edge, other.base
            ));
        }
    }
    Ok(est)
}

/// Mean of the three channels — the mean transmission used to rank candidates.
fn mean(rgb: &[f32; 3]) -> f32 {
    (rgb[0] + rgb[1] + rgb[2]) / 3.0
}

/// The inward scan depth (and strip end-trim): [`REBATE_SCAN_FRAC`] of the
/// shorter dimension, at least deep enough for a holder strip plus a minimal
/// band. Errors when the image can't fit the scan plus an interior.
fn scan_depth(image: &LinearImage) -> Result<u32> {
    let (w, h) = (image.width, image.height);
    let cap = ((w.min(h) as f32 * REBATE_SCAN_FRAC).round() as u32).max(MIN_BAND_STRIPS + 1);
    if 2 * cap >= w || 2 * cap >= h {
        return Err(NcError::Other(format!(
            "image {w}x{h} is too small for auto film-base detection; \
             {RECOVERY_ADVICE}"
        )));
    }
    Ok(cap)
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

/// The 1-px strip rectangle at `depth` pixels in from `edge`, trimmed by `cap`
/// at both ends (the corners belong to the perpendicular edges' holder).
fn strip_rect(image: &LinearImage, edge: Edge, depth: u32, cap: u32) -> [u32; 4] {
    let (w, h) = (image.width, image.height);
    match edge {
        Edge::Top => [cap, depth, w - 2 * cap, 1],
        Edge::Bottom => [cap, h - 1 - depth, w - 2 * cap, 1],
        Edge::Left => [depth, cap, 1, h - 2 * cap],
        Edge::Right => [w - 1 - depth, cap, 1, h - 2 * cap],
    }
}

/// The band rectangle covering strip depths `[start, end)` on `edge`.
fn band_rect(image: &LinearImage, edge: Edge, start: u32, end: u32, cap: u32) -> [u32; 4] {
    let (w, h) = (image.width, image.height);
    let thick = end - start;
    match edge {
        Edge::Top => [cap, start, w - 2 * cap, thick],
        Edge::Bottom => [cap, h - end, w - 2 * cap, thick],
        Edge::Left => [start, cap, thick, h - 2 * cap],
        Edge::Right => [w - end, cap, thick, h - 2 * cap],
    }
}

/// Classify the strip at `depth` in from `edge`.
fn classify_strip(image: &LinearImage, edge: Edge, depth: u32, cap: u32) -> Result<StripClass> {
    let mut chans = region_channels(image, strip_rect(image, edge, depth, cap))?;
    let (hi, spread) = channel_stats(&mut chans);
    if hi.iter().all(|&v| v < HOLDER_MAX_TRANSMISSION) {
        Ok(StripClass::Holder)
    } else if spread <= MAX_RELATIVE_SPREAD {
        Ok(StripClass::Uniform(hi))
    } else {
        Ok(StripClass::Other)
    }
}

/// Find the rebate candidate on one edge, if any: a contiguous holder run at
/// the very edge, then a run of uniform, value-continuous strips at least
/// [`MIN_BAND_STRIPS`] thick. The whole band is then re-measured as one region
/// and must itself pass the uniformity gate (defense against a slow drift the
/// per-strip checks can't see). A high-transmission band **at** the edge (no holder
/// outside it) is rejected — that is the bright-surround false positive, or a
/// crop with no holder, and both belong to `--base-region`, not auto.
fn edge_candidate(image: &LinearImage, edge: Edge, cap: u32) -> Result<Option<RebateCandidate>> {
    // Contiguous holder run from depth 0.
    let mut depth = 0;
    while depth < cap && classify_strip(image, edge, depth, cap)? == StripClass::Holder {
        depth += 1;
    }
    if depth == 0 || depth >= cap {
        return Ok(None); // no holder at the edge, or holder all the way down
    }

    // Uniform, value-continuous band immediately behind the holder.
    let start = depth;
    let mut prev: Option<[f32; 3]> = None;
    while depth < cap {
        let StripClass::Uniform(hi) = classify_strip(image, edge, depth, cap)? else {
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
    let region = band_rect(image, edge, start, depth, cap);
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
/// this bool + overloaded sentinel with a self-describing verdict enum
/// (`Uniform | Disagree | Degenerate`) so the estimate reports its own verdict
/// and the CLI stops re-deriving it is the `grid-verdict-enum` follow-up task.
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

    /// A `FilmBaseParams` selecting the given source.
    fn params(source: FilmBaseSource) -> FilmBaseParams {
        FilmBaseParams { source }
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
        let est = estimate(&img, &params(FilmBaseSource::Explicit([0.9, 0.55, 0.42]))).unwrap();
        assert_eq!(est.base, FilmBase::from([0.9, 0.55, 0.42]));
        assert!(est.warnings.is_empty());
    }

    #[test]
    fn region_source_samples_the_rectangle() {
        // Bright interior region, dark border: sampling the region must pick the
        // region's value rather than the surrounding frame.
        let mut img = solid(10, 10, [0.2, 0.2, 0.2]);
        fill_rect(&mut img, [4, 4, 2, 2], [0.8, 0.6, 0.5]);
        let est = estimate(&img, &params(FilmBaseSource::Region([4, 4, 2, 2]))).unwrap();
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
        let mixed = estimate(&img, &params(FilmBaseSource::Region([0, 0, 20, 12]))).unwrap();
        assert!(
            mixed.warnings.iter().any(|w| w.contains("not uniform")),
            "mixed rectangle must warn: {:?}",
            mixed.warnings
        );
        assert_close(mixed.base, REBATE, 1e-6); // p97 lands on the bright part, unchanged
        // The clean sub-rectangle does not warn.
        let clean = estimate(&img, &params(FilmBaseSource::Region([0, 0, 20, 6]))).unwrap();
        assert!(clean.warnings.is_empty(), "{:?}", clean.warnings);
    }

    #[test]
    fn auto_detects_rebate_behind_holder_on_one_edge() {
        // The real layout: holder → thin rebate (bottom edge only) → picture.
        let img = scan_with_rebate(&[Edge::Bottom]);
        let est = estimate(&img, &params(FilmBaseSource::Auto)).unwrap();
        assert_close(est.base, REBATE, 0.02);
        assert!(est.warnings.is_empty(), "{:?}", est.warnings);
    }

    #[test]
    fn auto_detects_agreeing_rebate_on_two_edges() {
        let img = scan_with_rebate(&[Edge::Bottom, Edge::Left]);
        let est = estimate(&img, &params(FilmBaseSource::Auto)).unwrap();
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
        let err = estimate(&img, &params(FilmBaseSource::Auto)).unwrap_err();
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
        let cands = rebate_candidates(&img).unwrap();
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
        let est = estimate(&img, &params(FilmBaseSource::Auto)).unwrap();
        assert_close(est.base, REBATE, 0.02);
    }

    #[test]
    fn auto_fails_loudly_without_a_rebate() {
        // Holder → picture directly, no rebate anywhere: auto must error with an
        // actionable message naming the recovery flags, never return a silent
        // wrong base.
        let img = scan_with_rebate(&[]);
        let err = estimate(&img, &params(FilmBaseSource::Auto)).unwrap_err();
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
            estimate(&img, &params(FilmBaseSource::Auto)).unwrap_err(),
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
        let err = estimate(&img, &params(FilmBaseSource::Auto)).unwrap_err();
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
        let est = estimate(&img, &params(FilmBaseSource::Auto)).unwrap();
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
        let est = estimate(&img, &params(FilmBaseSource::Auto)).unwrap();
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
        let err = estimate(&img, &params(FilmBaseSource::Auto)).unwrap_err();
        assert!(err.to_string().contains("too small"), "{err}");
    }

    #[test]
    fn rebate_candidates_report_region_and_confidence() {
        // The inspect surface: candidates carry the edge, a rectangle usable as
        // --base-region, the proposed base, and the spread (confidence).
        let img = scan_with_rebate(&[Edge::Left]);
        let cands = rebate_candidates(&img).unwrap();
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
        let est = estimate(&img, &params(FilmBaseSource::Region(c.region))).unwrap();
        assert_close(est.base, c.base, 1e-6);
        assert!(est.warnings.is_empty(), "{:?}", est.warnings);

        // Bottom edge exercises the mirrored `h - end` band arithmetic (Left
        // above only covers the `start`-relative form): rebate depths 3..7 →
        // rows 93..97, so the band rect is [cap, h-end, w-2cap, thick].
        let img = scan_with_rebate(&[Edge::Bottom]);
        let cands = rebate_candidates(&img).unwrap();
        let c = cands.iter().find(|c| c.edge == Edge::Bottom).unwrap();
        assert_eq!(c.region, [10, 93, 80, 4]);
        let est = estimate(&img, &params(FilmBaseSource::Region(c.region))).unwrap();
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
        let err = estimate(&img, &params(FilmBaseSource::Region([4, 4, 8, 8]))).unwrap_err();
        assert!(matches!(err, NcError::Usage(_)));
        // Empty region is also rejected (defense-in-depth; cli.rs rejects it too).
        assert!(matches!(
            estimate(&img, &params(FilmBaseSource::Region([0, 0, 0, 4]))).unwrap_err(),
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
        let est = estimate(&img, &params(FilmBaseSource::Region([0, 0, 10, 10]))).unwrap();
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
        let err = estimate(&img, &params(FilmBaseSource::Region([0, 0, 10, 10]))).unwrap_err();
        assert!(matches!(err, NcError::Other(_)), "got {err:?}");
        assert!(
            err.to_string().contains("--film-base"),
            "degenerate-base error must name a recovery flag: {err}"
        );
    }
}
