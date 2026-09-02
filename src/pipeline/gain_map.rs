//! Canonical gain-map construction in common linear Display P3.
//!
//! This stage deliberately stops before JPEG/ISO serialization. It owns the
//! typed common-domain boundary, BT.2020 → Display P3 conversion, compatibility
//! gamut mapping, and the exact offset-adjusted gain calculation. Container
//! metadata serializers must consume this one canonical result rather than
//! independently re-deriving gains or unit conversions. The future serializer
//! consumes [`GainMapRender`] directly; only a serializer inside this module (or
//! a child module) may destructure its private fields.

use serde::Serialize;

pub(crate) mod iso;

use crate::pipeline::colorimetry::pinned::{BT2020_TO_DISPLAY_P3, DISPLAY_P3_LUMA};
use crate::pipeline::display_tone::DisplayTone;
use crate::pipeline::hdr::{LINEAR_HEADROOM, LinearBt2020Hdr, REFERENCE_WHITE_NITS, render_linear};
use crate::pipeline::render_split::SharedDisplaySource;
use crate::pipeline::sdr::{RenderedSdr, SdrGamut, render as render_sdr};
use crate::types::{LinearImage, NcError, Result};

const ULTRA_HDR_V1_OFFSET: f32 = 1.0 / 64.0;

// BT.2020/D65 → XYZ/D65 → Display P3/D65, and the Display P3 luma weights. Both
// are defined once in `colorimetry::pinned` (imported above). The renderer inputs
// and output are both display-linear and reference-white-relative; no chromatic
// adaptation or transfer function belongs at this boundary, and the pinned matrix
// was derived with the adaptation term skipped for exactly that reason.
//
// `ULTRA_HDR_V1_OFFSET` stays here: it is **product/format policy**, not
// colorimetry, and policy belongs with the stage that owns it.

/// Fixed construction inputs which are part of the encoded format policy, not
/// user-facing conversion controls.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GainMapConfig {
    pub tone: DisplayTone,
    pub offset_sdr: [f32; 3],
    pub offset_hdr: [f32; 3],
    pub gain_gamma: [f32; 3],
}

impl GainMapConfig {
    /// Fixed public Ultra HDR v1 construction policy. Only the display tone
    /// remains a resolved print control; offsets and encoding gamma are format
    /// constants, not conversion knobs.
    pub fn ultra_hdr_v1(tone: DisplayTone) -> Self {
        Self {
            tone,
            offset_sdr: [ULTRA_HDR_V1_OFFSET; 3],
            offset_hdr: [ULTRA_HDR_V1_OFFSET; 3],
            gain_gamma: [1.0; 3],
        }
    }
}

/// Canonical metadata shared by every container dialect.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct GainMapMetadata {
    pub offset_sdr: [f32; 3],
    pub offset_hdr: [f32; 3],
    pub gain_gamma: [f32; 3],
    pub gain_min: [f32; 3],
    pub gain_max: [f32; 3],
    pub display_headroom_linear: f32,
    pub display_headroom_log2: f32,
    pub reference_white_nits: f32,
    pub common_primaries: &'static str,
    pub common_domain: &'static str,
    pub hdr_gamut_mapping: &'static str,
    pub gain_formula: &'static str,
}

/// The independently rendered SDR base, common-domain HDR rendition, and
/// canonical full-resolution RGB gains.
///
/// Keeping the three buffers together prevents a container encoder from pairing
/// a gain map with a different SDR base. The caller may downsample/quantize the
/// gain buffer only according to the selected, independently verified container
/// contract.
#[derive(Debug)]
pub struct GainMapRender {
    sdr: RenderedSdr,
    hdr_display_p3: CommonLinearDisplayP3Hdr,
    gain: GainRatioImage,
    metadata: GainMapMetadata,
}

/// Opaque common-domain HDR pixels in reference-white-relative linear Display P3.
#[derive(Debug)]
pub struct CommonLinearDisplayP3Hdr {
    image: LinearImage,
}

