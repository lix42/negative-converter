# Remove the Ultra HDR Native Dependency

> **Task id unchanged** (`output/ultrahdr-dependency-externalization`) so its
> links, dependency entries, and append-only progress sections keep resolving.
> The *scope* changed on 2026-08-05: from "externalize the snapshot to a
> published crate" to "remove the native dependency from the tree entirely."
> See `docs/progress/output.md` for the investigation that forced the re-scope.

## Goal

Delete `vendor/ultrahdr-sys` and end nc's **Ultra HDR** native dependency. nc
writes the Ultra HDR v1 XMP and CIPA Multi-Picture Format container itself, in
Rust, so `cargo build` and `cargo test` need no CMake, nasm, libjpeg, or network
fetch.

**This does not make nc toolchain-free, and must not be read that way.**
`lcms2` / `lcms2-sys` stay in `Cargo.toml` for colour management, so a C FFI and
a working C compiler remain part of the build after this task — that constraint is
`core/release-readiness`'s supported-platforms problem, not this one's. Scope here
is exactly the libultrahdr + libjpeg-turbo snapshot and the CMake/nasm/network
prerequisites it alone drags in.

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
- **Write MPF ourselves — and budget for it.** The tree has a **reader, not an
  emitter**. `insert_baseline_iso_segment` (from the prerequisite task
  `output/iso-gain-map-metadata`) parses the MP Index IFD and patches the first
  image's recorded size, but nothing constructs an MPF segment: assembly is still
  libultrahdr's, and the repo's other MPF mentions are marker/string checks. This
  task must write the **emitter**, which the reader does not provide. Do not scope
  it as "the same structure, already parsed" — that underestimates the work, and
  removing the native library before the emitter validates offsets and lengths
  would ship a broken container. Once the emitter exists,
  `insert_baseline_iso_segment` is retired: with assembly under our control both
  ISO segments are placed directly instead of spliced in afterwards.
- **Keep the ISO serializers unchanged.** `pipeline::gain_map::iso` (also from
  `output/iso-gain-map-metadata`) owns C.2.2/C.4.6 and is independent of the
  container.

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

- **an in-repo reconstructor that decodes each newly generated file — not a static
  golden alone.** This is the coverage that must not be lost, and a checked-in
  reference file cannot supply it: today
  `tests/pipeline.rs::ultra_hdr_v1_native_reconstruction_covers_odd_dimensions_and_hdr_vectors`
  reconstructs the output nc *just produced*, so a wrong XMP value or a mis-linked
  gain map fails CI. Values captured from the old file only prove our reader
  reproduces them from *that* file; they say nothing about what the new writer
  emits. Nor can it be closed by byte-comparing against libultrahdr's output,
  since the migration deliberately changes the XMP serialization. So this task
  owes a Rust reader able to parse our own MPF/XMP/gain map back and assert the
  reconstruction vectors on freshly generated files. Capture the goldens from
  libultrahdr **before** removing it, but treat them as the *reference values*
  that reader is checked against, not as the check itself;
- **an exiftool-based structural check, which is new work — not something already
  in place.** A repo-wide search finds exiftool only in `scripts/analysis/*` and
  `scripts/real-scan-verify/harness.sh`, both TIFF/asset inspection; there is no
  Ultra HDR MPF/XMP/ICC validator in `tests/` or `.github/`, and today's Ultra HDR
  assertions are marker/string checks plus the native decoder. Budget installing
  exiftool in both CI jobs, invoking it on generated output, and asserting its
  parsed fields; and
- **the documented external-decoder gate** for real HDR selection, re-run by hand
  when the writer changes. `iso_sample_for_external_decoder` already emits the
  file it needs. This one is a manual gate and never counts as CI coverage.

**Recalibrate the memory preflight in the same change.** `pipeline/memory.rs`'s
`RunProfile::UltraHdrV1` spends a calibrated **20 B/px** `byte_staging` term
explicitly on libultrahdr's owned input copies, its native destination, and the
Rust copy taken before the encoder is released. Those allocations disappear or
change shape under Rust assembly, and this is a **user-facing gate**: over
`--max-memory` it hard-rejects with exit 6, so a stale term can refuse a
conversion whose real peak fits, while an under-estimate silently over-approves.
Re-measure and update the term and its comment — CLAUDE.md's standing warning is
that nothing tests this model against the code.

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
library, or an **unpinned** build-time source download. The pinning is the point,
not the bundling: a fetch of an immutable revision (a full SHA, or a
content-hash-verified archive) is acceptable and is exactly the "fetched at a
pinned SHA" case above. A fetch of a mutable ref — branch or tag, including
libultrahdr's current `GIT_TAG 3.1.0` — is not, because a retag silently changes
the JPEG encoder and therefore output bytes.

## How to Verify

- `cargo build` and `cargo test` both succeed with **no** CMake, nasm, or libjpeg
  installed, and with the network disabled (`cargo build --offline`). A C compiler
  is still required — `lcms2-sys` keeps one in the build; see the Goal.
- CI drops its native prerequisites; the Linux and macOS jobs still pass all four
  gates.
- The final executable has no runtime dependency on libultrahdr or libjpeg on
  Linux and macOS.
- Byte-for-byte, the assembled file still satisfies every existing structural
  check: marker order with JFIF first, `hdrgm` and GContainer XMP, Display P3 ICC,
  MPF index whose offsets resolve and whose lengths sum to the file size, odd
  dimensions, and both ISO segments in the right images.
- exiftool independently resolves the MPF index and extracts the second image —
  **from CI**, via the newly added validator, not from a hand-run command.
- The in-repo reconstructor decodes each **freshly generated** file and matches the
  reconstruction vectors, so the coverage
  `ultra_hdr_v1_native_reconstruction_covers_odd_dimensions_and_hdr_vectors`
  provides today is preserved rather than downgraded to a structural check. The
  reference values were captured from libultrahdr **before** it was removed.
- `pipeline/memory.rs`'s `RunProfile::UltraHdrV1` is re-measured against a real
  scan and its `byte_staging` term and comment updated; a conversion that fits the
  budget is not rejected, and the estimate still covers the new peak.
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
  placement rules settled first.
  **On `insert_baseline_iso_segment` being retired later:** that is deliberate, not
  waste, and the edge is not a scheduling mistake. The splice is a small, tested
  adapter that let the ISO dialect ship against libultrahdr's existing assembly
  instead of blocking it behind a full container rewrite; inverting the order would
  have held the ISO work hostage to this deferred, non-blocking task. What the ISO
  task contributes permanently is the part that survives — the C.2.2/C.4.6 field
  table, its serializers, and the placement rules — while only the insertion
  adapter is replaced once nc owns assembly. The ordering is also now settled in
  fact: the ISO task merged as #76 before this one started.
