# HDR AVIF Windows Packaging

## Goal

Prove and gate the static libaom build behind `hdr-pq` / `hdr-hlg` on Windows, so
the AVIF output path's platform claim covers the third supported target rather
than stopping at macOS and Linux.

## Design

`output/hdr-avif-output` shipped the AVIF encoder gated on macOS and Linux only,
because CI's matrix is `[ubuntu-latest, macos-15]` and claiming an untested
platform would have been false. This task adds the missing coverage rather than
changing any encoding behaviour.

Add a `windows-latest` job to `.github/workflows/ci.yml` and make the native build
work under MSVC: libaom is built from the published `libaom-sys` crate's vendored
source via CMake, so the job needs a C/C++ toolchain, CMake, NASM for x86_64 SIMD,
and `libclang` for bindgen. Confirm the static link produces a self-contained
`nc.exe` with no libaom DLL dependency.

Nothing about the container, the codestream, or the pinned encoder settings should
change. Byte-identity is scoped per build/architecture (design-spec §8), so the
Windows binary is **not** expected to reproduce the macOS/Linux bytes; what must
hold is the weaker documented cross-build contract — identical semantic metadata
and decoded pixels within the codec bounds `io::avif` already pins.

If MSVC cannot build libaom without patching vendored source, prefer documenting
Windows as unsupported over carrying a local patch: the repo already has one
regretted native snapshot (`output/ultrahdr-dependency-externalization`).

## How to Verify

- A `windows-latest` CI job runs the same four gates as the other targets, plus
  `scripts/check-vendored-native.py`, and is green.
- `hdr-pq` and `hdr-hlg` produce files on Windows that an independent decoder reads
  with the same dimensions, depth, 4:4:4 sampling, full range, CICP, brands and
  content-light metadata as the macOS/Linux outputs.
- Decoded pixels agree with the other targets within the codec bounds pinned by
  `io::avif`'s `decoded_code_error_stays_within_the_pinned_codec_bounds`; exact byte
  identity is explicitly *not* required across builds.
- The resulting binary links libaom statically (no external codec DLL).
- `README.md`'s build prerequisites list the Windows toolchain, and
  `docs/tasks/output/hdr-avif-output.md`'s deferred-Windows note is retired.

## Dependencies

- [HDR AVIF output](hdr-avif-output.md)
