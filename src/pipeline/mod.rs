//! Pure pipeline stages between decode and encode: film-base estimation, color
//! transforms, and the stage wiring that threads them together.

pub mod color;
pub mod colorimetry;
pub mod film_base;
pub mod gain_map;
pub mod hdr;
pub mod input_semantics;
pub mod memory;
pub mod render_split;
pub mod sdr;
pub mod stages;
pub mod working_space;
