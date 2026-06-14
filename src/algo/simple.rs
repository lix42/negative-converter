//! `simple` — channel-inversion baseline (debug / B&W).
//!
//! Stub: implemented by the `algo-simple` task.

use crate::algo::Converter;
use crate::types::{FilmBase, LinearImage, Result, SimpleParams};

/// Channel-inversion converter configured by [`SimpleParams`].
pub struct Simple {
    pub params: SimpleParams,
}

impl Converter for Simple {
    fn convert(&self, _image: &LinearImage, _base: &FilmBase) -> Result<LinearImage> {
        todo!("algo-simple: inversion baseline with white balance + clip points")
    }
}