impl CommonLinearDisplayP3Hdr {
    #[cfg(test)]
    pub fn width(&self) -> u32 {
        self.image.width
    }

    #[cfg(test)]
    pub fn height(&self) -> u32 {
        self.image.height
    }

    #[cfg(test)]
    pub fn rgb(&self) -> &[f32] {
        &self.image.rgb
    }
}

/// Opaque, positive per-channel ratios before gain-map encoding.
#[derive(Debug)]
pub struct GainRatioImage {
    image: LinearImage,
}

impl GainRatioImage {
    pub fn width(&self) -> u32 {
        self.image.width
    }

    pub fn height(&self) -> u32 {
        self.image.height
    }

    #[cfg(test)]
    pub fn rgb(&self) -> &[f32] {
        &self.image.rgb
    }
}

impl GainMapRender {
    #[cfg(test)]
    pub fn sdr(&self) -> &RenderedSdr {
        &self.sdr
    }

    #[cfg(test)]
    pub fn hdr_display_p3(&self) -> &CommonLinearDisplayP3Hdr {
        &self.hdr_display_p3
    }

    #[cfg(test)]
    pub fn gain(&self) -> &GainRatioImage {
        &self.gain
    }

    #[cfg(test)]
    pub fn metadata(&self) -> &GainMapMetadata {
        &self.metadata
    }

    pub(crate) fn into_sdr(self) -> RenderedSdr {
        let Self {
            sdr,
            hdr_display_p3,
            ..
        } = self;
        let CommonLinearDisplayP3Hdr { image } = hdr_display_p3;
        drop(image);
        sdr
    }
}

/// Half-resolution, normalized 8-bit luminance gain-map pixels ready for JPEG
/// compression. Metadata remains in its canonical linear representation.
pub(crate) struct EncodedGainMap {
    pub width: u32,
    pub height: u32,
    pub samples: Vec<u8>,
    pub metadata: GainMapMetadata,
}

/// Render both display branches from one shared adjusted source and derive the
/// canonical gain map in reference-white-relative linear Display P3.
pub fn render(shared: &SharedDisplaySource, config: GainMapConfig) -> Result<GainMapRender> {
    validate_config(config)?;
    let sdr = render_sdr(shared, SdrGamut::DisplayP3, config.tone)?;
    let hdr = render_linear(shared, config.tone)?;
    build(sdr, hdr, config)
}

/// Encode canonical gain ratios into the legacy Ultra HDR v1 normalized-log
/// representation, then downsample by two with bilinear filtering.
pub(crate) fn encode_legacy_gain_map(render: &GainMapRender) -> Result<EncodedGainMap> {
    let width = render.gain.width();
    let height = render.gain.height();
    let out_width = width.div_ceil(2);
    let out_height = height.div_ceil(2);
    let policy = render.metadata;
    if policy.offset_sdr != [policy.offset_sdr[0]; 3]
        || policy.offset_hdr != [policy.offset_hdr[0]; 3]
        || policy.gain_gamma != [policy.gain_gamma[0]; 3]
    {
        return Err(NcError::Other(
            "legacy Ultra HDR v1 needs identical gain-map offsets and gamma on all channels".into(),
        ));
    }

    // Legacy XMP mode supports one luminance gain channel. Derive it from the
    // paired common-P3 renditions; keep the canonical RGB ratios untouched for
    // the future ISO serializer.
    let mut ratios = Vec::with_capacity((width * height) as usize);
    let mut gain_min = f32::INFINITY;
    let mut gain_max = f32::NEG_INFINITY;
    for (sdr, hdr) in render
        .sdr
        .image()
        .rgb
        .as_chunks::<3>()
        .0
        .iter()
        .zip(render.hdr_display_p3.image.rgb.as_chunks::<3>().0)
    {
        let sdr_luma = dot(*sdr, DISPLAY_P3_LUMA);
        let hdr_luma = dot(*hdr, DISPLAY_P3_LUMA);
        let ratio = (hdr_luma + policy.offset_hdr[0]) / (sdr_luma + policy.offset_sdr[0]);
        if !ratio.is_finite() || ratio <= 0.0 {
            return Err(NcError::Other(
                "legacy Ultra HDR luminance gain is non-finite or non-positive".into(),
            ));
        }
        gain_min = gain_min.min(ratio);
        gain_max = gain_max.max(ratio);
        ratios.push(ratio);
    }
    let log_min = gain_min.log2();
    // A spatially constant map still needs a finite normalization denominator.
    let log_span = (gain_max.log2() - log_min).max(1.0 / 255.0);

    let normalized = ratios
        .iter()
        .map(|ratio| normalize_log_gain(*ratio, log_min, log_span, policy.gain_gamma[0]))
        .collect::<Vec<_>>();
    let mut samples = Vec::with_capacity((out_width * out_height) as usize);
    for y in 0..out_height {
        for x in 0..out_width {
            let value = bilinear_sample(&normalized, width, height, out_width, out_height, x, y);
            samples.push((value * 255.0).round() as u8);
        }
    }

    let gain_max = 2.0f32.powf(log_min + log_span);
    Ok(EncodedGainMap {
        width: out_width,
        height: out_height,
        samples,
        metadata: GainMapMetadata {
            offset_sdr: [policy.offset_sdr[0]; 3],
            offset_hdr: [policy.offset_hdr[0]; 3],
            gain_gamma: [policy.gain_gamma[0]; 3],
            gain_min: [gain_min; 3],
            gain_max: [gain_max; 3],
            ..policy
        },
    })
}

