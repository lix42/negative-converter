//! The per-channel gain between a gain map's two renditions, for the new chain.
//!
//! A gain map stores `G = (hdr + o) / (base + o)` per channel, and a decoder rebuilds
//! the HDR rendition as `G · (base + o) − o` from the base **as stored**. The base is
//! the SDR rendition after the encoder's clamp to `[0, 1]`, so the gain is ratioed
//! against that. Ratioing against the unclamped rendition stores a gain short by
//! whatever was clamped, and the highlight rebuilds dark with nothing counting it.
//!
//! This is the arithmetic only. Downsampling, quantization and the container (Ultra
//! HDR / ISO 21496-1) are the destination's (`nf-destinations/gain-map-destination`), and so is
//! the offset, which is format policy rather than a conversion knob.
//! Clamping the HDR rendition to the destination's peak, and counting what that
//! clamps, is also the caller's: [`between`] clamps the alternate only to `≥ 0`.
//!
//! **A flat map is a fact, not an error.** When the pair agrees everywhere (a frame
//! with nothing above diffuse white and no colour the SDR cube binds), every gain is
//! exactly `1` and the map carries nothing. [`GainRange::flat`] says so, so a report
//! can state it rather than ship a gain map that silently does nothing.
//!
//! Written fresh, per the migration rule: `pipeline::gain_map` is the current chain's
//! builder and retires with it.

use serde::Serialize;

use crate::pipeline::pixels;
use crate::types::{LinearImage, NcError, Result};

/// Full-resolution per-channel gains from a base (SDR) to an alternate (HDR)
/// rendition.
#[cfg_attr(not(test), allow(dead_code))] // the gain-map destination (`nf-destinations/gain-map-destination`)
#[derive(Debug)]
pub struct GainRatios {
    width: u32,
    height: u32,
    /// Interleaved `r,g,b` gains, each finite and positive.
    rgb: Vec<f32>,
    offset: f32,
    range: GainRange,
}

/// The extent of a gain map, per channel — what its metadata states and what a report
/// reads.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct GainRange {
    pub min: [f32; 3],
    pub max: [f32; 3],
    /// Every gain is exactly `1`: the two renditions are identical as stored, and the
    /// map carries nothing.
    pub flat: bool,
}

#[cfg_attr(not(test), allow(dead_code))] // the gain-map destination (`nf-destinations/gain-map-destination`)
impl GainRatios {
    /// The gains' extent.
    pub fn range(&self) -> GainRange {
        self.range
    }

    /// The gains, interleaved `r,g,b`.
    pub fn rgb(&self) -> &[f32] {
        &self.rgb
    }

    /// What a decoder rebuilds from `base` (the SDR rendition, clamped here as the
    /// encoder stores it) with these gains, at full resolution and precision — the
    /// map's round trip before any quantization.
    pub fn apply_to(&self, base: &LinearImage) -> Result<Vec<f32>> {
        check_dimensions(base, self.width, self.height)?;
        let gains = pixels::triples(&self.rgb)?;
        let offset = f64::from(self.offset);
        pixels::try_map(&base.rgb, |index, px| {
            let gain = gains[index];
            Ok(std::array::from_fn(|c| {
                (f64::from(gain[c]) * (stored(px[c]) + offset) - offset) as f32
            }))
        })
    }
}

