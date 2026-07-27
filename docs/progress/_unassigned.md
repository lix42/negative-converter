# Negative Converter — Unassigned Progress Log

Log sections the epic migration could not attribute to any task — planning notes,
review triage, write-ups for tasks that no longer exist, and execution records
that were nested under another heading in the flat log (so they moved with their
parent section rather than with their own task). This is a parking lot, not a
category: when a section clearly belongs to an epic, move it into that epic's
file.


## External review triage — 7 findings → 7 tasks (2026-07-18, docs-only, uncommitted)

An external code review of the Step-1 codebase produced seven findings. Each was
**verified against the actual code** before acting (several claims were checked
with `tiffinfo`/`exiftool` on the real `../nc-assets` scans, `cargo build`, and
direct source reads); all seven held up. Per the user's direction the pass stayed
**docs-only** — every finding was turned into a tracked task rather than fixed in
place, since the working tree already held documentation edits and code changes
were to be scheduled, not mixed in. Result: `docs/TASKS.md` updated (Mermaid graph
+ dependency list + phase checklists) and seven new `docs/tasks/*.md` files. No
source, `Cargo.toml`, or `Cargo.lock` touched.

The tasks (all deps `[x]` ⇒ executable now, except where noted):

- `input-color-management` (Phase 6) — **input ICC → working space.** `InputColor::Auto`
  promises embedded/default-profile decoding but `Auto` ≡ `Linear` today (decode
  normalizes integers, every stage assumes linear Rec.709/D65; only `Profile` is
  rejected). Investigated with `exiftool`/`tiffinfo`: **all 26 real scans carry no
  embedded ICC profile and no colorimetry tags** (raw `Gamma=1` Plustek/SilverFast),
  while our own `converted/` outputs embed "sRGB built-in" — so this is a
  forward-looking fidelity feature (enabled once the user makes an IT8 scanner
  profile), not a fix for current output. One profile per scanner (device
  characterization), **not** per film roll; stock differences stay the density
  stage's job. Task uses lcms2 to build a source→working transform applied after
  decode; lifts the `--input-profile` rejection. Deliberately skipped the cheaper
  "honest default / fail-loud on embedded profile" option (pre-release).
- `density-safety-bounds` (Phase 6) — physical bounds on
  `density_scale`/`offset`/`gamma` (the sigmoid-bounds analogue density lacks;
  `validate` checks only finiteness/positivity) + a degenerate-output
  (histogram/dynamic-range collapse) **warning** catching the finite-all-black
  underflow the loss counters miss (`10^(γ·D')`: huge-negative density → finite
  `+0.0`, uncounted — acknowledged at `algo/density.rs:221-226`). Offset stays
  negative-capable (mask compensation) ⇒ magnitude cap. Warning needs a
  false-positive guard validated on real (legitimately dark) scans.
- `transactional-output-writes` (Phase 8) — artifacts written straight to final
  paths via `File::create`, sequentially; reproduced sidecar-fails-after-primary
  leaving an orphaned TIFF. Temp-write → fsync → rename. Framed as **honest
  "no partial artifacts + minimized window," not literal multi-file atomicity**
  (POSIX rename is per-file). Records the existing IR-before-primary mitigation.
- `memory-preflight` (Phase 8, Phase A of the memory review) — the 4 GiB decode
  limit guards only the u16 read buffer while derived peak is a multiple
  (u16+f32 decode, full-image clone in `to_output` incl. the never-transformed IR,
  quantize buffer; three full images can overlap — decoded `image` + algo
  `positive` + `to_output` clone ⇒ ~24 GiB, ~6× the 4 GiB input ceiling, and still
  ~16 GiB / two images after the in-place fix) unchecked. Adds a peak-memory preflight
  (one shared sizing model, operational `--max-memory`-style knob, fail-loud) and
  drops the `to_output` clone (transform in place, skip IR).
- `streaming-tiled-io` (Phase 8, Phase B, **evaluate-first**, gated on
  `memory-preflight` + `real-scan-verification`) — strip/tile decode + streaming
  encode. Opens with a **STEP 0 gate**: evaluate from measured peak whether it's
  needed at all; if data is insufficient, collect it first; proceed only if real
  scans exceed the budget. Default expectation: not needed yet (~18 MP ⇒ ~600 MB).
  Pushed back on committing to a full streaming architecture unmeasured.
