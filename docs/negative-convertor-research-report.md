# Research Report for a Rust-Based AI-Friendly Film Negative Converter

## What the app should optimize for

The most important design decision is to separate **capture fidelity** from **positive rendering**. Your scanner output is not just an image to invert; it is an archival record of the film’s transmitted light. SilverFast’s HDRi RAW workflow is explicitly designed around non-destructive archiving, preserving the scan data plus color-management information and, when available, infrared data for dust and scratch handling. FADGI makes a similar distinction for negatives: it recommends saving the scan as an archival master, then creating a separate production master by inversion and rendering. FADGI also states that there is **no reliable way to reconstruct exactly how a negative would have looked when historically printed**, because the negative, print stock, and process choices vary and are often unknowable. citeturn9view2turn28search4turn19view1turn16view0

That means your app should not be built as “a magic true-color negative converter.” It should be built as a **deterministic rendering engine with well-exposed parameters**, plus optional AI assistants. The deterministic core should own the physics and color science: film-base estimation, density conversion, tone mapping, paper simulation, white-balance correction, and output color management. AI should help with tasks that are ambiguous or labor-intensive, such as border detection, neutral-region suggestions, crop detection, dust masks, and per-roll parameter estimation. This is a better fit for the problem than making a generative model responsible for the entire inversion, because restoration practice remains subjective and reference truth is often absent. citeturn19view1turn41view0

A practical north star for the product is this short stack of principles:

- **Keep the original scan untouched** and store edits as a recipe or sidecar-like record. FADGI recommends documenting special processing, and projects such as NegPy explicitly use non-destructive recipes or databases for this reason. citeturn17view0turn30view0
- **Support scanner RAW-like TIFF/HDRi first**, because that matches your current workflow better than camera RAW. SilverFast HDR/HDRi is the immediate real-world input to support. citeturn9view2turn28search6
- **Use a physically meaningful inversion core** based on film density and characteristic curves, not only channel inversion. Kodak sensitometry, Cineon-style workflows, darktable’s `negadoctor`, RawTherapee’s exponent model, and NegPy’s sigmoid model all point in that direction. citeturn35view1turn8view1turn22view0turn43view0turn30view0
- **Make AI opt-in and assistive**, especially for dust removal, auto-crop, degraded-film correction, and roll-level normalization. Current AI work around film is more mature for restoration tasks than for authoritative negative inversion. citeturn41view0turn40view1

## RAW format support

For your use case, the first-class input is not generic camera RAW. It is **high-bit-depth scanner output**, especially SilverFast HDR/HDRi. SilverFast describes HDRi RAW as a non-destructive archive format containing the captured image data, color-management information, optional infrared data for dust/scratch removal, and negative-related metadata such as NegaFix profiles where applicable. SilverFast also documents 48-bit HDR as scanner-captured raw image information and 64-bit HDRi as that data extended with infrared-channel capture. citeturn9view2turn28search6turn28search14

On disk, the most important containers to support are **TIFF-family files** first and **DNG-family files** second. TIFF is still the dominant archival wrapper for scanned raster images, and the Library of Congress notes that it is widely used for primary or master files in digitization workflows. TIFF is extensible and can wrap different encodings and compression methods; it also has a 4 GB limitation in standard TIFF that BigTIFF extends with 64-bit offsets. DNG is explicitly a TIFF extension, and Adobe describes it as a publicly documented archival raw format. In March 2026, DNG was also published as **ISO 12234-4:2026**, which strengthens its long-term interoperability story. citeturn37view0turn25view3turn9view0turn9view1turn37view1

For an AI-friendly app, DNG has one especially interesting property: newer DNG versions add metadata structures that can help carry richer editing state. The Library of Congress notes that DNG 1.6 added **semantic masks** and profile gain-table tags. If you ever want your app to export raw-ish positives or intermediate files with AI-generated masks, DNG is more promising than plain TIFF for that specific use case. Adobe also provides a DNG SDK for reading and writing DNG and converting DNG data into forms suitable for display or processing. citeturn9view1turn9view0

