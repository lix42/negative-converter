//! Pure pipeline stages between decode and encode: film-base estimation, color
//! transforms, and the stage wiring that threads them together.

pub mod color;
pub mod film_base;
pub mod input_semantics;
pub mod stages;
