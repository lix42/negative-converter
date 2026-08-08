# Gain-Map Dialect Activation

## Goal

Close the last two items left over from
[`iso-gain-map-metadata`](iso-gain-map-metadata.md): verify the dual-dialect file
on **Android 15+**, and give the ISO dialect a **CLI path**, so a user can
actually produce one. Today `Dialects::LegacyPlusIso` is implemented, verified
against Apple ImageIO, and reachable only from an `#[ignore]` test.

That task shipped as done on the strength of the Apple half plus libultrahdr;
this one carries the remainder. It blocks nothing — `output/presets` is free to
proceed — but until it lands, nc's shipped gain-map output is
[legacy-only, and therefore plain SDR on Apple platforms](../../design-spec.md).

## Design

### 1. Android 15+ decoder verification

The one platform claim `iso-gain-map-metadata`'s **How to Verify** never tested.
Android 15 reads ISO 21496-1 gain maps, and unlike Apple it *also* reads Google's
legacy Ultra HDR v1 XMP — so it is the only place where dual-dialect coexistence
is observable end to end, and the only independent check on whether adding ISO
segments disturbed the legacy path on its home platform.

Generate the sample set exactly as `scripts/iso-decoder-oracle/README.md`
describes (a real scan at `NC_ISO_SAMPLE_EV=3.0` — at defaults the gain map is
inert and cannot discriminate anything), then on a physical Android 15+ device
or emulator confirm:

- `oracle-dual-dialect.jpg` displays as HDR, and `oracle-legacy-only.jpg` also
  does — the latter is the control proving the ISO segments did not break the
  legacy dialect.
- Which dialect wins on `oracle-conflicting.jpg`, whose two dialects disagree by
  exactly one stop. Apple selects ISO. **Record whatever Android does as observed
  behaviour, never as a conformance property** — ISO 21496-1 is silent on
  coexistence, and that guard is load-bearing in three documents already.

If the two platforms disagree on precedence, that is a finding worth a design
decision, not a bug to paper over: it would mean a dual-dialect file renders
differently by platform whenever the dialects diverge, which is an argument for
keeping them derived from one model (as they are today) rather than for picking
a winner.

Extend `scripts/iso-decoder-oracle/README.md` with the Android procedure. There
is no need for a second harness — the files are the same.

### 2. CLI activation

`LegacyPlusIso` carries a documented `#[allow(dead_code)]` naming `output/presets`
as its consumer. Removing that allowance is the mechanical definition of done.

**Boundary with `output/presets`, in the pattern
[`hdr-avif-output`](hdr-avif-output.md) established: whichever task ships the
CLI surface owns the name.** `output/presets` owns the neutral `gain-map-hdr`
preset and the default migration; if it lands first, it activates the dialect and
this task keeps only the Android half. If this task lands first, it must **not**
invent a competing preset name — the shipped `ultra-hdr-v1` is contractually
ISO-free (`tests/pipeline.rs` asserts its bytes contain no `21496`) and renaming
or re-pointing it would hand `output/presets` a migration instead of a
capability. Coordinate rather than racing.

Whatever the surface, activation must:

- keep `ultra-hdr-v1` byte-identical (sha256 `67911f22…5540` on the Ektar
  reference frame is the standing check);
- calibrate a `memory::RunProfile` for the new preset if it is a distinct one —
  the dual-dialect package is a few KB larger than the legacy one, so
  `UltraHdrV1`'s profile is very likely still correct, but confirm rather than
  assume, per `pipeline/memory.rs`;
- route through `validate_convert`, not bare `validate`.

## How to Verify

- On Android 15+: the dual-dialect file displays as HDR; the legacy-only control
  still displays as HDR; the conflicting file's selected dialect is recorded as
  observed behaviour with the platform and OS version noted.
- A user can produce a dual-dialect file from the CLI, and `--help` describes it.
- `LegacyPlusIso`'s `#[allow(dead_code)]` is gone.
- `ultra-hdr-v1` output is unchanged, checked by hash on a real frame.
- The Apple oracle still passes after activation (`PRESENT` plus a `GainMapMax`
  above 0 — **not** the headroom figure, which is nc's own declared constant
  echoed back and reads the same on a flat gain map).

## Dependencies

- [Final ISO gain-map metadata](iso-gain-map-metadata.md)
