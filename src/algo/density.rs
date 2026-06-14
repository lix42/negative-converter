//! `density` — density-domain inversion (Cineon / negadoctor style). Default.
//!
//! Density conversion and print rendering are **separate** sub-stages (core
//! fidelity rule); this owns only the density-domain conversion.
//!
//! Stub: implemented by the `algo-density` task.

use crate::algo::Converter;
use crate::types::{DensityParams, FilmBase, LinearImage, Result};

/// Density-domain converter configured by [`DensityParams`].
pub struct Density {
    pub params: DensityParams,
}

impl Converter for Density {
    fn convert(&self, _image: &LinearImage, _base: &FilmBase) -> Result<LinearImage> {
        todo!("algo-density: density-domain inversion (separate from print render)")
    }
}
