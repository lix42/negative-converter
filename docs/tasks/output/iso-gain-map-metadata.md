# Final ISO Gain-Map Metadata

## Goal

Extend NC's Ultra HDR v1 gain-map JPEG with final ISO 21496-1:2025 metadata
using the same SDR base, gain-map image, and canonical metadata model. Do not
claim ISO conformance until an authoritative final-standard checklist or
independent oracle verifies the serialized bytes and reconstruction behavior.

## Design

Keep `output/gain-map-hdr-output`'s public Ultra HDR v1 JPEG fully valid and add
the final ISO metadata dialect without generating a second gain map. Serialize
both dialects from the one canonical scale/offset/gamma/capacity model, with
explicit linear-versus-logarithmic unit conversions.

**The pinning source is the licensed ISO 21496-1:2025 text, and it is in hand**
(2026-08-04). Annex C.2 (normative) gives the binary structure, C.4 the JPEG
storage. Reading it settled the field table and found a real conformance defect
in the reference implementation — see the progress log. An implementation could
not have served as the pin: it cannot establish which fields are mandatory, what
ranges are legal, or whether dual-dialect coexistence is permitted. Apple
ImageIO remains valuable as the independent decoder *oracle* for the
verification step, used after the text rather than instead of it.

Pinned from the text (C.2.2, C.2.3, C.3, C.4):

- identifier, version fields, structure, byte order, and JPEG placement —
  implemented in `pipeline/gain_map/iso.rs`; and
- offset, gamma, headroom, and per-channel semantics, including that
  `is_multichannel` describes the **metadata** channel count and may differ from
  the gain map's own (C.2.3).

**The text does not settle the other two, and the task previously assumed it
would:**

- **The ISO ↔ Ultra HDR v1 mapping is not an ISO matter.** ISO 21496-1 is silent
  on Google's XMP dialect; it says nothing about coexistence, and C.3 only asks a
  host format to define how a file is *identified* as conforming. So a legal
  mapping cannot be derived from this standard — it is ours to define and defend.
- **Dual-aware precedence is a decoder-behaviour question, not a conformance
  property.** The spike's "a decoder that understands both must prefer ISO
  metadata" traces to Android guidance, not normative text. It must be
  established empirically against real decoders and must not be stated as an ISO
  requirement anywhere in the code, docs, or report.

The ISO extension must remain separate from rendering, gain-map quantization,
and the Ultra HDR container implementation so a conformance correction changes
only the ISO serializer and its tests.

**Container status (2026-08-04): implemented.** An earlier draft of this section
predicted an MPF rewrite would be the hard part; that prediction was wrong and is
corrected here. libultrahdr rewrites the baseline image's segments and drops
unknown APP2s, but appends the gain-map image **verbatim** — so the gain map's
segment goes in at encode time and only the baseline's must be spliced in after
packaging. Placement then does the work: MPF offsets are relative to the byte
after `MPF\0`, so inserting *before* that segment moves the reference point and
the gain map together, leaving every stored offset valid and needing only the
first image's recorded size patched. See
`io::ultra_hdr::insert_baseline_iso_segment`. Verified independently with
exiftool, which resolved and extracted the second MPF image.

The later `output/presets` task may make the neutral `gain-map-hdr` dual-dialect
output the default only after this task is complete. This task adds no CLI
surface: `ultra-hdr-v1` is contractually ISO-free, and preset naming belongs to
`output/presets`.

Remaining container questions:

- ~~**The segment goes in *both* images.**~~ Done — C.4.3's version-only payload
  in the baseline, C.4.6's full structure in the gain map.
- ~~**Resampling phase.**~~ Decided: **stay centre-aligned.** 6.2.2 NOTE 1 prefers
  co-sited (H.265 ChromaLoc type 2), but the NOTE is informative and switching
  would change already-shipped `ultra-hdr-v1` bytes for no measured gain. Recorded
  on `gain_map::resample_axis`; both dialects share it, so they cannot diverge.
- **Blocked — C.4.3's CIPA DC-007 baseline requirement.** C.4.3 requires a
  DC-007-compliant baseline and its NOTE explains that means Exif-compliant; nc
  writes JFIF and no Exif. ISO 21496-1 alone does settle that our *colour space*
  signalling is unambiguous (C.4.4 branch two: no Exif + ICC present ⇒ the ICC
  governs), but not DC-007's own requirements. **DC-007 and DC-008 are free** from
  CIPA (`cipa.jp/e/std/std-sec.html`) behind a JavaScript/POST disclaimer gate —
  easy in a browser, resistant to scripting. Do **not** synthesise an Exif block
  before reading it, and never with `ColorSpace = 1`, which would force an sRGB
  reading of the Display P3 base; `baseline_carries_no_exif_colorspace_claim`
  guards that.
- **External by nature — the decoder oracle.** An ISO-aware decoder reconstructing
  the HDR rendition, plus observing which dialect a dual-aware decoder prefers.
  `io::ultra_hdr::tests::iso_sample_for_external_decoder` (`#[ignore]`, honours
  `NC_ISO_SAMPLE_DIR`) writes the file that gate needs, since there is no CLI path.
  The in-repo `conflicting_dialect_fixture_really_disagrees` proves the fixture
  genuinely conflicts; it must never be extended to assert ISO precedence as a
  conformance property, because the standard is silent on coexistence.

## How to Verify

- An independent ISO 21496-1 implementation and an independent Ultra HDR v1
  implementation reconstruct the canonical HDR rendition from the same file
  within pinned codec-aware bounds.
- Metadata inspection proves both dialects express the same
  scale/offset/gamma/capacity semantics after unit conversion.
- A deliberately conflicting fixture proves a dual-aware decoder selects the
  ISO metadata.
- The final identifier and every mandatory field match the approved
  final-standard checklist; no ISO/TS draft identifier remains.
- The unchanged primary JPEG opens as SDR in an ordinary JPEG reader.
- Android 15+ and target Apple software recognize the dual-dialect file as HDR.

## Dependencies

- [Ultra HDR v1 gain-map JPEG output](gain-map-hdr-output.md)