If you later add a camera-scanning workflow, generic camera RAW support matters much more. The most conservative choice there is still **LibRaw**, which describes itself as supporting virtually all RAW formats and maintains a large supported-camera list. RawSpeed is a separate fast raw-decoding library used in darktable and oriented toward camera RAW decoding. In pure Rust, the most relevant crates I found are **`rawler`** and **`rawloader`**. `rawler` exposes raw pixels and metadata for supported cameras, while `rawloader` supports many formats but is candid that it only implements the “good parts” of DNG rather than the full, increasingly complex spec. That makes pure Rust plausible for import, but less mature than LibRaw for maximum coverage and camera churn. citeturn25view0turn25view1turn25view2turn25view5turn26view3

For this project, the best format strategy is:

- **Day-one input:** TIFF, 16-bit TIFF, SilverFast HDR/HDRi-style TIFF-family files, and associated metadata. citeturn9view2turn37view0turn25view3
- **Near-term input:** DNG and common camera RAWs via LibRaw FFI or `rawler`. citeturn9view0turn25view0turn25view5
- **Day-one output:** 16-bit TIFF plus smaller JPEGs. FADGI’s negative tables use TIFF or JPEG2000 for masters and emphasize 16-bit at higher quality levels. citeturn34view0turn16view0
- **Later output:** DNG only if you decide that raw-like interchange, semantic masks, or richer metadata justify the added complexity. citeturn9view0turn9view1turn37view1

## Algorithms for converting negatives to positives

The key domain fact is that film negatives are not simple RGB inversions. Kodak’s sensitometry materials define density as the base-10 logarithm of opacity, and they describe the characteristic **H&D curve** as a non-linear density-vs-log-exposure curve. Kodak also notes that in color negative film the orange mask and layer spacing are part of the system design; the dye curves are not meant to be interpreted as straight “invert the channels and you are done.” FADGI likewise warns that scanning a color negative, inverting it, and tweaking the positive image does **not** reproduce exactly how the original would have looked on photographic paper. citeturn35view1turn35view0turn5view0turn19view0turn19view1

The algorithm families worth knowing are these.

A **simple inversion baseline** is just channel inversion plus some white-balance or border-neutralization logic. This is useful as a debugging reference and for black-and-white edge cases, but it is not a strong endpoint for color negatives because it ignores density-space behavior, orange-mask compensation, and film-specific transfer curves. FADGI’s recommendation to use “scene balancing” programs and film-oriented correction tools amounts to an official statement that better-than-naive inversion is needed in practice. citeturn19view1

A **density-domain inversion** is the most credible baseline for your app. darktable’s `negadoctor` describes itself as a film-negative inverter based on Kodak Cineon densitometry ideas and paper-print simulation. Its source code converts the scan from transmission to density relative to `Dmin`, applies corrections in log space, maps back through an exponential, and then applies print-like controls such as exposure, black, gamma, and highlight compression. darktable’s manual exposes the same idea in UI terms: film-base color, `D min`, `D max`, scan exposure bias, highlight white balance, shadow cast correction, paper black, paper grade, paper gloss, and print exposure. Kodak’s Cineon and Digital LAD materials are historically consistent with this density-oriented way of thinking about film scans and print aims. citeturn22view0turn44view0turn8view1turn7view1

A **power-law or exponent model** is the approach documented by RawTherapee. RawTherapee says that, for negatives, each channel value is proportional to a power of the reciprocal of the corresponding channel in the original exposure, and it lets the user tune a reference exponent plus red and blue ratios. This is elegant and surprisingly compact, but RawTherapee also makes an important limitation explicit: the tool supports only raw photos from Bayer or X-Trans sensors and does **not** support non-raw formats. That makes it a good source of algorithm ideas for camera-scanned negatives, but not a drop-in model for SilverFast TIFF/HDRi scanner files. citeturn43view0

A **sigmoid or explicit H&D-curve model** is the most promising next step after density-domain inversion. NegPy’s design is notable because it explicitly models film and paper behavior rather than treating inversion as simple channel reversal. Its README says it neutralizes orange mask using channel sensitometry and models the photographic H&D curve with a logistic sigmoid. That lines up well with Kodak’s sensitometric framing and gives you parameters that make photographic sense. For a build-you-own application, this is a strong inspiration because it can produce a UI whose controls map to film behavior rather than to arbitrary math knobs. citeturn30view0turn35view1

