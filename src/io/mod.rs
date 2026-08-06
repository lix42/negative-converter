//! Input/output stages: decode scanner files into [`LinearImage`] and encode
//! results out to TIFF. These are the only places crate-specific image/TIFF
//! types appear; everything else speaks the neutral types in [`crate::types`].

pub mod avif;
pub mod decode;
pub mod encode;
/// Temp-write → fsync → rename, so no truncated file ever appears at a final path.
/// Every writer in this module goes through it; see the module docs for the exact
/// guarantee (per-file atomicity, not a multi-file transaction).
pub mod staged;
pub mod ultra_hdr;
