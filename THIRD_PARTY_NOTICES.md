# Third-party notices

## libultrahdr

The `ultra-hdr-v1` output uses Google’s libultrahdr, pinned under
`vendor/ultrahdr-sys/libultrahdr`. Its licenses are reproduced in that directory.

This product includes Gain Map technology under license by Adobe.

The legacy HDR gain-map metadata implementation includes material covered by the
Adobe HDR Gain Map patent license. The required notice and full license terms are
reproduced at:

`vendor/ultrahdr-sys/libultrahdr/adobe-hdr-gain-map-license/NOTICE`

This preset writes legacy Ultra HDR v1 XMP/MPF metadata. It does not claim
ISO 21496-1 conformance.

## libultrahdr image_io

libultrahdr statically compiles its `third_party/image_io` support library into
nc's native dependency. image_io is licensed under Apache License 2.0. The exact
license text shipped with that source is:

`vendor/ultrahdr-sys/libultrahdr/third_party/image_io/LICENSE`

## modp_b64

The image_io library above compiles
`third_party/image_io/src/modp_b64/modp_b64.cc`. modp_b64 is Copyright (c)
2005, 2006 Nick Galbreath and is distributed under its BSD license. The exact
copyright, conditions, and disclaimer shipped with that source are:

`vendor/ultrahdr-sys/libultrahdr/third_party/image_io/src/modp_b64/LICENSE`

## libjpeg-turbo

The statically linked JPEG implementation is libjpeg-turbo 3.1.0, bundled
under `vendor/ultrahdr-sys/libultrahdr/third_party/turbojpeg`.

This software is based in part on the work of the Independent JPEG Group.

The libjpeg API code is distributed under the IJG license reproduced in the
vendored `README.ijg`. The TurboJPEG API and build-system portions are also
covered by the following Modified (3-clause) BSD License:

Copyright (C)2009-2024 D. R. Commander. All Rights Reserved.

Copyright (C)2015 Viktor Szathmáry. All Rights Reserved.

Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the following conditions are met:

- Redistributions of source code must retain the above copyright notice, this
  list of conditions and the following disclaimer.
- Redistributions in binary form must reproduce the above copyright notice,
  this list of conditions and the following disclaimer in the documentation
  and/or other materials provided with the distribution.
- Neither the name of the libjpeg-turbo Project nor the names of its
  contributors may be used to endorse or promote products derived from this
  software without specific prior written permission.

THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS",
AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE
IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE
DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDERS OR CONTRIBUTORS BE LIABLE
FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL
DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR
SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER
CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY,
OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE
OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.

## libaom (AV1) — the `hdr-pq` / `hdr-hlg` AVIF output

The `hdr-pq` and `hdr-hlg` outputs encode AV1 with libaom, reached through the
published `libaom-sys` crate. That crate vendors libaom's source and links it
statically, so **no libaom snapshot is checked into this repository** and the exact
version is pinned by `Cargo.lock` (`libaom-sys 0.17.2+libaom.3.11.0` at the time of
writing). The license and patent files travel with the crate source, under the
Cargo registry checkout:

- `<cargo-registry-src>/libaom-sys-<version>/vendor/LICENSE`
- `<cargo-registry-src>/libaom-sys-<version>/vendor/PATENTS`

libaom is Copyright (c) 2016, Alliance for Open Media, under the 2-clause BSD
license reproduced in that `LICENSE`. `libaom-sys` itself is BSD-2-Clause.

### AOM patent license review

libaom's `PATENTS` carries the **Alliance for Open Media Patent License 1.0**,
which grants each Licensor's Necessary Claims on a "no-charge, royalty-free,
irrevocable" basis to make, use, sell, offer for sale, import or distribute an
Implementation, subject to its § 1.2 conditions (notably the defensive-termination
and availability clauses). Distributing nc's AVIF output path therefore needs no
per-unit royalty to participating licensors.

This is a **factual summary of the shipped license text, not legal advice, and not
a completed legal review.** The HDR output spike deliberately re-homed the
"licensed normative text / legal review" gate to the encoder tasks rather than
treating it as satisfied (see `docs/hdr-output-spike.md`). What is discharged here
is the *standards* half — the AVIF v1.2 and AV1 specifications are public, so the
profile, brand and CICP conformance claims were verified against their normative
text. Counsel review of the AOM patent grant before a binary release remains
outstanding and is tracked with the release task.

nc does **not** depend on libavif; the AVIF container is written by `src/io/avif.rs`
and carries no third-party code.

## Binary distribution license bundle

A binary release must package this notice file together with the exact license
files it references, including:

- `vendor/ultrahdr-sys/libultrahdr/LICENSE`
- `vendor/ultrahdr-sys/libultrahdr/third_party/image_io/LICENSE`
- `vendor/ultrahdr-sys/libultrahdr/third_party/image_io/src/modp_b64/LICENSE`
- `vendor/ultrahdr-sys/libultrahdr/third_party/turbojpeg/LICENSE.md`
- `vendor/ultrahdr-sys/libultrahdr/third_party/turbojpeg/README.ijg`
- `vendor/ultrahdr-sys/libultrahdr/adobe-hdr-gain-map-license/NOTICE`

and, copied out of the `libaom-sys` crate source at the version `Cargo.lock` pins:

- libaom's `vendor/LICENSE`
- libaom's `vendor/PATENTS` (Alliance for Open Media Patent License 1.0)