A good MVP algorithm for your app is therefore a **hybrid deterministic model**:

```text
decode scan to linear scanner RGB
estimate Dmin from unexposed film border
convert transmission to density
apply per-channel density correction and roll-level normalization
map density through a film/paper curve
render to positive with print-like controls
apply output color transform
```

A useful implementation sketch, distilled from Kodak sensitometry, Cineon-style density handling, darktable’s `negadoctor`, and film-oriented projects such as NegPy, is to work with equations of this shape: estimate base transmission from unexposed film, compute density as a logarithmic ratio to that base, correct the density in channel-aware space, then render through a print-like tone curve with black, exposure, gamma, and highlight compression. The precise parameterization is up to you, but the important architectural point is that **density conversion and print rendering should be separate stages**. citeturn35view1turn22view0turn44view0turn30view0

For AI, the best use is not “replace the inversion algorithm.” It is **parameter estimation and restoration assistance**: find the film border, suggest `Dmin`, detect clipped regions, estimate per-roll coefficients, propose neutral patches, detect dust, and create local masks for degraded negatives. The restoration literature and current open-source projects are concentrated there. A 2022 Scientific Reports paper on historical color-film restoration emphasized that conventional practice is laborious and subjective, while film-focused open-source work such as Spotless-Film concentrates on dust detection and inpainting rather than on replacing the inversion core. citeturn41view0turn40view1

## What “conversion quality” should mean

The hardest truth in this space is that **quality has two meanings**. First, there is the quality of the **capture**: bit depth, resolution, dynamic range, uniformity, misregistration, sharpening, noise, and color encoding accuracy. Second, there is the quality of the **rendering**: whether the positive image is convincing, consistent, and pleasing. FADGI is especially useful here because it says negatives are not straightforwardly scene-referred, the original basis for colorimetric judgment is ambiguous, and exact historic appearance is often unrecoverable. That means you should not judge your app only by “does this look nice.” You should judge it by whether it preserves information cleanly and renders that information consistently. citeturn19view0turn19view1

For capture fidelity, FADGI gives concrete targets for **photographic negatives from 35mm up to 4×5**. Its table for that class shows that higher-quality workflows move to **16-bit** masters and roughly **2940 ppi** at 3-star and **3960 ppi** at 4-star, with tone-response, white-balance, lightness-uniformity, and channel-misregistration thresholds also tightening. Dynamic range targets are listed in the same tables, but FADGI explicitly says dynamic range is **informative rather than critical** for compliance. These numbers are not universal aesthetic truths, but they are a strong benchmark for deciding whether your scanner input is good enough to support a high-quality inversion pipeline. citeturn34view0turn19view0turn19view1

For color and tone evaluation, FADGI explicitly frames color accuracy in terms of **ΔE2000** between target values and measured results in a working color space. The Sharma paper is the standard implementation reference for CIEDE2000, and the Wang–Bovik SSIM paper remains useful when you do have paired or reference-adjacent comparisons and want a structural metric rather than only a pixelwise or color-difference metric. In practice, ΔE2000 is best for target-based calibration and profiling, while SSIM is better for regression tests against a selected reference rendering. citeturn19view2turn15search0turn15search1

A good QA setup for this app would combine **objective scan tests** and **render regression tests**. FADGI’s OpenDICE and AutoSFR tools are meant for use with its guidelines, and the OpenDICE resource page even includes sample target/reference data for **Negative 35mm**, **Negative 120×120**, and **Negative 4×5**. That gives you a ready-made way to build a technical test suite around real negative formats instead of relying only on subjective eyeballing. citeturn42view0

A practical quality rubric for the converter should ask:

- Does the archival scan avoid highlight and shadow clipping before inversion? FADGI recommends scanning negatives as transparencies and keeping both ends of the range intact. citeturn19view1
- Does the inversion preserve neutral regions and roll-to-roll consistency? RawTherapee notes that neutral-spot picking can be done once per roll and copied across the roll, which is a strong hint that roll-level consistency is a real requirement. citeturn43view0
- Does the rendering remain stable under calibration targets and standard test negatives? FADGI and OpenDICE exist precisely to answer that question. citeturn42view0turn16view0
- Does the positive render hold highlight detail, shadow detail, and believable color without over-sharpening or channel artifacts? That is the print/render side of quality, and darktable’s paper-black, gamma, gloss, and exposure controls are all responses to that need. citeturn44view0

## Libraries and implementation choices

If you want to stay Rust-first, the most realistic strategy is a **Rust core with selective FFI where maturity matters**. For basic image containers and pixel processing, the Rust ecosystem is in good shape: `image` provides general image decoding/encoding and common image operations, `tiff` handles TIFF decoding/encoding, and `palette` gives strongly typed color-space conversions and emphasizes the difference between gamma-encoded and linear RGB, which matters a lot for film work. citeturn29view0turn26view2turn26view1

For metadata, you have usable Rust options. `kamadak-exif` is a pure-Rust EXIF parser for TIFF, JPEG, HEIF, PNG, and WebP containers, while `rexiv2` wraps the Exiv2/GExiv2 stack for broader EXIF/XMP/IPTC-style workflows. Exiv2 itself is a mature C++ metadata library that also handles ICC profiles embedded in images. If you want sidecars, recipes, and metadata-rich outputs, this part of the stack matters almost as much as the inversion math. citeturn27search6turn27search3turn27search0turn27search4

For camera RAW import, **pure Rust is possible but not yet the conservative choice for full coverage**. `rawler` and `rawloader` are the main Rust-native candidates I found. `rawler` exposes both raw image data and metadata for supported camera RAWs, while `rawloader` supports many camera formats and returns black/white points, Bayer layout, white balance multipliers, and XYZ conversion matrices. That said, LibRaw still looks like the safer fallback if broad camera support becomes important. LibRaw explicitly targets embedding in RAW converters, and its current changelog and support pages show ongoing work on modern DNG and camera support. citeturn25view5turn26view3turn25view0turn26view4

For color management, **Little CMS** is the obvious anchor. Its official site describes it as an ICC-based color-management engine focused on accuracy and performance, and the `rust-lcms2` bindings are described as stable and production-used. That makes Little CMS the right place to handle scanner input profiles, working-space conversion, output-space conversion, and soft-proof-like transforms. If you later need LUT-heavy or studio-style color pipelines, OpenColorIO is the standard C++ option, and Rust bindings such as `ocio-rs` are starting to appear, though they are much newer and should be treated more cautiously. citeturn14search0turn25view6turn25view7turn13search0

If you want a non-Rust fallback for broad image I/O, **OpenImageIO** is worth knowing. It is a mature format-agnostic image library used heavily in visual effects and film pipelines, with read/write support for many formats via plugins. I would not make it your day-one dependency if the goal is a Rust-native core, but it is a strong interoperability or tooling option if you later need support for formats such as DPX, Cineon, EXR, or studio-like conversion tools. citeturn36search1turn36search3turn36search10

The implementation choice I would make from this research is:

- **Core math and pipeline in Rust** using `f32` or `f64` in a linear working space. `palette` is especially useful because it is explicit about linear versus non-linear RGB. citeturn26view1
- **TIFF/HDRi import and export in Rust** via `tiff` plus custom parsing for scanner-specific metadata and channels. citeturn26view2turn9view2
- **ICC color management via Little CMS** through `rust-lcms2`. citeturn25view6turn14search0
- **Metadata via EXIF/XMP tooling** using `kamadak-exif` for pure Rust parsing and Exiv2/Rexiv2 where richer metadata writing is needed. citeturn27search6turn27search3turn27search0
- **Optional camera RAW backend** through LibRaw FFI first, with `rawler` as a pure-Rust path if you decide to invest in it later. citeturn25view0turn25view5

## Workflow pieces you probably should add