- `dependency-hygiene` (Phase 8) — drop three unused crates (`image`,
  `kamadak-exif`, `palette`; **verified `cargo build --all-targets` succeeds
  without them** — `image` pulls a large codec tree) and unify the two `Algorithm`
  enums onto `types::Algorithm`, removing the dead `algo::mod::Algorithm` copy and
  its `#[allow(dead_code)]`. Pure cleanup. (Noted: `cargo` doesn't warn on unused
  *deps* by default, which is why CI missed them.)
- `release-readiness` (Phase 8) — (1) **doc-accuracy corrections** (do-first,
  independent): README still says "pre-implementation / coding hasn't started" +
  "Planned usage" (false); `TASKS.md` says "two algorithms" omitting `sigmoid`
  (three exist); obsolete `--out-depth f32` → `--output-hdr` in **two** task files
  (`real-scan-verification.md:32`, `pipeline-orchestration.md:49`); the research
  report's `citeturn…` tokens are **PUA-wrapped** (plain grep finds 0) and need
  delimiter-aware cleanup. (2) **productization**: license (**user decision** —
  none present), Cargo release metadata (all fields absent), supported platforms
  (lcms2-sys C-FFI cross-compile constraint), binary packaging (sequence after
  `real-scan-verification`).

**Deferred / not created:** the cheaper Option-1 honest-default for input color
(pre-release makes it moot — folded into `input-color-management` lifting the
rejection). **Open:** pick a first task — the doc-accuracy half of
`release-readiness` is the quickest, most user-visible win.


## color-characterization-calibration
**Status:** superseded
**Updated:** 2026-07-23

- 2026-07-23: Superseded by `optional-color-correction-profiles`. Measured
  neutralization is now an explicitly selected, non-blocking correction feature;
  it is not part of the default film-preserving pipeline and no display task
  depends on it.

- 2026-07-21: Added the offline calibration half split from the runtime task. It
  fits matrix/curves against controlled target data, validates held-out Delta E,
  justifies model complexity, and emits a reproducible versioned artifact with
  scanner/film/development provenance.
- 2026-07-21: Added explicit target reference coordinates/illuminant and declared
  adaptation into ACEScg D60. Calibration normalization may not bake creative WB;
  artifacts also carry the exact reconstruction-domain compatibility contract.
- 2026-07-21: Calibration inputs now follow per-algorithm canonical domains:
  density artifacts fit the Dmax-neutral positive and reuse across scalar Dmax
  placement; sigmoid v1 fits one exact fixed Dmax; simple fits its pinned affine
  inversion settings.
- 2026-07-21: Superseded the prior simple affine wording. Simple calibration fits
  raw unclamped `1 - scan/Dmin`; inversion WB and black/white placement are
  excluded from calibration and artifact compatibility.


## post-characterization-render-pipeline
**Status:** superseded
**Updated:** 2026-07-23

- 2026-07-23: Superseded by `film-master-render-pipeline`. The replacement
  consumes typed NC film RGB v1 mapped ACEScg, renames `scene-master` to
  `film-master`, and explicitly preserves intentional film rendering rather than
  claiming physical scene recovery.

- 2026-07-21: Split pipeline/routing work from characterization runtime. This task
  moves WB/exposure/black/highlight controls after characterization, provides the
  common SDR/HDR source API, and defines a true scene master. The master rejects
  frame-local auto Dmax, accepts supported `none` or fixed/roll Dmax, and preserves
  exposure; current `--output-hdr` remains a rendered transitional float TIFF.
- 2026-07-21: Made the master bypass fail-loud: any non-default downstream render
  control remaining after CLI/recipe merge is a usage error, never ignored.
  Added flags-win reset, conflict, and resolved-report provenance requirements.
- 2026-07-21: Inserted algorithm-specific placement before the output split.
  Density Dmax is now a scalar gain after characterization; sigmoid/simple arrive
  already placed under their artifact contracts. Scene-master includes placement
  but still bypasses every later print/display control.
