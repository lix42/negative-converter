//! `Dmin` / film-base estimation from the unexposed border (pure).
//!
//! Stub: implemented by the `film-base-estimation` task.

use crate::types::{FilmBase, FilmBaseParams, LinearImage, Result};

/// Resolve the film base for `image`: use the explicit override if given,
/// otherwise estimate it from the configured/auto-detected border region.
pub fn estimate(_image: &LinearImage, _params: &FilmBaseParams) -> Result<FilmBase> {
    todo!("film-base-estimation: estimate Dmin from border / apply override")
}
