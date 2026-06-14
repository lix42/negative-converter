//! [`LinearImage`] → 16-bit / 32-bit-float TIFF, embedded ICC, sidecar JSON,
//! optional IR export.
//!
//! Stub: implemented by the `tiff-encode` task.

use std::path::Path;

use crate::types::{LinearImage, OutputParams, Result};

/// Encode `image` to a TIFF at `path` per `params` (depth, profile, BigTIFF).
pub fn encode(_image: &LinearImage, _params: &OutputParams, _path: &Path) -> Result<()> {
    todo!("tiff-encode: write u16/f32 TIFF with ICC + sidecar")
}