The most important missing piece is **scanner calibration**. SilverFast’s IT8 documentation says scanner IT8 calibration produces input ICC profiles for reflective and transparency positive originals and is **not applicable to negative originals**. In other words, you should calibrate the scanner on an appropriate positive target and then use that scanner profile in the negative workflow; you should not expect a direct “negative IT8 profile” to solve inversion. SilverFast’s newer IT8 materials also tie the workflow to ISO 12641 targets, and FADGI says targets with 100 or more patches are better inputs for ICC profile creation. citeturn38view1turn38view0turn31search8turn19view2

The second missing piece is **capturing or preserving unexposed film base information**. darktable’s `negadoctor` is very explicit that you should include some unexposed film border in the scan or photograph so the app can estimate `Dmin`; if the holder hides it, take a separate image of unexposed film and reuse that measurement across the roll. This is not a minor nicety. It is the anchor point for density conversion. citeturn44view0

The third missing piece is **infrared-aware dust removal**. SilverFast’s HDRi format can include an infrared channel, and SilverFast’s iSRD uses that channel for defect detection. This is a major opportunity for your app, because it gives you a more trustworthy defect mask than RGB-only heuristics. But SilverFast also documents important limits: infrared-based defect detection is poor or unavailable for traditional silver-based black-and-white films, and Kodachrome can be difficult because dense emulsion blocks infrared in dark areas. So the right architecture is to support both **IR-assisted dust masks** and **RGB/AI fallback dust removal**. citeturn24search0turn24search12turn24search6

The fourth missing piece is **roll-level workflow support**. RawTherapee’s documentation says neutral-spot picking only needs to be done once per film roll and then copied to the rest of the roll. NegPy similarly emphasizes batch normalization and non-destructive recipes. This strongly suggests that your app should be designed around **film-roll presets**, not image-by-image isolated editing. A good UI model would let the user define a roll profile containing film stock, `Dmin`, density curve parameters, neutral spots, and preferred print rendering, then override locally only when needed. citeturn43view0turn30view0

The fifth missing piece is **calibrated viewing and documented processing**. FADGI stresses that negative rendering should be done on a calibrated monitor in a proper viewing environment and that unusual processing should be documented in metadata or project records. That makes a recipe store, sidecar JSON, or XMP output more than a convenience feature; it is part of producing a reproducible workflow. citeturn19view1turn17view0

From all of that, the most sensible build order is:

- **Phase one:** import TIFF/HDRi, estimate `Dmin`, implement density-based inversion, add paper/render controls, export 16-bit TIFF and JPEG. citeturn9view2turn22view0turn44view0
- **Phase two:** add scanner-profile handling, roll presets, histogram/clip warnings, and OpenDICE-style QA around standard targets. citeturn38view1turn42view0
- **Phase three:** ingest infrared channels and add hybrid dust removal with IR-first and AI/RGB fallback. citeturn24search0turn40view1
- **Phase four:** add camera RAW import via LibRaw or `rawler`, then optional DNG-aware workflows if you want raw-style interchange or semantic masks. citeturn25view0turn25view5turn9view0turn9view1

## Open questions and limitations

The public sources I found are strong on **workflow concepts, image science, and library choices**, but weaker on the exact **on-disk schema** of SilverFast HDRi files. SilverFast officially documents what HDR/HDRi contains, including infrared and color-management information, but I did not find a public low-level specification of the TIFF tags, extra IFDs, or metadata layout that would let you implement HDRi parsing without testing against real files. For the build phase, you should expect to reverse-engineer or sample-test actual SilverFast outputs. citeturn9view2turn28search6

I also did not find a clearly dominant **Rust-native full DNG processing/writing stack** comparable in maturity to Adobe’s DNG SDK or LibRaw. The Rust story is much better for TIFF, generic image processing, and metadata than for full-fidelity DNG processing. If DNG import/export becomes central, FFI to Adobe’s SDK or LibRaw may still be the pragmatic choice. citeturn9view0turn25view0turn25view5turn26view3

Finally, the AI literature and tools I found were stronger for **restoration tasks** than for authoritative consumer negative inversion. The most convincing AI-related sources in this research were about degraded-color-film restoration, subjectivity reduction, and dust removal, not about replacing density-based inversion itself. That does not mean a learned negative converter is impossible; it means the highest-confidence path today is still a **physics-based core with AI assistance around it**. citeturn41view0turn40view1