- 2026-07-21: Moved ownership of density artifact evaluation and Dmax placement
  wholly into the characterization runtime. This task now accepts only ordinary
  placed `f32` ACEScg, cannot observe the private extended-range intermediate,
  and records the fixed/none placement already applied to a scene master.
- 2026-07-21: Moved shipped simple inversion-WB and clip-low/high remapping after
  characterization. Target presets use `print.white_balance` plus new
  `print.linear_range`; old simple controls are warned conflicting aliases, while
  legacy no-preset TIFF retains current ordering during migration. Scene master
  rejects any non-default resolved adjustment.


## color-management planning — main reconciliation
**Status:** documentation reconciled
**Updated:** 2026-07-21

- Rebased onto `origin/main` after `roll-conversion` (`3b93ae5`) and
  `dmax-reference` (`06b75fb`) merged. Preserved both append-only implementation
  histories and marked both tasks complete in the canonical index.
- Replaced the stale output-preset reconciliation note with the shipped `nc roll`
  contract: `<stem>_positive.tiff` automatic names today, explicit manifest
  outputs and per-frame partial recipes, path-derived per-image sidecars, one
  stdout/`--report-file` roll report, and pre-write collision checks. The future
  preset task extends those guarantees only where container-specific suffixes and
  per-frame preset selection require it.
- Reconciled shipped Dmax behavior with the planned characterized runtime. Today
  `density.dmax` defaults to roll-fixed `fixed`, `--fixed-d-max` resets a recipe,
  `nc estimate --d-max-region` emits the reusable explicit scalar, roll mode warns
  on auto/per-frame Dmax, and the default-render change remains the deferred
  `pipeline_version 1` boundary. Future characterization keeps the scalar's
  roll-fixed acquisition but treats density Dmax as post-artifact exposure
  placement rather than promising display white; sigmoid still scopes the exact
  numeric Dmax.
- Review correction: `sdr-display-rendering` returns rendered-linear destination
  pixels and resolved metadata, never transfer-encoded pixels. Display P3 or the
  corresponding destination-output stage applies transfer encoding afterward;
  gain-map construction consumes the pre-transfer rendition for common-linear
  ratio derivation. This removes the prior double-encoding ambiguity.

### Real-scan core verification — executed 2026-07-22 (task: real-scan-verification)

Full matrix run against the user's five real rolls (Ektar, phoenix, Portra160,
Portra400, Portra400-leica-flaw) on the compiled release binary. Derived numbers
only — no sample pixels read into context. Full write-up + numbers:
[`docs/reports/real-scan-verification.md`](../reports/real-scan-verification.md);
rerunnable harness + frozen recipes under `scripts/real-scan-verify/` (see its `README.md`).

- **All assets are HDRi** (`ir_present: true`; IR carried in the transparency-mask
  IFD), standard frame 5184×3599 ≈ 18.66 MP. Scanner Plustek 8300i / SilverFast 9.2.9.
- Per-roll `Dmin` (unexposed frame) + `Dmax` (fully-exposed leader) measured from a
  holder-free center-40% region, frozen to recipes. 4/5 rolls clean.
- Matrix: inspect ✅ · estimate ✅ (`--auto-base` fails loudly on every frame per the
  holder layout — correct) · convert 16-bit+float ✅ (float byte-lossless; u16 clips
  4.8–10.3% high) · IR export + `--strict` ✅ · determinism byte-identical ✅ ·
  resource ✅.
- **Resource / streaming STEP 0 input:** measured peak **~930 MiB @ 18.66 MP
  (~50 MiB/MP)**, ~1.6 s wall — ~1.5× the design's ~600 MB model (model omits the
  carried IR plane + `to_output` clone). Target = 8 GB M3 MacBook Air (2024),
  ~4–5 GB usable. Assumed 4× worst case ⇒ ~3.7 GiB (4× MP) to ~15 GiB (4× per side)
  ⇒ **`memory-preflight` gate required; streaming a conditional GO** pending
  post-preflight re-measure and the true input envelope.
- **Follow-ups:** (1) default 16-bit highlight clipping → display-output roadmap;
  (2) Harman Phoenix dense base trips the `Dmax ≳1.0` floor + base-uniformity check
  → candidate new task (per-stock/dense-base Dmax handling); (3) widen
  `memory-preflight` sizing model to count IR + clone. No hard defects.
