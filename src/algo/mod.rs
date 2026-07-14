//! Pluggable negative→positive converters behind a [`Converter`] trait.
//!
//! The trait is finalized by the `algo-interface` task; `simple` and `density`
//! are its two Step-1 implementations. Density is the default.

pub mod density;
pub mod simple;

use std::str::FromStr;

use crate::algo::density::Density;
use crate::algo::simple::Simple;
use crate::types::{
    DensityParams, FilmBase, LinearImage, NcError, PrintParams, Result, SimpleParams,
};

/// A negative→positive conversion algorithm. Implementations are pure: given the
/// decoded image and the estimated film base, they produce a positive image in
/// the linear working space (print rendering stays a separate sub-stage).
///
/// The trait is deliberately object-safe (no associated `Params` type, params
/// live in the implementor) so [`build`] can hand back a `Box<dyn Converter>` and
/// the rest of the pipeline stays algorithm-agnostic. (The design-spec §7.2
/// sketch shows an associated-type variant; that is not object-safe and this task
/// supersedes it.)
pub trait Converter {
    /// Convert `image` to a positive, using `base` as the `Dmin` anchor.
    fn convert(&self, image: &LinearImage, base: &FilmBase) -> Result<LinearImage>;

    /// Convert and surface optional per-conversion diagnostics for the JSON report.
    ///
    /// This is the value-path a resolved-anchor (`Dmax`) or similar diagnostic
    /// rides back on — analogous to how `io::encode` returns an `EncodeReport`
    /// alongside its result: the algorithm *surfaces* values, the orchestrator
    /// *reports* them. It is a reporting channel, not a new control knob (controls
    /// still live in the param structs), so widening it doesn't reopen the
    /// object-safety / associated-`Params` question the trait settled.
    ///
    /// The default runs [`Converter::convert`] and reports nothing, so algorithms
    /// with no diagnostics (e.g. `simple`) need not implement it.
    fn convert_reported(
        &self,
        image: &LinearImage,
        base: &FilmBase,
    ) -> Result<(LinearImage, ConvertReport)> {
        Ok((self.convert(image, base)?, ConvertReport::default()))
    }
}

/// Optional per-conversion diagnostics an algorithm may surface for the JSON
/// report (see [`Converter::convert_reported`]). Empty by default.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ConvertReport {
    /// The resolved display-white anchor density (`Dmax`) the density render used,
    /// when one was applied. `None` for algorithms/config that don't anchor
    /// (`simple`, or `density` with `dmax = none`).
    pub dmax: Option<f32>,
}

/// The shipped Step-1 algorithms. `density` is the default.
///
/// The wired CLI/recipe surface standardized on the identical
/// [`crate::types::Algorithm`] (a neutral type with no `algo` dependency), so
/// the Step-1 binary doesn't consume this copy — it survives as the `algo`
/// module's own selector, exercised by the tests here and by `AlgoParams`.
/// Unifying the two enums is a follow-up cleanup.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Algorithm {
    /// Channel-inversion baseline (debug / B&W).
    Simple,
    /// Density-domain inversion (Cineon / negadoctor style). The default.
    #[default]
    Density,
}

impl FromStr for Algorithm {
    type Err = NcError;

    /// Parse the `--algorithm` value. Unknown names fail loudly as
    /// [`NcError::Usage`] (exit 2) rather than silently defaulting.
    fn from_str(s: &str) -> Result<Self> {
        match s.to_ascii_lowercase().as_str() {
            "simple" => Ok(Algorithm::Simple),
            "density" => Ok(Algorithm::Density),
            _ => Err(NcError::Usage(format!(
                "unknown algorithm '{s}' (expected: simple|density)"
            ))),
        }
    }
}

/// Per-algorithm parameter sets, tagged by the selected algorithm. Each variant
/// carries exactly the params its converter consumes; `Density` carries both
/// sub-stages' params (density correction + the separate print render).
#[derive(Clone, Debug, PartialEq)]
pub enum AlgoParams {
    Simple(SimpleParams),
    Density {
        density: DensityParams,
        print: PrintParams,
    },
}

