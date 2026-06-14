//! Working-space → output color transforms via lcms2; depth-aware default
//! profile selection and the ICC blob to embed.
//!
//! Stub: implemented by the `color-management` task.

use crate::types::{LinearImage, OutputParams, Result};

/// Transform `image` from the linear working space into the output profile
/// selected by `params`, returning the converted image and the ICC blob to
/// embed at encode time.
pub fn to_output(_image: &LinearImage, _params: &OutputParams) -> Result<(LinearImage, Vec<u8>)> {
    todo!("color-management: working->output ICC transform + profile blob")
}