fn normalize_log_gain(ratio: f32, log_min: f32, log_span: f32, gamma: f32) -> f32 {
    ((ratio.log2() - log_min) / log_span)
        .clamp(0.0, 1.0)
        .powf(gamma)
}

/// Centre-aligned resampling taps, shared by both metadata dialects.
///
/// **Deliberate divergence from a non-normative preference.** ISO 21496-1 6.2.2
/// NOTE 1 prefers a co-sited phase offset (H.265 ChromaLoc type 2); this is
/// centre-aligned. The NOTE is informative, so co-siting is a quality
/// preference rather than a conformance requirement, and switching would change
/// the bytes of the already-shipped `ultra-hdr-v1` output for no measured gain.
/// Revisit only with evidence that a real decoder's reconstruction suffers —
/// and if it changes, both dialects must move together, since they share this
/// function.
fn resample_axis(index: u32, dst_len: u32, src_len: u32) -> (u32, u32, f32) {
    let coordinate = (((f64::from(index) + 0.5) * f64::from(src_len) / f64::from(dst_len)) - 0.5)
        .clamp(0.0, f64::from(src_len - 1));
    let lower = coordinate.floor() as u32;
    let upper = (lower + 1).min(src_len - 1);
    (lower, upper, (coordinate - f64::from(lower)) as f32)
}

fn bilinear_sample(
    samples: &[f32],
    src_width: u32,
    src_height: u32,
    dst_width: u32,
    dst_height: u32,
    x: u32,
    y: u32,
) -> f32 {
    let (x0, x1, tx) = resample_axis(x, dst_width, src_width);
    let (y0, y1, ty) = resample_axis(y, dst_height, src_height);
    let at = |sx: u32, sy: u32| samples[(sy * src_width + sx) as usize];
    let top = at(x0, y0) + (at(x1, y0) - at(x0, y0)) * tx;
    let bottom = at(x0, y1) + (at(x1, y1) - at(x0, y1)) * tx;
    top + (bottom - top) * ty
}

fn validate_config(config: GainMapConfig) -> Result<()> {
    // The tone needs no check here: `DisplayTone` validates its knee width at
    // construction, and both renderers are handed the same resolved value.
    for (name, values) in [("SDR", config.offset_sdr), ("HDR", config.offset_hdr)] {
        for (channel, value) in values.into_iter().enumerate() {
            if !value.is_finite() || value <= 0.0 {
                return Err(NcError::Usage(format!(
                    "gain-map {name} offset channel {channel} must be finite and positive \
                     (got {value})"
                )));
            }
        }
    }
    for (channel, value) in config.gain_gamma.into_iter().enumerate() {
        if !value.is_finite() || value <= 0.0 {
            return Err(NcError::Usage(format!(
                "gain-map gamma channel {channel} must be finite and positive (got {value})"
            )));
        }
    }
    Ok(())
}