impl AlgoParams {
    /// The algorithm this parameter set selects. Not consumed by the Step-1
    /// orchestrator (it derives the report's algorithm from the resolved config
    /// directly); kept as part of the `AlgoParams` API and exercised by tests.
    #[allow(dead_code)]
    pub fn algorithm(&self) -> Algorithm {
        match self {
            AlgoParams::Simple(_) => Algorithm::Simple,
            AlgoParams::Density { .. } => Algorithm::Density,
        }
    }
}

/// Build a boxed converter from its parameter set.
///
/// The `AlgoParams` variant *is* the algorithm selector (see
/// [`AlgoParams::algorithm`]), so construction is **total** — there is no
/// separate algorithm argument that could disagree with the params, hence no
/// failure mode and no `Result`. The CLI resolves `--algorithm` plus the
/// per-algorithm flags into a single `AlgoParams` (rejecting contradictory flags
/// there, where the flag context lives for a good error message); everything
/// below that boundary receives an already-valid value.
///
/// Takes `params` **by value** — the converter stores them, so it owns them
/// outright (no clone). A caller that still needs the params afterward (e.g. to
/// emit them in the JSON report) should clone at the call site.
pub fn build(params: AlgoParams) -> Box<dyn Converter> {
    match params {
        AlgoParams::Simple(params) => Box::new(Simple { params }),
        AlgoParams::Density { density, print } => Box::new(Density { density, print }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_str_parses_known_algorithms() {
        assert_eq!(Algorithm::from_str("simple").unwrap(), Algorithm::Simple);
        assert_eq!(Algorithm::from_str("density").unwrap(), Algorithm::Density);
    }

    #[test]
    fn from_str_rejects_unknown_name_as_usage() {
        let err = Algorithm::from_str("sigmoid").unwrap_err();
        assert_eq!(err.exit_code(), 2); // NcError::Usage
    }

    #[test]
    fn default_algorithm_is_density() {
        assert_eq!(Algorithm::default(), Algorithm::Density);
    }

    #[test]
    fn algorithm_serializes_lowercase() {
        assert_eq!(
            serde_json::to_string(&Algorithm::Density).unwrap(),
            "\"density\""
        );
        assert_eq!(
            serde_json::to_string(&Algorithm::Simple).unwrap(),
            "\"simple\""
        );
    }

    /// A trivial implementor proving the trait is object-safe and callable
    /// through `Box<dyn Converter>` (returns the input unchanged).
    struct Identity;
    impl Converter for Identity {
        fn convert(&self, image: &LinearImage, _base: &FilmBase) -> Result<LinearImage> {
            Ok(image.clone())
        }
    }

    #[test]
    fn trait_is_object_safe() {
        let converter: Box<dyn Converter> = Box::new(Identity);
        let img = LinearImage::new(1, 1, vec![0.1, 0.2, 0.3], None).unwrap();
        let base = FilmBase::from([1.0, 1.0, 1.0]);
        let out = converter.convert(&img, &base).unwrap();
        assert_eq!(out.rgb, img.rgb);
    }

    #[test]
    fn build_constructs_a_converter_for_each_variant() {
        // Smoke test: both variants build without panicking. `Converter` isn't
        // `Debug`, so we just exercise construction (and prove the match is
        // exhaustive over `AlgoParams`).
        let _simple = build(AlgoParams::Simple(SimpleParams::default()));
        let _density = build(AlgoParams::Density {
            density: DensityParams::default(),
            print: PrintParams::default(),
        });
    }

    #[test]
    fn algo_params_reports_its_algorithm() {
        assert_eq!(
            AlgoParams::Simple(SimpleParams::default()).algorithm(),
            Algorithm::Simple
        );
        assert_eq!(
            AlgoParams::Density {
                density: DensityParams::default(),
                print: PrintParams::default(),
            }
            .algorithm(),
            Algorithm::Density
        );
    }

    #[test]
    fn from_str_agrees_with_serde_wire_form() {
        // `FromStr` and serde's `rename_all = "lowercase"` map the same strings to
        // the same variants, but are written independently — pin them together so
        // adding a variant can't let the two drift apart.
        for algo in [Algorithm::Simple, Algorithm::Density] {
            let wire = serde_json::to_string(&algo).unwrap(); // e.g. "\"simple\""
            let name = wire.trim_matches('"');
            assert_eq!(Algorithm::from_str(name).unwrap(), algo);
            assert_eq!(serde_json::from_str::<Algorithm>(&wire).unwrap(), algo);
        }
    }
}
