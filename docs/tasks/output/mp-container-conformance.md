# MP Container Conformance (CIPA DC-007)

## Goal

Make nc's gain-map JPEG a conformant **Baseline MP File** per CIPA
DC-007-Translation-2025: type the gain map as a gain map rather than
`Undefined`, and settle the Exif-baseline requirement. Both were found by reading
the licensed-but-free CIPA text on 2026-08-06 and are recorded in
[progress/output.md](../../progress/output.md) under
`## iso-gain-map-metadata (CIPA DC-007 read — verdict)`.

Neither item is required for the ISO 21496-1 metadata to *function* — Apple
ImageIO reconstructs HDR from nc's file today with the type code `Undefined` and
no Exif present. This is conformance-claim work. It is split out precisely
because it changes shipped container bytes and must not hold `output/presets`
behind a change unrelated to the metadata `output/iso-gain-map-metadata` owns.

## Design

### 1. MP Type Code for the gain map (small, well-specified)

DC-007 Table 4 assigns ISO 21496-1 gain maps **MP Type Code `050000`**, and marks
`000000` (Undefined) as **× — "shall not be used"** in a Baseline MP File
(`.JPG`). nc currently writes `000000` for the second image; the primary's
`030000` is already correct.

The wrong value comes from **libultrahdr**, not nc — `ultrahdr_app` v1.4.0's own
output has the same code, unsurprisingly, since the gain-map type postdates it.
The repair is a 4-byte field in the MPEntry array, patched post-packaging in the
same place `insert_baseline_iso_segment` already fixes the first image's recorded
size. Note the MPEntry layout is `attribute(4) · size(4) · offset(4) ·
dependencies(2+2)`, and the type code lives in the low 24 bits of the attribute
word — so this is a masked update, not a whole-word write.

**This changes the shipped `ultra-hdr-v1` bytes**, because MPF is written for both
dialects. That is the main reason it is a separate task: it needs its own
before/after review and a refreshed golden, not a quiet ride-along.

### 2. Exif baseline (larger, and genuinely open)

DC-007 §4.2.1 says a Baseline MP File uses the Exif compressed-image file format,
and §5.1 places the MP Extensions APP2 "immediately after the Exif Attributes in
the APP1 marker segment". nc writes `APP0 JFIF · APP1 XMP · APP2 ICC · APP2 ISO ·
APP2 MPF` — no Exif at all. ISO 21496-1 C.4.3's DC-007 requirement reaches us
through this.

How hard it binds is worth weighing rather than assuming: §7's *tag-level*
requirements are "**should** be followed" for non-thumbnail Individual Images,
and its tables are explicitly pinned to Exif 2.32 / DCF 2.0. The structural
statements in §4.2.1/§5.1 are the stronger ones. A defensible outcome of this
task is a **narrowed, sourced conformance claim** rather than an Exif block.

Three constraints on anyone who does add Exif:

- **`ColorSpace` must be `Uncalibrated`, never `1`.** A value of 1 forces an sRGB
  reading that would misidentify the Display P3 base.
  `baseline_carries_no_exif_colorspace_claim` is the tripwire and must be updated
  deliberately, not deleted.
- **Whether libultrahdr's `package()` preserves, rewrites, or drops an APP1 Exif
  is unknown and must be established by probe.** It rewrites the baseline's
  segments and drops unknown APP2s; the behaviour is asymmetric in a way that was
  already guessed backwards once. `ultrahdr_app` has an `-x` Exif-insertion flag,
  so the native API may be the correct route rather than post-patching.
- **Adding Exif changes the baseline's marker layout**, which is exactly what
  silently disabled the ISO metadata before
  (`baseline_iso_segment_precedes_the_frame_header`). Re-run the decoder oracle;
  the Rust suite alone provably cannot catch a placement regression.

### Source documents

CIPA DC-007-Translation-2025 (Multi-Picture Format) and DC-008-Translation-2026
(Exif) are **free** from `cipa.jp/e/std/std-sec.html`. The disclaimer gate is a
plain POST — `std/js/dll.js` copies the query string into a hidden `dlltarget`
field posted to `std/documents/dll.cgi`, so
`curl -X POST .../dll.cgi --data-urlencode dlltarget=CIPA_DC-007-2025_E` fetches
the PDF with no browser. **Do not commit either PDF**; restate and cite numbered
subclauses, as this repo already does for ISO 21496-1.

## How to Verify

- The gain map's MP Type Code reads `050000` in an independent reader (exiftool
  reports it as a named type), and the primary still reads `030000`.
- libultrahdr still decodes the package, and the ImageIO oracle still reports the
  ISO gain map PRESENT and reconstructs at the expected headroom — both dialects
  re-verified after any container change.
- The shipped `ultra-hdr-v1` byte change is deliberate, reviewed, and recorded
  against a refreshed baseline rather than discovered.
- If Exif is added: it round-trips through `package()`, carries
  `ColorSpace = Uncalibrated`, and the MP Extensions APP2 follows it.
- If Exif is *not* added: the residual non-conformance is stated explicitly, cited
  to DC-007 §4.2.1/§5.1, and the product claim is narrowed to match.

## Dependencies

- [Final ISO gain-map metadata](iso-gain-map-metadata.md)
