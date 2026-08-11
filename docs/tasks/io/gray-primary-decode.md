# Decode a single-channel gray SilverFast scan

## Goal

Accept a SilverFast scan whose **primary image is 16-bit grayscale** rather than
3-channel RGB, carrying its IR page as before. Today nc refuses the file outright,
so B&W scans cannot enter the pipeline at all.

## Why this is its own task

`io/silverfast-decode` required `Gray(16)` only for the **IR plane** beside an RGB
`IFD0`; a gray *primary* was never in scope. And `algo/bw-support` — the mono
colour model — explicitly rules this out: *"16-bit RAW scan input is a separate
concern … do not pull input-format work into this task."* So neither existing task
owns it, and `bw-support` is blocked behind it.

Measured on real assets (`../nc-assets/rolls/ILFORT-HP5-2026-08-10/`, seven Ilford
HP5 frames, 2026-08-11):

```
unsupported: … expected 3-channel 16-bit RGB in the primary image,
             found Gray(16); only SilverFast HDR/HDRi 16-bit scans are supported
```

Their structure is otherwise familiar — `IFD0` gray 16-bit 5184x3600, `IFD1`
reduced-resolution, `IFD2` full-resolution **"Transparency mask"**
(`NewSubfileType=4`, the marker the IR path already requires). So the IR plane is
present and marker-verified; only the primary's channel count differs.

## Open questions

1. **What does a gray primary become internally?** `LinearImage` is interleaved
   RGB. Replicating the single channel into three is the cheap answer and keeps
   every downstream stage untouched, but it triples the buffer and invites the
   later mono colour model to un-replicate it. A one-channel variant avoids that
   and touches every stage. Decide with `algo/bw-support`, which is the consumer.
2. **Does the memory model change?** The current bytes-per-pixel figures assume
   3-channel input; whichever representation is chosen, `pipeline::memory` needs
   its own numbers rather than inheriting these.
3. **What identifies the file as a supported gray scan** rather than an arbitrary
   grayscale TIFF? The existing decoder leans on SilverFast XMP and the IFD
   layout; state which of those is load-bearing here.
4. **Reject or accept a gray primary with no IR page?** Both exist in the wild.

## How to Verify

- The HP5 frames decode, and `nc inspect` reports their dimensions, IR presence
  and scanner metadata — derived numbers only, never sample pixels in context.
- The IR page is recognised as marker-verified (`NewSubfileType=4`), the same as
  on an RGB HDRi scan.
- Existing RGB HDR/HDRi fixtures are byte-identical through decode — this task
  adds an input shape, it must not perturb the shipped one.
- A gray TIFF that is *not* a SilverFast scan is refused with a clear message.

## Dependencies

- [SilverFast HDR/HDRi decode](silverfast-decode.md)

Blocks [Black & white negative support](../algo/bw-support.md), which owns the
mono colour model once these files can be read, and gates the B&W half of
[IR usability by measurement](../film-base/ir-usability-detection.md).
