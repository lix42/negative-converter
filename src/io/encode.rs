//! [`LinearImage`] → 16-bit / 32-bit-float TIFF, embedded ICC, sidecar JSON,
//! optional IR export.
//!
//! Stub: implemented by the `tiff-encode` task.

use std::path::Path;

use crate::types::{LinearImage, OutputParams, Result};

/// Encode `image` to a TIFF at `path` per `params` (depth, BigTIFF). `icc` is the
/// output-profile blob to embed — produced by `pipeline::color::to_output`, so
/// the encoder embeds exactly the profile the pixels were converted into rather
/// than re-resolving it. `None` embeds no profile.
pub fn encode(
    _image: &LinearImage,
    _params: &OutputParams,
    _icc: Option<&[u8]>,
    _path: &Path,
) -> Result<()> {
    todo!("tiff-encode: write u16/f32 TIFF with embedded ICC + sidecar")
}
