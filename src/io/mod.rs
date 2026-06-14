//! Input/output stages: decode scanner files into [`LinearImage`] and encode
//! results out to TIFF. These are the only places crate-specific image/TIFF
//! types appear; everything else speaks the neutral types in [`crate::types`].

pub mod decode;
pub mod encode;
