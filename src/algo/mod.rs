//! Pluggable negative→positive converters behind a [`Converter`] trait.
//!
//! The trait is finalized by the `algo-interface` task; `simple` and `density`
//! are its two Step-1 implementations. Density is the default.

pub mod density;
pub mod simple;

use crate::types::{FilmBase, LinearImage, Result};

/// A negative→positive conversion algorithm. Implementations are pure: given the
/// decoded image and the estimated film base, they produce a positive image in
/// the linear working space (print rendering stays a separate sub-stage).
pub trait Converter {
    /// Convert `image` to a positive, using `base` as the `Dmin` anchor.
    fn convert(&self, image: &LinearImage, base: &FilmBase) -> Result<LinearImage>;
}