/// The per-channel gains from `sdr` (the base) to `hdr` (the alternate), with the
/// offset `o` added to both.
///
/// Both renditions must share dimensions and primaries (the caller's to guarantee: a
/// pair from `chain::render_pair` shares its gamut by construction). **Each sample is
/// taken as an encoder stores it**: the base clamped to `[0, 1]`, the alternate to
/// `≥ 0`. Fit gamut writes no meaningful negative, but where two channels tie on the
/// cube's black face the one not assigned the boundary can land an ulp below zero
/// (about `-1e-17`), so refusing negatives would fail legitimate pairs. A non-finite
/// sample is refused, naming the lowest pixel: the chain never writes one. So is a gain
/// too large for an `f32`: the alternate is not clamped to a peak here (the
/// destination's job), so a finite but huge HDR sample over a dark base can overflow.
#[cfg_attr(not(test), allow(dead_code))] // the gain-map destination (`nf-destinations/gain-map-destination`)
pub fn between(sdr: &LinearImage, hdr: &LinearImage, offset: f32) -> Result<GainRatios> {
    if !(offset.is_finite() && offset > 0.0) {
        return Err(NcError::Other(format!(
            "a gain map's offset must be finite and positive, got {offset}"
        )));
    }
    check_dimensions(hdr, sdr.width, sdr.height)?;
    let base = pixels::triples(&sdr.rgb)?;
    let o = f64::from(offset);
    let rgb = pixels::try_map(&hdr.rgb, |index, alternate| {
        let base = base[index];
        for (name, px) in [("SDR", base), ("HDR", alternate)] {
            if !px.iter().all(|v| v.is_finite()) {
                return Err(NcError::Other(format!(
                    "a gain map's {name} rendition has a non-finite sample at pixel \
                     {index} ({px:?})"
                )));
            }
        }
        let gain: [f32; 3] = std::array::from_fn(|c| {
            ((f64::from(alternate[c].max(0.0)) + o) / (stored(base[c]) + o)) as f32
        });
        if !gain.iter().all(|g| g.is_finite()) {
            return Err(NcError::Other(format!(
                "a gain map's gain overflows at pixel {index}: HDR {alternate:?} over SDR \
                 {base:?}"
            )));
        }
        Ok(gain)
    })?;
    // `min` and `max` are exact, so their fold order cannot move the result.
    let (mut min, mut max) = ([f32::INFINITY; 3], [f32::NEG_INFINITY; 3]);
    for px in pixels::triples(&rgb)? {
        for c in 0..3 {
            min[c] = min[c].min(px[c]);
            max[c] = max[c].max(px[c]);
        }
    }
    if rgb.is_empty() {
        (min, max) = ([1.0; 3], [1.0; 3]);
    }
    let flat = min == [1.0; 3] && max == [1.0; 3];
    Ok(GainRatios {
        width: sdr.width,
        height: sdr.height,
        rgb,
        offset,
        range: GainRange { min, max, flat },
    })
}

/// An SDR sample as the encoder stores it.
fn stored(sample: f32) -> f64 {
    f64::from(sample.clamp(0.0, 1.0))
}

