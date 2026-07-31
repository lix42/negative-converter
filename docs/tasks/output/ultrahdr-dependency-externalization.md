# Externalize the Ultra HDR Native Dependency

## Goal

Replace NC's checked-in `vendor/ultrahdr-sys` native source snapshot with an
exact published Cargo dependency once a qualifying release is available. Reduce
repository size and manual dependency ownership without weakening build
reproducibility, output compatibility, licensing, or self-contained deployment.

This is deferred maintenance and blocks no output task. Until every readiness
condition below is satisfied, the reviewed local snapshot remains the supported
implementation.

## Design

Prefer an exact crates.io version of `ultrahdr-sys`, pinned both as an exact
Cargo requirement and in `Cargo.lock`. The selected release must:

- contain google/libultrahdr's corrected JPEG marker-order behavior from commit
  `11ac0c325bbf56ecf8be8704ff0f79fc9e1aac77` or a verified equivalent;
- expose the metadata feature selection NC requires, including Ultra HDR v1 XMP
  emission without falsely enabling or claiming final ISO metadata;
- build and statically link libultrahdr and its JPEG dependency without a
  machine-installed runtime library;
- perform no network download from its native CMake/build script—the Cargo
  package must already contain or deterministically declare all native inputs;
- support NC's Linux and macOS CI targets; and
- carry the complete license and notice material needed for binary distribution.

Do not depend directly on `google/libultrahdr`: it is a C++ project and does not
provide NC's Cargo FFI/build boundary. Do not replace the snapshot with an
unpinned branch, tag-only Git dependency, runtime system library, or build-time
source download. An exact-revision Git fork may be evaluated separately if a
published release remains unavailable, but adopting one requires an explicit
decision because it changes fresh-build availability and dependency custody; it
is not the default outcome of this task.

When a release qualifies:

- change `Cargo.toml` from the path dependency to the exact registry version and
  regenerate `Cargo.lock`;
- remove `vendor/ultrahdr-sys`, its snapshot manifest, and the native snapshot CI
  verifier, including the force-tracking guard required because the copied
  upstream `.gitignore` hides legitimate snapshot files;
- update CI prerequisites only where the published crate's documented build
  requirements differ;
- update `README.md`, `THIRD_PARTY_NOTICES.md`, and distribution-license copying
  so they refer to packaged dependency licenses rather than removed repository
  paths; and
- record the selected crate version, included libultrahdr/libjpeg-turbo
  revisions, feature set, and qualification evidence in the output progress log.

The migration must not change gain-map math, renderer behavior, CLI/recipe
semantics, metadata claims, or output preset defaults.

## How to Verify

- Inspect the published `.crate` archive and its build script before changing
  NC; record evidence for every readiness condition above.
- Start from an empty Cargo cache with normal Cargo registry access, then build
  without any secondary build-script download and without installed
  libultrahdr/libjpeg shared libraries.
- Confirm the final executable has no runtime dependency on libultrahdr or
  libjpeg on both Linux and macOS.
- Run the existing marker-order, XMP/MPF/GContainer, Display P3 ICC, gain-map
  reconstruction, odd-dimension, determinism, and failure-path tests unchanged.
- Generate a representative `ultra-hdr-v1` file and verify it with the
  independent metadata inspection and libultrahdr reconstruction checks used by
  `output/gain-map-hdr-output`.
- Verify the complete binary-distribution license bundle from a clean packaged
  build; no notice may reference a deleted local path.
- Run the full CI gate:
  `cargo fmt --all --check`,
  `cargo clippy --all-targets -- -D warnings`,
  `cargo build`, and `cargo test`.

## Dependencies

- [Ultra HDR v1 gain-map JPEG output](gain-map-hdr-output.md)
