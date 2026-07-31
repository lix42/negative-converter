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

## Binary distribution license bundle

A binary release must package this notice file together with the exact license
files it references, including:

- `vendor/ultrahdr-sys/libultrahdr/LICENSE`
- `vendor/ultrahdr-sys/libultrahdr/third_party/image_io/LICENSE`
- `vendor/ultrahdr-sys/libultrahdr/third_party/image_io/src/modp_b64/LICENSE`
- `vendor/ultrahdr-sys/libultrahdr/third_party/turbojpeg/LICENSE.md`
- `vendor/ultrahdr-sys/libultrahdr/third_party/turbojpeg/README.ijg`
- `vendor/ultrahdr-sys/libultrahdr/adobe-hdr-gain-map-license/NOTICE`