fn build(sdr: RenderedSdr, hdr: LinearBt2020Hdr, config: GainMapConfig) -> Result<GainMapRender> {
    if sdr.metadata().gamut != SdrGamut::DisplayP3 {
        return Err(NcError::Other(
            "gain-map SDR base must be rendered in linear Display P3".to_string(),
        ));
    }
    validate_common_domain(
        sdr.metadata().linear_domain,
        hdr.metadata().linear_domain,
        sdr.metadata().reference_white_nits,
        hdr.metadata().reference_white_nits,
    )?;
    if sdr.image().width != hdr.image().width || sdr.image().height != hdr.image().height {
        return Err(NcError::Other(format!(
            "gain-map rendition dimensions differ: SDR is {}x{}, HDR is {}x{}",
            sdr.image().width,
            sdr.image().height,
            hdr.image().width,
            hdr.image().height
        )));
    }

    let mut hdr_p3 = Vec::with_capacity(hdr.image().rgb.len());
    let mut gains = Vec::with_capacity(hdr.image().rgb.len());
    let mut gain_min = [f32::INFINITY; 3];
    let mut gain_max = [f32::NEG_INFINITY; 3];

    for (index, (sdr_px, hdr_px)) in sdr
        .image()
        .rgb
        .as_chunks::<3>()
        .0
        .iter()
        .zip(hdr.image().rgb.as_chunks::<3>().0)
        .enumerate()
    {
        validate_non_negative_finite("SDR", *sdr_px, index)?;
        validate_non_negative_finite("HDR", *hdr_px, index)?;
        let converted = mul(BT2020_TO_DISPLAY_P3, *hdr_px);
        if !converted.iter().all(|value| value.is_finite()) {
            return Err(NcError::Other(format!(
                "gain-map BT.2020 to Display P3 conversion produced a non-finite sample at \
                 pixel {index}"
            )));
        }
        let luminance = dot(converted, DISPLAY_P3_LUMA);
        if !luminance.is_finite() || !(0.0..=LINEAR_HEADROOM).contains(&luminance) {
            return Err(NcError::Other(format!(
                "gain-map Display P3 HDR luminance is outside the reference-white-relative \
                 display range at pixel {index}"
            )));
        }
        let mapped = gamut_map(converted, luminance, LINEAR_HEADROOM);
        validate_non_negative_finite("common-domain HDR", mapped, index)?;

        let mut gain_px = [0.0; 3];
        for channel in 0..3 {
            let denominator = sdr_px[channel] + config.offset_sdr[channel];
            let numerator = mapped[channel] + config.offset_hdr[channel];
            let gain = numerator / denominator;
            if !denominator.is_finite()
                || denominator <= 0.0
                || !numerator.is_finite()
                || numerator <= 0.0
                || !gain.is_finite()
                || gain <= 0.0
            {
                return Err(NcError::Other(format!(
                    "gain-map formula produced an invalid value at pixel {index}, channel \
                     {channel}"
                )));
            }
            gain_px[channel] = gain;
            gain_min[channel] = gain_min[channel].min(gain);
            gain_max[channel] = gain_max[channel].max(gain);
        }
        hdr_p3.extend_from_slice(&mapped);
        gains.extend_from_slice(&gain_px);
    }

    let width = sdr.image().width;
    let height = sdr.image().height;
    let hdr_display_p3 = CommonLinearDisplayP3Hdr {
        image: LinearImage::new(width, height, hdr_p3, None)?,
    };
    let gain = GainRatioImage {
        image: LinearImage::new(width, height, gains, None)?,
    };
    Ok(GainMapRender {
        sdr,
        hdr_display_p3,
        gain,
        metadata: GainMapMetadata {
            offset_sdr: config.offset_sdr,
            offset_hdr: config.offset_hdr,
            gain_gamma: config.gain_gamma,
            gain_min,
            gain_max,
            display_headroom_linear: LINEAR_HEADROOM,
            display_headroom_log2: LINEAR_HEADROOM.log2(),
            reference_white_nits: REFERENCE_WHITE_NITS,
            common_primaries: "display-p3-d65",
            common_domain: "linear-relative-to-203-nit-reference-white",
            hdr_gamut_mapping: "display-p3-neutral-axis-radial-boundary-v1",
            gain_formula: "(hdr+offset_hdr)/(sdr+offset_sdr)",
        },
    })
}

