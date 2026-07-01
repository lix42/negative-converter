//! `Dmin` / film-base estimation from the unexposed border (pure).
//!
//! The film base is the unexposed leader/rebate of the negative: the area of
//! minimum density, hence **maximum transmission** — it scans as the brightest,
//! near-uniform margin around the frame. Its per-channel transmission is the
//! `Dmin` anchor the `density` algorithm divides by (`D = -log10(scan / Dmin)`),
//! so a good estimate matters.
//!
//! The source of the base is a single mutually-exclusive choice carried by
//! [`FilmBaseSource`] (resolved from the flags/recipe in `cli.rs`): an explicit
//! per-channel override, a user-supplied region to sample, or auto-detection of
//! the border. This stage just honors whichever the caller selected.

use crate::types::{FilmBase, FilmBaseParams, FilmBaseSource, LinearImage, NcError, Result};

/// Percentile used to summarize a region per channel. A high percentile (rather
/// than the raw max) resists hot pixels / dust sparkles while still landing on
/// the bright film base. Design task suggests 95th–99th; 97th is the middle.
const SAMPLE_PERCENTILE: f32 = 0.97;

/// Fraction of the shorter image dimension used as the auto-detected margin band
/// width on each edge. Small enough to stay in the rebate, wide enough to gather
/// a stable sample.
const AUTO_MARGIN_FRAC: f32 = 0.04;

/// Max acceptable per-channel relative spread within the auto margin band for it
/// to count as a confident, near-uniform border. Spread is
/// `(p_high - p_low) / p_high`; a real rebate is flat, an image that bleeds to
/// the edge is not.
const AUTO_MAX_RELATIVE_SPREAD: f32 = 0.15;

/// Low percentile used for the uniformity check (paired with [`SAMPLE_PERCENTILE`]).
const AUTO_LOW_PERCENTILE: f32 = 0.10;

/// Resolve the film base for `image` from the selected [`FilmBaseSource`]:
/// return the explicit override, sample the given region, or auto-detect the
/// unexposed border. Region bounds and auto-detection confidence are checked
/// here (the image isn't available at the CLI boundary), failing loudly rather
/// than returning a silently-wrong anchor.
pub fn estimate(image: &LinearImage, params: &FilmBaseParams) -> Result<FilmBase> {
    match params.source {
        FilmBaseSource::Explicit(rgb) => Ok(FilmBase::from(rgb)),
        FilmBaseSource::Region(rect) => sample_region(image, rect),
        FilmBaseSource::Auto => auto_estimate(image),
    }
}

/// Per-channel high-percentile transmission over the rectangle `[x, y, w, h]`
/// (the film-base summary statistic; see [`SAMPLE_PERCENTILE`]).
fn sample_region(image: &LinearImage, rect: [u32; 4]) -> Result<FilmBase> {
    sample_region_at(image, rect, SAMPLE_PERCENTILE)
}

/// Per-channel `p`-quantile transmission over the rectangle `[x, y, w, h]`.
/// The rectangle must lie within the image; an out-of-bounds or empty region is
/// a usage error rather than a clamp, so a bad `--base-region` fails loudly.
fn sample_region_at(image: &LinearImage, [x, y, w, h]: [u32; 4], p: f32) -> Result<FilmBase> {
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

    Ok(FilmBase {
        r: percentile(&mut chans[0], p),
        g: percentile(&mut chans[1], p),
        b: percentile(&mut chans[2], p),
    })
}

/// Step-1 heuristic border detection: sample the outer margin band on all four
/// edges, summarize per channel, and accept it only if the band is near-uniform
/// and brighter than the frame interior. On low confidence, fail loudly so the
/// user can pass `--film-base` / `--base-region` instead of getting a silently
/// wrong anchor.
fn auto_estimate(image: &LinearImage) -> Result<FilmBase> {
    let (w, h) = (image.width, image.height);
    let margin = (w.min(h) as f32 * AUTO_MARGIN_FRAC).round() as u32;
    let margin = margin.max(1);
    // Need an interior to compare against; a sliver image can't be auto-detected.
    if margin * 2 >= w || margin * 2 >= h {
        return Err(NcError::Other(format!(
            "image {w}x{h} is too small for auto film-base border detection; \
             pass --film-base or --base-region"
        )));
    }

    // Gather the four edge strips into a per-channel sample of the margin band.
    let mut chans: [Vec<f32>; 3] = [Vec::new(), Vec::new(), Vec::new()];
    let push_px = |i: usize, chans: &mut [Vec<f32>; 3]| {
        chans[0].push(image.rgb[i * 3]);
        chans[1].push(image.rgb[i * 3 + 1]);
        chans[2].push(image.rgb[i * 3 + 2]);
    };
    for row in 0..h {
        let edge_row = row < margin || row >= h - margin;
        for col in 0..w {
            let in_band = edge_row || col < margin || col >= w - margin;
            if in_band {
                push_px((row * w + col) as usize, &mut chans);
            }
        }
    }

    // Per-channel high percentile is the candidate base; the low percentile gives
    // the spread for the uniformity check.
    let mut base = [0.0f32; 3];
    for (c, samples) in chans.iter_mut().enumerate() {
        let hi = percentile(samples, SAMPLE_PERCENTILE);
        let lo = percentile(samples, AUTO_LOW_PERCENTILE);
        let spread = if hi > 0.0 { (hi - lo) / hi } else { 1.0 };
        if spread > AUTO_MAX_RELATIVE_SPREAD {
            return Err(NcError::Other(format!(
                "auto film-base border is not uniform on channel {c} \
                 (relative spread {spread:.2} > {AUTO_MAX_RELATIVE_SPREAD:.2}); \
                 the frame may bleed to the edge — pass --film-base or --base-region"
            )));
        }
        base[c] = hi;
    }

    // The base must be brighter than the interior (it's the densest/brightest
    // transmission area); if it isn't, we haven't found an unexposed border.
    // Compare against the interior *median* (not a high percentile): the sampled
    // interior can still clip part of a wide rebate, and the median resists that
    // contamination so a genuine border stays distinguishable.
    let interior = sample_region_at(image, [margin, margin, w - 2 * margin, h - 2 * margin], 0.5)?;
    let interior = [interior.r, interior.g, interior.b];
    if !base.iter().zip(interior).any(|(&b, i)| b > i * 1.02) {
        return Err(NcError::Other(
            "auto film-base border is not brighter than the frame interior; \
             no unexposed border detected — pass --film-base or --base-region"
                .to_string(),
        ));
    }

    Ok(FilmBase::from(base))
}

