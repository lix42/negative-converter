//! SilverFast HDR (48-bit RGB) / HDRi (64-bit RGB+IR) → [`LinearImage`].
//!
//! Stub: implemented by the `silverfast-decode` task. The IR plane is preserved
//! into [`LinearImage::ir`], never consumed (design-spec §6.1).

use std::path::Path;

use crate::types::{LinearImage, Result};

/// Decode a SilverFast HDR/HDRi TIFF at `path` into a linear `f32` image.
pub fn decode(_path: &Path) -> Result<LinearImage> {
    todo!("silverfast-decode: read HDR/HDRi TIFF into LinearImage")
}