fn validate_common_domain(
    sdr_domain: &str,
    hdr_domain: &str,
    sdr_reference_white: f32,
    hdr_reference_white: f32,
) -> Result<()> {
    const SDR_DOMAIN: &str = "display-linear-relative-to-203-nit-reference-white";
    const HDR_DOMAIN: &str = "bt2020-linear-relative-to-203-nit-reference-white";
    if sdr_domain != SDR_DOMAIN
        || hdr_domain != HDR_DOMAIN
        || sdr_reference_white.to_bits() != hdr_reference_white.to_bits()
        || sdr_reference_white.to_bits() != REFERENCE_WHITE_NITS.to_bits()
    {
        return Err(NcError::Other(
            "gain-map renditions must use the same 203-nit reference-white-relative linear \
             domain; absolute-nit or encoded inputs cannot be mixed"
                .to_string(),
        ));
    }
    Ok(())
}

fn validate_non_negative_finite(name: &str, pixel: [f32; 3], index: usize) -> Result<()> {
    if !pixel.iter().all(|value| value.is_finite() && *value >= 0.0) {
        return Err(NcError::Other(format!(
            "gain-map {name} rendition contains a negative or non-finite sample at pixel {index}"
        )));
    }
    Ok(())
}

/// Same-luminance radial mapping into the Display P3 compatibility cube.
fn gamut_map(rgb: [f32; 3], luminance: f32, maximum: f32) -> [f32; 3] {
    let neutral = f64::from(luminance);
    let upper = f64::from(maximum);
    let delta = rgb.map(|channel| f64::from(channel) - neutral);
    let mut scale = 1.0_f64;
    let mut limiting_boundary = None;
    for (channel, d) in delta.into_iter().enumerate() {
        let candidate = if d > 0.0 {
            Some(((upper - neutral) / d, maximum))
        } else if d < 0.0 {
            Some((-neutral / d, 0.0))
        } else {
            None
        };
        if let Some((candidate, boundary)) = candidate
            && candidate < scale
        {
            scale = candidate;
            limiting_boundary = Some((channel, boundary));
        }
    }
    let mut output = delta.map(|d| (neutral + scale * d) as f32);
    if let Some((channel, boundary)) = limiting_boundary {
        output[channel] = boundary;
    }
    output
}

fn mul(matrix: [[f32; 3]; 3], value: [f32; 3]) -> [f32; 3] {
    [
        dot(matrix[0], value),
        dot(matrix[1], value),
        dot(matrix[2], value),
    ]
}