/// The `p`-quantile (0.0–1.0) of `values` by nearest-rank, sorting in place.
/// Empty input yields `0.0`. NaNs sort to the end so they don't poison the rank.
fn percentile(values: &mut [f32], p: f32) -> f32 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Greater));
    let p = p.clamp(0.0, 1.0);
    // Nearest-rank: index of the p-quantile within [0, len-1].
    let idx = ((values.len() - 1) as f32 * p).round() as usize;
    values[idx]
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

    /// A `FilmBaseParams` selecting the given source.
    fn params(source: FilmBaseSource) -> FilmBaseParams {
        FilmBaseParams { source }
    }

    #[test]
    fn explicit_source_returns_value_verbatim() {
        // A tiny dark image that auto-detection would reject still resolves,
        // because the explicit value is returned verbatim without sampling.
        let img = solid(4, 4, [0.1, 0.1, 0.1]);
        let base = estimate(&img, &params(FilmBaseSource::Explicit([0.9, 0.55, 0.42]))).unwrap();
        assert_eq!(base, FilmBase::from([0.9, 0.55, 0.42]));
    }

    #[test]
    fn region_source_samples_the_rectangle() {
        // Bright interior region, dark border: sampling the region must pick the
        // region's value rather than the surrounding frame.
        let mut img = solid(10, 10, [0.2, 0.2, 0.2]);
        for y in 4..6 {
            for x in 4..6 {
                set_px(&mut img, x, y, [0.8, 0.6, 0.5]);
            }
        }
        let base = estimate(&img, &params(FilmBaseSource::Region([4, 4, 2, 2]))).unwrap();
        assert!((base.r - 0.8).abs() < 1e-6);
        assert!((base.g - 0.6).abs() < 1e-6);
        assert!((base.b - 0.5).abs() < 1e-6);
    }

    #[test]
    fn auto_detects_bright_uniform_border() {
        // Bright uniform border, darker interior — the classic rebate layout.
        let mut img = solid(40, 40, [0.92, 0.55, 0.42]);
        for y in 4..36 {
            for x in 4..36 {
                set_px(&mut img, x, y, [0.25, 0.20, 0.18]);
            }
        }
        let base = estimate(&img, &params(FilmBaseSource::Auto)).unwrap();
        assert!((base.r - 0.92).abs() < 0.02, "r = {}", base.r);
        assert!((base.g - 0.55).abs() < 0.02, "g = {}", base.g);
        assert!((base.b - 0.42).abs() < 0.02, "b = {}", base.b);
    }

    #[test]
    fn high_percentile_resists_hot_pixels() {
        // A handful of blown-out pixels in the region must not pull the estimate
        // up to the max — the 97th percentile stays near the true base.
        let mut img = solid(10, 10, [0.5, 0.5, 0.5]);
        for x in 0..3 {
            set_px(&mut img, x, 0, [9.0, 9.0, 9.0]);
        }
        let base = sample_region(&img, [0, 0, 10, 10]).unwrap();
        assert!(base.r < 1.0, "hot pixels leaked into estimate: {}", base.r);
        assert!((base.r - 0.5).abs() < 1e-6);
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
    fn auto_fails_loudly_without_a_border() {
        // A uniform image has no border brighter than its interior: auto must
        // error with an actionable message, never return a silent wrong base.
        let img = solid(40, 40, [0.5, 0.5, 0.5]);
        let err = estimate(&img, &params(FilmBaseSource::Auto)).unwrap_err();
        assert!(matches!(err, NcError::Other(_)));
    }

    #[test]
    fn auto_fails_on_non_uniform_border() {
        // A horizontal gradient makes the margin band non-uniform → reject.
        let (w, h) = (40u32, 40u32);
        let mut buf = Vec::with_capacity((w * h * 3) as usize);
        for _y in 0..h {
            for x in 0..w {
                let v = x as f32 / w as f32;
                buf.extend_from_slice(&[v, v, v]);
            }
        }
        let img = LinearImage::new(w, h, buf, None).unwrap();
        let err = estimate(&img, &params(FilmBaseSource::Auto)).unwrap_err();
        assert!(matches!(err, NcError::Other(_)));
    }
}
