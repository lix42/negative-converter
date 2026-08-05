# Remove the Ultra HDR Native Dependency

> **Task id unchanged** (`output/ultrahdr-dependency-externalization`) so its
> links, dependency entries, and append-only progress sections keep resolving.
> The *scope* changed on 2026-08-05: from "externalize the snapshot to a
> published crate" to "remove the native dependency from the tree entirely."
> See `docs/progress/output.md` for the investigation that forced the re-scope.

## Goal

Delete `vendor/ultrahdr-sys` and end nc's C/C++ dependency. nc writes the
Ultra HDR v1 XMP and CIPA Multi-Picture Format container itself, in Rust, so
`cargo build` and `cargo test` both need no CMake, clang, nasm, libjpeg, or
network fetch.

This is deferred maintenance and blocks no output task. Until it lands, the
reviewed local snapshot remains the supported implementation.

## Why not the published crate (the original plan)

Investigated 2026-08-05 against the published archive, per the old plan's own
verification step. `ultrahdr-sys` is a third-party wrapper
([`Enter-tainer/libultrahdr-rs`](https://github.com/Enter-tainer/libultrahdr-rs)),
not Google's, still at **0.1.5 (2026-04-29)**. Two independent problems:

1. **It predates the marker-order fix.** Its bundled `jpegr.cpp` has no APP0
   extraction (`grep -c "Extract APP0"` → 0). Adopting it would reintroduce the
   exact segment ordering that made macOS ImageIO reject our files.
2. **Structural, and unaffected by any version bump:** libultrahdr's CMake gets
   libjpeg-turbo via `ExternalProject_Add(GIT_REPOSITORY … GIT_TAG 3.1.0)`. With
   the crate's `vendored` feature that is a **build-time clone from GitHub at a
   mutable tag**; without it, the build links a **machine-installed** libjpeg
   (`cargo:rustc-link-lib=jpeg`). The first breaks pinning, the second breaks the
   self-contained binary and makes output vary per user machine. The `GIT_TAG`
   line lives inside the crate's own bundled `CMakeLists.txt`, and
   `ExternalProject_Add` exposes no cache-variable override for it (unlike
   `FetchContent`'s `FETCHCONTENT_SOURCE_DIR_*`), so we cannot pin it without
   forking — which is a local copy again.

Our snapshot exists precisely to solve (2): `libultrahdr/CMakeLists.txt` is the
**one file we modified** from upstream `11ac0c3`, replacing both libjpeg-turbo
`GIT_REPOSITORY`/`GIT_TAG` blocks with `DOWNLOAD_COMMAND ""` so the build
consumes the in-tree `third_party/turbojpeg`. Everything else, `jpegr.cpp`
included, is verbatim upstream (verified by diff).

The repository-size motive that framed the old plan does not survive
measurement: the whole pack is **14.36 MiB**. The real cost is the maintenance
apparatus — 782 tracked files, the force-tracking guard, and
`scripts/check-vendored-native.py` — not bytes.

## Design

**Production becomes pure Rust.** Only six native calls are on the shipping
path — `uhdr_create_encoder`, `uhdr_enc_set_compressed_image`,
`uhdr_enc_set_gainmap_image`, `uhdr_encode`, `uhdr_get_encoded_stream`,
`uhdr_release_encoder` — and together they do one job: write XMP plus MPF and
concatenate two JPEGs nc already encoded itself. Replace that with nc's own
assembly:

- **Both JPEG images are already ours** (`jpeg_encoder`, pure Rust). libjpeg-turbo
  is present only for libultrahdr's internal use, so it leaves with it.
- **Write the legacy XMP ourselves** — the `hdrgm` gain-map packet and the
  GContainer directory, whose exact strings existing tests already assert.
- **Write MPF ourselves.** `io::ultra_hdr` already parses and patches the MP Index
  IFD; emitting it is the same structure, and doing so retires
  `insert_baseline_iso_segment` — with assembly under our control, both ISO
  segments are placed directly instead of spliced in afterwards.
- **Keep the ISO serializers unchanged.** `pipeline::gain_map::iso` owns
  C.2.2/C.4.6 and is already independent of the container.

**Segment order becomes ours to state rather than inherit.** Today the shipped
file's ordering depends on libultrahdr's APP0-extraction fix; assembling the
file ourselves removes that whole class of bug, and the existing marker-order
test becomes an assertion about our own writer.

**The reconstruction oracle is replaced, not relocated.** 29 of the module's 46
`uhdr::` references are the decode-and-verify oracle. It must not survive as a
dev-dependency: `cargo test` builds dev-dependencies, so CI would still need the
native toolchain and the libjpeg fetch, and the dependency would have moved
rather than gone. Its value is also narrower than it looks — libultrahdr reads
only the **legacy** dialect, so it never was an ISO oracle, and the manual
Apple/Android gate answers the same question with real consumer decoders.
Replace it with:

- **captured goldens**: a small checked-in reference file plus the reconstruction
  values libultrahdr produces today, recorded once while the dependency is still
  present, so independence is banked at the moment of capture;
- **exiftool structural validation**, already in use and already proving MPF/XMP/
  ICC correctness; and
- **the documented external-decoder gate** for real HDR selection, re-run by hand
  when the writer changes. `iso_sample_for_external_decoder` already emits the
  file it needs.

The migration must not change gain-map math, renderer behavior, CLI/recipe
semantics, metadata claims, or output preset defaults. It *will* change the
shipped `ultra-hdr-v1` bytes, because our XMP serialization will not be
byte-identical to libultrahdr's. That preset is non-default and the gain map is
not covered by `version::PIPELINE_FINGERPRINTS`, so no `pipeline_version`
boundary is involved — but the determinism and golden assertions for that
preset must be re-captured deliberately, not adjusted until they pass.

### Recorded alternative, if upstream ever closes the gap

Keep this route documented rather than pursued. An exact crates.io
`ultrahdr-sys` becomes viable only when a release both contains `11ac0c3` (or a
verified equivalent) **and** obtains libjpeg-turbo without a mutable-tag fetch
and without a system library — bundled in the `.crate`, fetched at a pinned SHA,
or supplied by a Rust `-sys` crate that vendors source (`mozjpeg-sys` is
precedent for the pattern, though mozjpeg is a fork, not a drop-in). Watching
crates.io for a version bump is **not** a sufficient trigger; condition two is an
upstream design choice that may never change. Our own delta is small enough to
upstream (two `DOWNLOAD_COMMAND ""` lines plus a bundled tree) if someone wants
to try, but merge and release cadence would not be ours.

Do not adopt an unpinned branch, a tag-only Git dependency, a runtime system
library, or a build-time source download.

## How to Verify

- `cargo build` and `cargo test` both succeed with **no** CMake, clang, nasm, or
  libjpeg installed, and with the network disabled (`cargo build --offline`).
- CI drops its native prerequisites; the Linux and macOS jobs still pass all four
  gates.
- The final executable has no runtime dependency on libultrahdr or libjpeg on
  Linux and macOS.
- Byte-for-byte, the assembled file still satisfies every existing structural
  check: marker order with JFIF first, `hdrgm` and GContainer XMP, Display P3 ICC,
  MPF index whose offsets resolve and whose lengths sum to the file size, odd
  dimensions, and both ISO segments in the right images.
- exiftool independently resolves the MPF index and extracts the second image.
- The captured-golden reconstruction values still match, and the goldens were
  recorded from libultrahdr **before** it was removed.
- `vendor/ultrahdr-sys`, the snapshot manifest, the force-tracking guard, and
  `scripts/check-vendored-native.py` are all deleted together.
- `README.md`, `THIRD_PARTY_NOTICES.md`, and the distribution-license bundle no
  longer reference removed paths, and no longer carry libultrahdr/libjpeg-turbo
  notices that no longer apply.
- Full CI gate: `cargo fmt --all --check`,
  `cargo clippy --all-targets -- -D warnings`, `cargo build`, `cargo test`.

## Dependencies

- [Ultra HDR v1 gain-map JPEG output](gain-map-hdr-output.md)
- [Final ISO gain-map metadata](iso-gain-map-metadata.md) — re-implementing
  assembly must reproduce **both** dialects, so it needs that task's C.4.3/C.4.6
  placement rules settled first; doing it earlier would mean writing the ISO
  container work twice.