fn dot(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::algo::reconstruct;
    use crate::pipeline::render_split::display_source;
    use crate::pipeline::working_space::map_nc_film_rgb_v1;
    use crate::types::{FilmBase, PrintParams, Reconstruction};

    fn close(actual: f32, expected: f32) {
        assert!((actual - expected).abs() < 2e-5, "{actual} != {expected}");
    }

    // `pub(super)` so the `iso` child module's tests build the same fixtures
    // rather than growing a second, drifting copy. `iso::tests` is a *sibling*
    // of this module, not a descendant, so it cannot see private items here;
    // `pub(super)` reaches exactly `gain_map` and its descendants, which is the
    // narrowest visibility that works.
    pub(super) fn shared_from_film_rgb(rgb: &[f32]) -> SharedDisplaySource {
        let scan = rgb.iter().map(|value| 1.0 - value).collect();
        let image = LinearImage::new((rgb.len() / 3) as u32, 1, scan, None).unwrap();
        let (film, _) =
            reconstruct(&image, &FilmBase::from([1.0; 3]), &Reconstruction::Simple).unwrap();
        display_source(map_nc_film_rgb_v1(film), &PrintParams::default()).unwrap()
    }

    pub(super) fn config() -> GainMapConfig {
        GainMapConfig {
            tone: DisplayTone::DEFAULT,
            offset_sdr: [1.0 / 64.0; 3],
            offset_hdr: [1.0 / 64.0; 3],
            gain_gamma: [1.0; 3],
        }
    }

    #[test]
    fn equal_reference_white_with_equal_offsets_has_unit_gain() {
        let shared = shared_from_film_rgb(&[1.0; 3]);
        let output = render(&shared, config()).unwrap();
        for gain in output.gain().rgb() {
            close(*gain, 1.0);
        }
    }

    #[test]
    fn without_a_display_tone_curve_a_bounded_source_gains_exactly_unity() {
        // Today the SDR shoulder *lifts* highlights above the untouched HDR
        // rendition, so the default gain map is below unity above the knee. Remove
        // the curve and both renditions carry the same luminance, so the map is
        // exactly flat — the inert default becomes inert by construction rather than
        // by the shoulder's arithmetic. Worth pinning: it is the interaction a reader
        // would otherwise have to derive.
        let ramp: Vec<f32> = [0.0, 0.18, 0.5, 0.9, 1.0]
            .into_iter()
            .flat_map(|v| [v; 3])
            .collect();
        let shared = shared_from_film_rgb(&ramp);
        let shouldered = render(&shared, config()).unwrap();
        assert!(
            shouldered.gain().rgb().iter().any(|gain| *gain < 0.99),
            "the shipped shoulder should move some gains off unity"
        );

        let linear = render(
            &shared,
            GainMapConfig {
                tone: DisplayTone::None,
                ..config()
            },
        )
        .unwrap();
        for gain in linear.gain().rgb() {
            close(*gain, 1.0);
        }
    }

    #[test]
    fn peak_gain_uses_actual_sdr_sample_and_offsets_not_display_headroom() {
        let shared = shared_from_film_rgb(&[LINEAR_HEADROOM; 3]);
        let output = render(&shared, config()).unwrap();
        let sdr = output.sdr().image().rgb[0];
        let hdr = output.hdr_display_p3().rgb()[0];
        let expected = (hdr + 1.0 / 64.0) / (sdr + 1.0 / 64.0);
        close(output.gain().rgb()[0], expected);
        assert!((output.gain().rgb()[0] - LINEAR_HEADROOM).abs() > 0.01);
    }

    #[test]
    fn black_and_near_black_have_finite_positive_gains() {
        let shared = shared_from_film_rgb(&[0.0, 0.0, 0.0, 1e-6, 2e-6, 3e-6]);
        let output = render(&shared, config()).unwrap();
        assert!(
            output
                .gain()
                .rgb()
                .iter()
                .all(|gain| gain.is_finite() && *gain > 0.0)
        );
        close(output.gain().rgb()[0], 1.0);
    }

    #[test]
    fn invalid_offsets_fail_before_gain_math() {
        for invalid in [0.0, -0.01, f32::NAN, f32::INFINITY] {
            let mut config = config();
            config.offset_sdr[1] = invalid;
            let error = render(&shared_from_film_rgb(&[0.5; 3]), config).unwrap_err();
            assert!(error.to_string().contains("finite and positive"));
        }
    }

    #[test]
    fn gain_gamma_is_validated_and_propagated_as_format_policy() {
        let mut policy = config();
        policy.gain_gamma = [1.0, 1.25, 0.8];
        let output = render(&shared_from_film_rgb(&[0.5; 3]), policy).unwrap();
        assert_eq!(output.metadata().gain_gamma, policy.gain_gamma);

        for invalid in [0.0, -0.01, f32::NAN, f32::INFINITY] {
            let mut invalid_policy = config();
            invalid_policy.gain_gamma[2] = invalid;
            let error = render(&shared_from_film_rgb(&[0.5; 3]), invalid_policy).unwrap_err();
            assert!(error.to_string().contains("gamma channel 2"));
            assert!(error.to_string().contains("finite and positive"));
        }
    }

    #[test]
    fn mixed_units_fail_with_stable_domain_diagnostic() {
        let error = validate_common_domain(
            "display-linear-relative-to-203-nit-reference-white",
            "bt2020-linear-absolute-nits",
            203.0,
            203.0,
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("same 203-nit reference-white-relative linear domain")
        );
    }

    #[test]
    fn bt2020_to_p3_matrix_preserves_neutral_axis_and_reference_scale() {
        for value in [0.0, 1.0, LINEAR_HEADROOM] {
            let p3 = mul(BT2020_TO_DISPLAY_P3, [value; 3]);
            for channel in p3 {
                close(channel, value);
            }
        }
    }

    #[test]
    fn bt2020_to_p3_matrix_matches_the_derivation_from_named_primaries() {
        // This test used to quote "independently calculated" reference vectors
        // that were in fact this matrix's own columns to the same digits — so it
        // could only ever restate the literal, never contradict it. It now
        // re-derives the matrix in binary64 from the named source definitions.
        //
        // Genuine independence lives in `colorimetry::tests`, which anchors the
        // same matrix on chromaticities recovered from the standards and on an
        // externally published matrix. Here the job is narrower and still worth
        // doing: prove *this stage* is multiplying by the matrix the colorimetry
        // module says it should, for the D65→D65 no-adaptation composition.
        use crate::pipeline::colorimetry::definitions::{BRADFORD, BT2020, DISPLAY_P3};
        use crate::pipeline::colorimetry::derive::rgb_to_rgb;

        let derived = rgb_to_rgb(BT2020, DISPLAY_P3, BRADFORD);
        for channel in 0..3 {
            let mut unit = [0.0_f32; 3];
            unit[channel] = 1.0;
            let actual = mul(BT2020_TO_DISPLAY_P3, unit);
            for row in 0..3 {
                close(actual[row], derived[row][channel] as f32);
            }
            // A primary must actually move: BT.2020 is wider than P3, so no
            // column can come through unchanged.
            assert_ne!(actual, unit);
        }
    }

    #[test]
    fn p3_compatibility_mapping_preserves_luminance_and_chroma_direction() {
        let input = [-0.5, 1.2, 0.4];
        let luminance = dot(input, DISPLAY_P3_LUMA);
        let output = gamut_map(input, luminance, LINEAR_HEADROOM);
        close(dot(output, DISPLAY_P3_LUMA), luminance);
        let before = input.map(|channel| channel - luminance);
        let after = output.map(|channel| channel - luminance);
        let scales = [
            after[0] / before[0],
            after[1] / before[1],
            after[2] / before[2],
        ];
        close(scales[0], scales[1]);
        close(scales[1], scales[2]);
        assert!(
            output
                .iter()
                .all(|value| (0.0..=LINEAR_HEADROOM).contains(value))
        );
    }

    #[test]
    fn metadata_extrema_come_from_actual_per_channel_gains() {
        let shared =
            shared_from_film_rgb(&[0.0, 0.0, 0.0, 0.18, 0.3, 0.5, 1.0, 1.0, 1.0, 3.0, 0.8, 0.2]);
        let output = render(&shared, config()).unwrap();
        for channel in 0..3 {
            let gains = output
                .gain()
                .rgb()
                .as_chunks::<3>()
                .0
                .iter()
                .map(|pixel| pixel[channel]);
            let (minimum, maximum) = gains.fold(
                (f32::INFINITY, f32::NEG_INFINITY),
                |(minimum, maximum), gain| (minimum.min(gain), maximum.max(gain)),
            );
            close(output.metadata().gain_min[channel], minimum);
            close(output.metadata().gain_max[channel], maximum);
        }
        close(
            output.metadata().display_headroom_log2,
            LINEAR_HEADROOM.log2(),
        );
    }

    #[test]
    fn legacy_xmp_map_is_half_resolution_single_channel_and_deterministic() {
        let shared = shared_from_film_rgb(&[0.0, 0.0, 0.0, 0.18, 0.3, 0.5, 2.0, 0.6, 0.1]);
        let first = render(&shared, config()).unwrap();
        let second = render(&shared, config()).unwrap();
        let first = encode_legacy_gain_map(&first).unwrap();
        let second = encode_legacy_gain_map(&second).unwrap();
        assert_eq!((first.width, first.height), (2, 1));
        assert_eq!(first.samples.len(), 2);
        assert_eq!(first.samples, second.samples);
        assert_eq!(first.metadata.gain_min, [first.metadata.gain_min[0]; 3]);
        assert_eq!(first.metadata.gain_max, [first.metadata.gain_max[0]; 3]);
        assert!(first.metadata.gain_min[0] > 0.0);
        assert!(first.metadata.gain_max[0] > first.metadata.gain_min[0]);
    }

    #[test]
    fn legacy_normalization_pins_log2_gamma_and_extrema_domain() {
        let log_min = 1.0_f32.log2();
        let log_span = 4.0_f32.log2() - log_min;
        close(normalize_log_gain(1.0, log_min, log_span, 1.0), 0.0);
        close(normalize_log_gain(2.0, log_min, log_span, 1.0), 0.5);
        close(normalize_log_gain(4.0, log_min, log_span, 1.0), 1.0);
        close(normalize_log_gain(2.0, log_min, log_span, 2.0), 0.25);
    }

    #[test]
    fn odd_size_bilinear_mapping_uses_actual_source_and_destination_lengths() {
        // A 5x3 linear ramp `x + 10y`, normalized by 24. Center-aligned 5→3
        // and 3→2 coordinates are x=[1/3, 2, 11/3], y=[1/4, 7/4].
        // Because bilinear interpolation reproduces a linear ramp exactly, this
        // independently pins both axes and catches the old hard-coded 2x taps.
        let samples = (0..3)
            .flat_map(|y| (0..5).map(move |x| (x + 10 * y) as f32 / 24.0))
            .collect::<Vec<_>>();
        let expected = [
            17.0 / 144.0,
            4.5 / 24.0,
            37.0 / 144.0,
            107.0 / 144.0,
            19.5 / 24.0,
            127.0 / 144.0,
        ];
        let mut actual = Vec::new();
        for y in 0..2 {
            for x in 0..3 {
                actual.push(bilinear_sample(&samples, 5, 3, 3, 2, x, y));
            }
        }
        for (actual, expected) in actual.into_iter().zip(expected) {
            close(actual, expected);
        }
    }

    #[test]
    fn render_is_deterministic_and_keeps_all_buffers_dimensionally_coupled() {
        let shared = shared_from_film_rgb(&[0.42, 0.18, 0.07, 2.0, 0.6, 0.1]);
        let first = render(&shared, config()).unwrap();
        let second = render(&shared, config()).unwrap();
        assert_eq!(
            first
                .gain()
                .rgb()
                .iter()
                .map(|value| value.to_bits())
                .collect::<Vec<_>>(),
            second
                .gain()
                .rgb()
                .iter()
                .map(|value| value.to_bits())
                .collect::<Vec<_>>()
        );
        assert_eq!(first.metadata(), second.metadata());
        assert_eq!(first.sdr().image().width, first.gain().width());
        assert_eq!(first.hdr_display_p3().width(), first.gain().width());
        assert_eq!(first.sdr().image().height, first.gain().height());
        assert_eq!(first.hdr_display_p3().height(), first.gain().height());

        let _: &CommonLinearDisplayP3Hdr = first.hdr_display_p3();
        let _: &GainRatioImage = first.gain();
        assert_eq!(first.metadata().gain_gamma, [1.0; 3]);

        fn consume_coupled_render(_: GainMapRender) {}
        consume_coupled_render(first);
    }
}
