//! `density` — density-domain inversion (Cineon / negadoctor style). Default.
//!
//! Density conversion and print rendering are **separate** sub-stages (core
//! fidelity rule); this owns only the density-domain conversion.
//!
//! Stub: implemented by the `algo-density` task.

use crate::algo::Converter;
use crate::types::{DensityParams, FilmBase, LinearImage, PrintParams, Result};

/// Density-domain converter.
///
/// Holds **both** sub-stages' params: `density` (transmission→density correction)
/// and `print` (the separate print-render controls). Keeping them as distinct
/// fields preserves the core fidelity rule — the two are parameterized
/// independently even though one algorithm owns them.
pub struct Density {
    pub density: DensityParams,
    pub print: PrintParams,
}

impl Converter for Density {
    fn convert(&self, _image: &LinearImage, _base: &FilmBase) -> Result<LinearImage> {
        todo!("algo-density: density-domain inversion (separate from print render)")
    }
}
