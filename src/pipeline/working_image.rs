//! The pixel buffer every new-flow stage boundary carries.
//!
//! Each boundary in `pipeline::chain` is its own type — [`SceneReferredImage`],
//! [`GradedImage`], [`RangeFittedImage`], [`DisplayReferredImage`] — because each
//! names a *different* position in the chain, and the compiler refusing an
//! out-of-order composition is the whole point of the skeleton
//! (`nf-core/stage-skeleton`). What they share is one piece of knowledge, not a
//! coincidence of shape: what a full-frame working image **is** — interleaved
//! linear RGB, the carried IR plane, the length invariants that tie them to the
//! dimensions, and the rule that a `Debug` never prints pixels. That lives here,
//! once; the wrappers stay separate so they can diverge as their stages gain
//! parameters.
//!
//! [`SceneReferredImage`]: crate::pipeline::scene_correction::SceneReferredImage
//! [`GradedImage`]: crate::pipeline::look::GradedImage
//! [`RangeFittedImage`]: crate::pipeline::fit_range::RangeFittedImage
//! [`DisplayReferredImage`]: crate::pipeline::fit_gamut::DisplayReferredImage

use std::fmt;

use crate::pipeline::working_space::AcesCgImage;
use crate::types::LinearImage;

/// A full-frame working image in flight between two stages.
///
/// Visible only inside `pipeline`: it is the boundary types' shared payload, never
/// a value a caller outside the chain can hold. Values may leave `[0, 1]` — the
/// working range is preserved all the way to the encoder, which is the only place
/// clamping happens — and may be non-finite until fit range, which refuses them.
///
/// Deliberately not `Clone`: a full-frame copy is [`WorkingBuffer::copy`], named so
/// every call site is visible to the memory model (`pipeline::memory`).
pub(in crate::pipeline) struct WorkingBuffer {
    width: u32,
    height: u32,
    /// Interleaved `r,g,b`, `len == width * height * 3`.
    rgb: Vec<f32>,
    /// Carried-through IR plane (HDRi input), `len == width * height`.
    ir: Option<Vec<f32>>,
}

impl WorkingBuffer {
    /// A full-frame copy, the IR plane included when the scan has one. Its one caller
    /// is the SDR/HDR branch point (`look::GradedImage::split`), where a gain map needs
    /// both renditions; a new caller is a new full-frame buffer for
    /// `pipeline::memory`'s model.
    #[cfg_attr(not(test), allow(dead_code))] // the gain-map destination (`nf-destinations/gain-map-destination`), via `GradedImage::split`
    pub(in crate::pipeline) fn copy(&self) -> Self {
        Self {
            width: self.width,
            height: self.height,
            rgb: self.rgb.clone(),
            ir: self.ir.clone(),
        }
    }

    /// Take the buffers out of the chain's input [`AcesCgImage`].
    ///
    /// A **move**, not a copy: the `Vec`s are handed over, so entering the chain
    /// allocates nothing. That is what makes a distinct type per boundary free at
    /// runtime, and it is the property `nf-core/buffer-strategy` inherits when it
    /// decides what each stage does with the buffer it is given.
    pub(in crate::pipeline) fn from_aces(image: AcesCgImage) -> Self {
        // `into_linear` is `AcesCgImage`'s own consuming unwrap, so the length
        // invariants it validated on the way in still hold here by construction.
        //
        // It does **not** carry `LinearImage::ir_verified`, which that round trip
        // already resets: an `AcesCgImage` has never held the plane's provenance,
        // only the plane. Inherited rather than introduced here — whether the new
        // chain should carry it is `nf-core/buffer-strategy`'s to settle, and it
        // matters because a shape-only IR plane must not be trusted by a
        // conversion consumer even though it is still exportable.
        let linear = image.into_linear();
        Self {
            width: linear.width,
            height: linear.height,
            rgb: linear.rgb,
            ir: linear.ir,
        }
    }

    /// Unwrap into the plain working image — the **way out of the chain**, and
    /// the mirror of [`AcesCgImage::into_linear`], which is the way in.
    ///
    /// A move, like every boundary crossing above it: the encoder takes
    /// `&LinearImage`, so without this the only way off the last boundary would be
    /// to copy the buffers — ~0.9 GB on a 74.6 MP scan, on top of the peak
    /// `pipeline::memory` models.
    pub(in crate::pipeline) fn into_linear(self) -> LinearImage {
        // Same reasoning as `AcesCgImage::into_linear`: the invariants hold, but
        // route through the validated constructor anyway so a regression is loud.
        LinearImage::new(self.width, self.height, self.rgb, self.ir)
            .expect("a WorkingBuffer preserves the validated buffer-length invariants")
    }

    /// The interleaved `r,g,b` samples, for a stage that transforms them in place.
    pub(in crate::pipeline) fn rgb_mut(&mut self) -> &mut [f32] {
        &mut self.rgb
    }

    /// The boundary types' shared `Debug` body — dimensions and whether an IR
    /// plane rides along, **never** the pixel buffers (matching
    /// [`AcesCgImage`]'s own rule). `name` is the wrapping type's own name, so
    /// each boundary still prints as itself.
    pub(in crate::pipeline) fn fmt_named(
        &self,
        f: &mut fmt::Formatter<'_>,
        name: &str,
    ) -> fmt::Result {
        f.debug_struct(name)
            .field("width", &self.width)
            .field("height", &self.height)
            .field("ir", &self.ir.is_some())
            .finish_non_exhaustive()
    }
}