fn check_dimensions(image: &LinearImage, width: u32, height: u32) -> Result<()> {
    if (image.width, image.height) != (width, height) {
        return Err(NcError::Other(format!(
            "a gain map's renditions differ in size: {}×{} vs {width}×{height}",
            image.width, image.height
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const OFFSET: f32 = 1.0 / 64.0;

    fn image(rgb: &[f32]) -> LinearImage {
        LinearImage::new((rgb.len() / 3) as u32, 1, rgb.to_vec(), None).unwrap()
    }

    #[test]
    fn an_identical_pair_is_flat_with_every_gain_exactly_one() {
        let rgb = [0.0, 0.0, 0.0, 0.18, 0.2, 0.3, 1.0, 0.4, 0.0];
        let gains = between(&image(&rgb), &image(&rgb), OFFSET).unwrap();
        assert!(gains.rgb().iter().all(|g| *g == 1.0), "{:?}", gains.rgb());
        assert_eq!(
            gains.range(),
            GainRange {
                min: [1.0; 3],
                max: [1.0; 3],
                flat: true
            }
        );
    }

    #[test]
    fn one_differing_sample_makes_the_map_live() {
        // Falsifiability for the flat case: one channel of one pixel is enough.
        let sdr = [0.18, 0.18, 0.18, 1.0, 0.4, 0.3];
        let mut hdr = sdr;
        hdr[3] = 1.3;
        let range = between(&image(&sdr), &image(&hdr), OFFSET).unwrap().range();
        assert!(!range.flat, "{range:?}");
        assert!(range.max[0] > 1.0 && range.min[0] == 1.0, "{range:?}");
        assert_eq!((range.min[1], range.max[1]), (1.0, 1.0), "{range:?}");
    }

    #[test]
    fn the_gains_rebuild_the_hdr_rendition_from_the_stored_base() {
        // A saturated colour the SDR cube pulled in (gains both above and below 1),
        // a highlight above SDR's white (the base is stored clamped), and black.
        let sdr = [1.0, 0.4475, 0.3872, 1.2, 1.2, 1.2, 0.0, 0.0, 0.0];
        let hdr = [1.3692, 0.3432, 0.2312, 3.5, 3.5, 3.5, 0.0, 0.0, 0.0];
        let gains = between(&image(&sdr), &image(&hdr), OFFSET).unwrap();
        let rebuilt = gains.apply_to(&image(&sdr)).unwrap();
        for (got, want) in rebuilt.iter().zip(hdr) {
            assert!((got - want).abs() < 1e-6, "{rebuilt:?} vs {hdr:?}");
        }
        let range = gains.range();
        assert!(range.min[1] < 1.0 && range.max[0] > 1.0, "{range:?}");
    }

    #[test]
    fn the_gain_is_ratioed_against_the_clamped_base() {
        // Against the unclamped 1.2 the highlight would rebuild dark by the clamped
        // fraction; against the stored 1.0 it rebuilds exactly.
        let (sdr, hdr) = ([1.2f32; 3], [3.5f32; 3]);
        let gain = between(&image(&sdr), &image(&hdr), OFFSET).unwrap().rgb()[0];
        let o = f64::from(OFFSET);
        assert_eq!(gain, ((3.5 + o) / (1.0 + o)) as f32);
        assert_ne!(gain, ((3.5 + o) / (1.2 + o)) as f32);
    }

    #[test]
    fn a_sample_the_chain_cannot_write_is_refused_naming_the_pixel() {
        let good = [0.5; 6];
        for (bad, name) in [(f32::NAN, "HDR"), (f32::INFINITY, "SDR")] {
            let mut broken = good;
            broken[4] = bad;
            let (sdr, hdr) = if name == "SDR" {
                (broken, good)
            } else {
                (good, broken)
            };
            let err = between(&image(&sdr), &image(&hdr), OFFSET).unwrap_err();
            let msg = err.message();
            assert!(msg.contains(name) && msg.contains("pixel 1"), "{msg}");
        }
    }

    #[test]
    fn a_gain_too_large_for_f32_is_refused_naming_the_pixel() {
        // Both samples finite, the ratio not: the invariant is on the gain, not only on
        // its inputs.
        let (sdr, hdr) = (
            [0.5, 0.5, 0.5, 0.0, 0.5, 0.5],
            [0.5, 0.5, 0.5, f32::MAX, 0.5, 0.5],
        );
        let err = between(&image(&sdr), &image(&hdr), OFFSET).unwrap_err();
        let msg = err.message();
        assert!(
            msg.contains("overflows") && msg.contains("pixel 1"),
            "{msg}"
        );
    }

    #[test]
    fn a_rounding_negative_is_taken_as_stored_not_refused() {
        // Where two channels tie on the black face, fit gamut can leave one an ulp
        // below zero. The gain treats it as the zero an encoder stores.
        let (sdr, hdr) = ([0.5, -1e-17, 0.2], [0.5, -1e-17, 0.2]);
        let gains = between(&image(&sdr), &image(&hdr), OFFSET).unwrap();
        assert_eq!(gains.rgb(), &[1.0; 3]);
        assert!(gains.range().flat);
    }

    #[test]
    fn mismatched_renditions_and_bad_offsets_are_refused() {
        let one = image(&[0.5; 3]);
        let two = image(&[0.5; 6]);
        assert!(
            between(&one, &two, OFFSET)
                .unwrap_err()
                .message()
                .contains("differ in size")
        );
        for offset in [0.0, -1.0, f32::NAN] {
            assert!(
                between(&one, &one, offset)
                    .unwrap_err()
                    .message()
                    .contains("offset")
            );
        }
    }
}
