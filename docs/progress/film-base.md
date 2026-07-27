# Negative Converter — film-base Progress Log

Execution log for the `film-base` epic: what was done and how, key decisions, what
works, what doesn't. TASKS.md holds the authoritative status (the checkboxes);
this file is the narrative beside it.

One `##` section per task in this epic, named by the bare task name (the part
after the `/`). Read this whole file before starting a task in this epic, and
read other epics' `Epic summary` sections when you depend on them. Append
entries — don't rewrite earlier ones.

## Epic summary

What other epics need to know about `film-base`:

- **`Dmin` and `Dmax` are different quantities that share this code.** `Dmin` is
  a per-channel **transmission** (the film base). `Dmax` is a **scalar** anchor in
  **density** units. Never conflate them (design-spec §4).
- **`estimate` returns `BaseEstimate { base, warnings }`** and **guards the
  resolved base finite-and-positive on every channel at birth** — a region on the
  dark holder errors loudly here, not silently downstream. The per-algo guards in
  `algo` remain defence-in-depth. Warnings ride to the report and `--strict`
  promotes them.
- **Real scans are laid out `dark holder → thin inset rebate → picture`** — the
  rebate is *not* the outer margin. Auto detection marches 1-px strips inward and
  takes the first uniform, value-continuous band sitting **immediately behind a
  holder run**; the brightest survivor wins. It uses **no orange/colour
  assumption** (holder-backing + flatness + brightness only), so a near-neutral
  base doesn't break it — **don't hard-code an orange-mask assumption anywhere.**
- **Auto is best-effort and refuses loudly** when it can't find a rebate. The
  supported workflow is measure once from an unexposed reference, then reuse:
  `nc estimate` emits paste-ready `--film-base` / recipe-fragment forms (only when
  the measurement would actually be accepted by `convert`).
- **IR is consumed here, and only here so far.** Under an explicit
  `--film-type chromogenic` on a scan with a *marker-verified* IR plane,
  `ir_holder_mask` masks the opaque holder before the RGB rebate search. `FilmType`
  (recipe key `input.film_type`) is a shared input-medium axis — `algo/bw-support`
  is expected to reuse it for IR dust removal. Known limitation: a thin holder
  margin that is IR-dark only in the shallow probe can hide a rebate behind it;
  the workaround is `--base-region`.
- **`Dmax` is roll-fixed, not per-frame.** The default is `Fixed` (nominal 2.0 in
  corrected-density units); `nc estimate --d-max-region` measures a calibrated
  scalar from a fully-exposed leader frame and emits it as `{"explicit": d}`.
  Per-frame `Auto` is demoted to opt-in and is the marker that a run is *not*
  film-master-compatible. A per-channel Dmax would smuggle in white balance, so
  the anchor is scalar by construction.
- **Reusing an explicit `Dmax` has a domain caveat:** it is measured against raw
  `D`, so non-default `density_scale`/`offset` or a non-neutral regional balance
  shift the `D′` domain and mis-anchor it. The orchestrator warns; heed it.
- **Known gap:** the reference-Dmax plausibility floor (`≳1.0`) and the
  base-uniformity check are C41-calibrated and **false-alarm on dense/neutral-base
  stocks** like Harman Phoenix (`film-base/dense-base-dmax-plausibility`).


## estimation
**Status:** done
**Updated:** 2026-06-30

- Goal: estimate `Dmin` `FilmBase` from border/region with full CLI override.
- **Done.** `pipeline/film_base.rs` implements `estimate(&LinearImage,
  &FilmBaseParams) -> Result<FilmBase>` as a thin `match` over the selected
  `FilmBaseSource`, delegating to pure helpers (`sample_region`, `auto_estimate`,
  `percentile`).
- **Rebased onto the merged `cli-framework` model (was originally built on the
  flat `FilmBaseParams`).** The foundation-review question "flat fields vs enum"
  was answered by `cli-framework`, not here: `FilmBaseParams` is now `{ source:
  FilmBaseSource }` where `FilmBaseSource = Auto | Region([u32;4]) |
  Explicit([f32;3])`. Precedence (`explicit > region > auto`) is therefore
  **structural** and resolved in `cli.rs`'s flag→recipe merge — `estimate` just
  honors whichever variant it's handed. I dropped my earlier `FilmBaseEstimate`
  return type and its separate report-enum (name-collided with the input
  `FilmBaseSource` and the merged stub is `-> Result<FilmBase>`); reporting *how*
  the base was chosen is derivable by the orchestrator from `params.source`.
- **Decisions (unchanged by the rebase):**
  - **Estimation statistic:** per-channel **97th percentile** (nearest-rank,
    `SAMPLE_PERCENTILE`) over the sampled pixels — resists hot pixels/dust while
    landing on the bright base (task suggested 95th–99th). `percentile` sorts NaNs
    to the end so they can't poison the rank.
  - **Region sampling** validates the rect against image bounds with u64 math
    (no u32 wrap near the edge); out-of-bounds or empty region → `NcError::Usage`
    (exit 2). `cli.rs` already rejects a zero-area `Region` at the boundary, but
    the bounds/empty check stays here as defense-in-depth (the CLI can't see the
    image dimensions, so OOB can only be caught in the stage).
  - **Auto border detection (Step-1 heuristic):** sample the outer margin band
    (`AUTO_MARGIN_FRAC = 4%` of the shorter side on all four edges), take the p97
    per channel as the candidate base, and accept only if (a) the band is
    near-uniform — per-channel relative spread `(p97−p10)/p97 ≤ 0.15` — and (b) the
    base is brighter than the interior **median** (median, not p97, so a sampled
    interior that clips a wide rebate doesn't defeat the check). On low confidence
    it returns a clear, actionable `NcError::Other` telling the user to pass
    `--film-base`/`--base-region` (per user decision: **hard error, no silent
    fallback** to whole-image sampling).
- **Notes for dependent tasks:**
  - `pipeline-orchestration` / `nc estimate`: `estimate` returns just the resolved
    `FilmBase`. For the JSON report, take the *source* label from `cfg.film_base
    .source` (you already hold it) rather than expecting it back from `estimate`.
    If a report ever needs the auto path's *detected* region, `estimate` will have
    to be extended to return it — today it doesn't (the auto sample is a spread
    edge band, not a single reusable `--base-region` rect).
- **Verify:** 8 unit tests in `film_base.rs` (explicit verbatim, region samples the
  rect, auto detects a bright uniform border, p97 rejects hot pixels, OOB/empty
  region → Usage error, auto fails loudly on no-border and on a non-uniform
  gradient, non-finite samples never become the base). Full suite **76/76**,
  `clippy --all-targets -D warnings` clean, `fmt` clean.
- **Ship review pass (4 agents):** applied the accepted findings — `percentile` now
  ranks over finite values only via `f32::total_cmp` (a NaN/±inf can never be
  returned as the base; comment was previously unsound); fixed a contradictory
  "densest" comment and softened the auto doc's over-claim (it can mis-anchor on a
  uniform bright surround — deferred to `auto-base-redesign`); cast the auto index
  math to `usize` first. Declined (with reasons): changing the auto heuristic now
  (that's the `auto-base-redesign` task, which gained a "must not mis-anchor on a
  bright surround" requirement) and the auto-failure `NcError` variant (Other/exit-1
  catch-all is defensible per §11).
- **Real-scan verification (throwaway `#[ignore]` probes, decoded via `io::decode`;
  probes not committed):**
  - Decoding works on every real scan tried: `../nc-assets/{48,64}bit-full/*`
    (3456×2396) and the full-res `~/Pictures/scan/20260630-nikon-84{2,4}.tif`
    (5184×3600 HDRi, after the decode preview-IFD fix above). Region/explicit
    `estimate` paths return sensible per-channel values on all of them.
  - **Real scans have a `holder → thin rebate → picture` structure, NOT a bright
    outer margin.** Marching a 1px strip inward from each edge: the outermost band
    is the near-black film **holder** (~0.01), then a **thin, bright, uniform
    orange film-base rebate** sits *behind* it, then the picture. The rebate only
    appears on some edges and can be a few px wide. Measured rebate is consistent
    per film stock (e.g. `48bit-full/1` bottom and `/2` left both ≈`[0.53, 0.26,
    0.16]`), confirming Dmin is a stock/develop/scanner property, not per-frame.
  - **The current outer-4%-margin auto heuristic can't isolate that rebate** — it
    averages holder+rebate+picture into one high-spread blob and **fails loudly**
    (correct fail-safe, exercised on real data), but the auto *happy path* does not
    work on real scans. A proper fix (scan strips inward, pick the brightest
    low-spread band past the holder) is **deferred** — see decision below.
  - **Decision (with user): focus on the explicit-reference workflow, not auto.**
    Because Dmin is constant across a roll scanned with fixed settings, the
    accurate path is: scan one **unexposed reference** frame once, measure its base
    with `--base-region`, and reuse it as `--film-base` across the batch (design's
    reusable-recipe idea). Verified end-to-end: the unexposed reference
    `20260630-nikon-844.tif` (same film/develop/scanner as the `842` scan) yields a
    large uniform base of **`[0.553, 0.271, 0.159]`** from a center region; `842`'s
    own left-edge rebate reads `[0.475, 0.236, 0.136]` and its picture center
    `[0.387, 0.189, 0.090]` (darker, as expected). Note the reference-vs-edge-rebate
    gap (~14%): the large clean unexposed area is the more reliable anchor than a
    narrow edge strip (edge falloff/fog) — another reason to prefer a dedicated
    reference frame.
- **Follow-up tasks noted (not in this branch):**
  - **Auto redesign:** inward-strip "brightest uniform band past the holder"
    detector so `--auto-base` works on real `holder→rebate→picture` scans. Deferred
    per the Step-1 "don't over-engineer auto" guidance now that the explicit path
    covers real work.
  - **White holder support:** some film holders are white, not black — auto/border
    logic assumes a dark surround. Add a CLI flag (e.g. `--holder white|black`) to
    tell the detector which. Follow-up.


## auto-base-redesign
**Status:** done
**Updated:** 2026-07-15

- Goal: robust `--auto-base` film-base detection on real scan layouts (dark
  holder → rebate → picture), replacing the best-effort Step-1 heuristic.
- **Scope note (2026-07-15):** the content-based source (`--base-content` /
  `film_base.source = "content"`) was **reassigned out of this task** to the new
  `film-base-content-fallback` task (see the authoritative "Scope change" note
  below). I had implemented it here; it has since been **removed** from this
  worktree — enum variant, flag + wiring, content-estimate logic, report shape,
  and its tests are gone. What remains of content mode here is a **one-line
  cross-reference**: the auto-refusal error message *suggests* `--base-content`
  (naming the owning task) and never silently falls back to it.
- **Done (kept scope).** `pipeline/film_base.rs` rewritten around an inward-scan
  detector, plus the two same-family items retained by the scope change: the
  `--base-region` uniformity warning, and `nc inspect` candidate regions. Whole
  task is pure functions; `cli`/`stages` only ferry warnings.
- **Detector shape** (`rebate_candidates` + `select_auto_base`; `auto_estimate`
  is their composition):
  - Per edge, march 1-px strips inward, up to `REBATE_SCAN_FRAC = 10%` of the
    short side (min 3 px). Strips are **trimmed by the scan depth at both
    ends**, otherwise the perpendicular edges' holder margins contaminate every
    strip (the reason the probe strips in the original verification looked
    dirty at the corners).
  - Per strip: per-channel p97 (`SAMPLE_PERCENTILE`) + worst-channel relative
    spread `(p97−p10)/p97`. Classes: **holder** (all channels p97 <
    `HOLDER_MAX_TRANSMISSION = 0.05`; real holder ≈ 0.01, dimmest real rebate
    channel ≈ 0.14, so 0.05 splits with margin), **uniform** (all-channel
    spread ≤ 0.15 — kept the strict all-channel gate), else **other**.
  - A candidate band is the **first** run of ≥ `MIN_BAND_STRIPS = 2` uniform,
    value-continuous strips (adjacent-strip step ≤ 10% per channel — this is
    what stops the band merging into an adjacent flat picture region) sitting
    **immediately behind a contiguous holder run**; the whole band is then
    re-measured as one region and must pass the spread gate again (catches slow
    drift). Bands at depth 0 (no holder outside) are rejected.
  - Selection: candidates must beat the frame-interior **median** on **every**
    channel by ≥5% (`INTERIOR_BRIGHTNESS_MARGIN`, replacing the old lenient
    any-channel/2% gate); the **brightest** survivor wins.
- **Key decisions and why:**
  - **The corroborating anti-bright-surround signal is holder-backing, not
    mandatory cross-edge agreement.** A bright surround bleeding to the frame
    edge has no dark holder outside it → no candidate → refusal. Mandatory
    cross-edge agreement was rejected because a real rebate legitimately appears
    on a single edge (verified: `48bit-full/2` left-only). Cross-edge
    *disagreement* between surviving candidates is a report **warning**
    (`--strict`-promotable), not an error.
  - **"Brightest candidate wins" is physically grounded:** the rebate is Dmin =
    per-channel max transmission; no genuine picture area can out-bright clean
    base. This is also why a uniform dark band behind the holder can never
    out-rank a real rebate (unit-tested).
  - `estimate` now returns **`BaseEstimate { base, warnings }`**; the region
    path's uniformity check emits a warning (never alters the value), rides
    `Rendered.film_base_warnings` through `stages::render` into the report, and
    `--strict` refuses it (e2e-tested).
  - `percentile` switched from full sort to `retain(finite)` +
    `select_nth_unstable_by` (O(n), still deterministic — an order statistic is
    tie-order independent).
  - `inspect` now reports `base_candidates` (edge, `--base-region`-ready rect,
    value, spread) even when selection refuses, so users confirm a rectangle
    instead of measuring one.
- **Verification:** unit tests in `film_base.rs` (single/two-edge rebate,
  bright-surround refusal naming the recovery flags, dark-band out-ranking,
  disagreement warning + within-tolerance no-warning, mixed-region warning with
  unchanged value, degenerate-region-base rejection, candidate serde-shape +
  rect round-trips through `Region` incl. the mirrored bottom-edge arithmetic,
  plus the retained explicit/region/percentile suite); e2e test (mixed
  `--base-region` warning + `--strict` refusal on both `convert` and `estimate`).
  Full gate clean on the rebased base: fmt / clippy `-D warnings` / build /
  **145 unit + 19 e2e**.
- **Post-review pass (2026-07-14, pr-review-toolkit 5-agent review of the
  working-tree diff; findings fixed):**
  - **Degenerate base rejected at birth** (type-design + silent-failure, HIGH):
    `estimate` now runs `guard_base` over every source, erroring loudly on any
    zero / negative / non-finite channel. Previously a region on the holder could
    return a poison base with exit 0 and no warning — `nc estimate` has no
    downstream algo to catch it. Closes the "reject degenerate bases at birth"
    follow-up noted in the CLAUDE.md film-base gotcha (per-algo guards stay as
    defense-in-depth); CLAUDE.md updated to match.
  - **Estimation moved out of `stages::render`** (silent-failure, MEDIUM):
    `run_convert` now resolves the base and pushes its warnings *before* the
    fallible render, so a downstream render error can't swallow the "non-uniform
    region" line explaining a bad base. `render` takes a resolved `&FilmBase`;
    `Rendered` lost its `film_base`/`film_base_warnings` fields (the orchestrator
    owns them). This also tightens the stage split (estimation = stage 2,
    render = stages 3–4).
  - **`nc estimate --strict`** (silent-failure, MEDIUM): the base-producing
    command now promotes its warnings to a failing (non-zero) exit, so a script
    baking a Dmin into a recipe short-circuits on a plausible-looking-but-bad
    region. The report (including the warnings) still emits *before* the gate —
    matching `convert` — so the signal is the non-zero exit code, not a suppressed
    value; a consumer must gate on the exit code. Makes the `BaseEstimate`
    "`--strict` promotes" doc true on every warning-producing path.
  - **Minor:** unified auto-refusal recovery wording into one `RECOVERY_ADVICE`
    const (all refusals, incl. the too-small case, *suggest* `--base-content` as
    the owned-elsewhere fallback); doc fixes (candidates are pre-brightness-gate;
    `percentile` is rounded-rank not nearest-rank; `select_auto_base` names the
    5% margin + same-image contract). Declined: `base_candidates: Some(vec![])`
    for "ran, found nothing" — the adjacent "unavailable" warning already
    disambiguates; not worth the shape change. Review came back clean after the
    fixes.
- **Rebased onto origin/main `3c7f5bd` (2026-07-15)** to pick up #20's
  `--out-depth`→`--output-hdr` rename, the merged bw-support docs (#19/#21), and
  the #22 scope-change note. Conflicts resolved in `design-spec.{md,html}` and
  `stages.rs` (the render test now uses `hdr: true`, not `OutDepth::F32`); then
  the content-source removal above was applied on the new base.
- **Real-scan status:** the full-size scans (`../nc-assets`, `~/Pictures/scan`)
  are **not present in this environment** — only the committed 502×462 fixture
  crops, which are picture-interior crops (probe: all strips high-spread, no
  holder). On them the detector correctly refuses and the region-warning behaves
  as designed. Thresholds are set from the numbers recorded in the
  `film-base-estimation` real-scan verification (holder ≈0.01, rebate
  ≈[0.53,0.26,0.16], picture spread ≫0.15); **running the detector's happy path
  on the full-size scans still needs doing — fold it into
  `real-scan-verification`** (its task already covers default-output checks).
  Note the follow-ups `ir-holder-detection` and `auto-base-neutral-stock` layer
  on this: the detector deliberately uses **no** orange/colored-base assumption
  (holder-backing + flatness + brightness are color-independent), so a
  near-neutral base (Harman Phoenix, R/B ≈ 0.84) does not break the confidence
  gates.
- **Notes for dependents:**
  - `white-holder-support`: the polarity assumption lives in exactly two spots —
    `StripClass::Holder` classification (`HOLDER_MAX_TRANSMISSION`) and the
    doc'd "holder-backing" rationale. A `film_base.holder = white` knob should
    flip the holder test to "very bright on all channels" (and then the
    "brightest survivor" rule needs care: a white holder is brighter than the
    rebate, but it sits *outside* the band, so selection logic is unchanged —
    only classification flips). `Edge`/`RebateCandidate` are already public.
  - `estimate-reuse-output`: `BaseEstimate.warnings` and
    `Report.base_candidates` are the hooks for the reuse-ready output; the
    candidate `region` is already `--base-region`-shaped.

### Scope change — content-based source reassigned (2026-07-15)

A design pass (Phoenix/Ektar real-scan verification + workflow discussion) moved
work out of this task. The task file couldn't be edited during the pass (agents
active on it), so this note is the authoritative redirect for whoever picks it up.

**Remove from scope — the Content-based source bullet.** The "Also in this task's
scope" section lists a **Content-based source (ladder Tier 3)** item — the
`film_base.source = "content"` variant, the `--base-content` flag, per-channel
high-percentile of exposed content, its report wiring, and its tests. **That is
now owned solely by the new `film-base-content-fallback` task**
(`docs/tasks/film-base-content-fallback.md`). Drop it from this task's
implementation *and its verification* so the two tasks don't both build the same
enum/flag/report/tests.

**Keep in scope (unchanged):** the inward-scan detector, the **uniformity warning
on `--base-region`**, and **`nc inspect` reporting candidate rebate regions**. The
only remaining content-mode responsibility here is a **one-line cross-reference**:
when auto-detection refuses, the failure message should *suggest* `--base-content`
(never implement it or silently fall back).

**Two follow-ups now layer on top of this task (no action needed here, but read
them so you don't bake in assumptions they'll have to unwind):**

- `ir-holder-detection` — uses the IR plane to mask the holder (0–4 edges)
  content-independently, feeding the RGB rebate search; may replace the RGB-only
  holder-classification step where IR is present. Largely sidesteps
  `white-holder-support` (opacity, not color, is the IR signal).
- `auto-base-neutral-stock` — hardens detection for near-neutral bases (Harman
  Phoenix, R/B ≈ 0.84) where base color isn't a usable discriminator. Real-scan
  verification found opposite bases across stocks (Ektar orange R/B 2.73, Phoenix
  neutral 0.85), so any confidence gate that assumes a colored/orange base needs a
  color-independent corroborator (flatness / geometry / cross-frame value
  agreement). **Don't hard-code an orange-mask assumption.**

Both are tracked in `TASKS.md` as dependents of this task.
- 2026-07-27: Epic-migration redirect — the content-fallback task path cited above
  is now [film-base/content-fallback](../tasks/film-base/content-fallback.md).
  The entry above is preserved verbatim.


## white-holder-support
**Status:** not started
**Updated:** —

- Goal: support scans made in light/white film holders, where the current
  darker-than-interior assumptions of base estimation don't hold.


## estimate-reuse-output
**Status:** done
**Updated:** 2026-07-14

- Goal: `nc estimate` output shaped for direct reuse (drop-in recipe fragment /
  flag values), closing the measure-once-reuse-for-the-roll loop.
- **Done.** Full CI gate clean (`fmt --check`, `clippy --all-targets -D
  warnings`, `build`, `test`); suite is **208 tests** (182 unit + 26 E2E; the
  counts include tests other tasks added under the same base).
- **Rebased onto `origin/main` 94fdc12 (2026-07-16).** Post-merge of #23
  (`auto-base-redesign`: inward-scan rebate detector — `estimate` now returns
  `BaseEstimate { base, warnings }`, plus `RebateCandidate`/`Edge`/`guard_base`)
  and #27 (`algo-sigmoid`). Conflicts in film_base.rs, cli.rs, tests/pipeline.rs,
  and both design-spec files, all reconciled keeping BOTH sides:
  - **film_base.rs:** my `estimate_grid`/`GridEstimate`/`GridCell` now sit on the
    new detector. Grid cells switched from the old `sample_region` (which now
    returns `BaseEstimate`) to `sample_region_at(_, SAMPLE_PERCENTILE)` (still
    `FilmBase`). Dropped the two obsolete `auto_fails_*` unit tests that #23
    deleted; kept all grid tests.
  - **cli.rs:** `run_estimate`'s non-grid branch adopts the `BaseEstimate` API
    (`est.base` + folds `est.warnings`); grid branch and reuse-ready output
    unchanged. `Report` keeps both #23's `base_candidates` and my
    `film_base_flag`/`film_base_recipe`/`grid`. Deduped a doubly-added
    `EstimateArgs.strict` (both #23 and I added `--strict` to estimate) into one
    field with a merged doc.
  - **tests:** kept #23's `mixed_base_region_warns_and_strict_refuses_it` plus my
    two estimate tests. Dropped the `--strict` from the reuse round-trip test:
    the new inward-scan uniformity gate warns on any `--base-region` of this
    real-photo fixture (no region-uniform patch exists), so a clean-strict-exit-0
    assertion isn't expressible here — `estimate --strict` is covered by #23's
    mixed-region test instead.
  - **docs:** subcommand table keeps #23's richer `inspect` row + my `estimate`
    row; the §8 ladder Tier-1 keeps #23's inward-scan detail with my
    `nc estimate --grid` reference.
- **Rebased onto `origin/main` 3c7f5bd (2026-07-15).** Post-merge of #20
  (`--out-depth u16|f32` → the boolean `--output-hdr`), #21 (bw-support docs),
  and #22 (roll-workflow/versioning follow-up tasks + acquisition ladder). Only
  code change from the rebase: the two `convert` round-trip invocations in the
  reuse E2E test switched `--out-depth f32` → `--output-hdr` (both still set
  `output.hdr = true`, so the byte-identical A/B comparison holds). My
  estimate/grid/reuse surface does not touch `OutDepth`, so cli.rs merged
  cleanly. Design check against the new tasks: my Tier-1 `estimate`
  reader + `film_base_flag`/`film_base_recipe` are exactly what
  `base-acquisition-planner` (Tier 1) consumes and `roll-conversion` applies —
  complementary, no overlap; I use only `Explicit`/`Region`/`Auto` sources, not
  the `--base-content`/`FilmBaseSource::Content` owned by
  `film-base-content-fallback`; and I touch no `Dmax` surface (owned by
  `dmax-reference`). §8/§9 grid + reuse wording sits inside the existing
  acquisition ladder without duplicating the new task specs.

### What was built
- **Reuse-ready report fields (`cli.rs`).** The `estimate` report now carries,
  beside the raw `film_base` measurement:
  - `film_base_flag` — a paste-ready `"--film-base R,G,B"` string. Values are
    formatted with `f32`'s `Display` (shortest round-tripping decimal), so
    parsing them back yields the **bit-identical** measured `f32`s.
  - `film_base_recipe` — the same measurement as a `FilmBaseParams` fragment,
    serializing to the documented `{"source":{"explicit":[r,g,b]}}` shape; it
    parses back both standalone and as a recipe's `film_base` section
    (unit-tested against `deny_unknown_fields`).
  - Both are attached **only when the measurement passes the same
    explicit-base validation `convert` applies** (each channel in `(0, 1]`).
    A degenerate measurement (e.g. a region on the dark holder sampling ~0)
    is still reported as `film_base`, but a warning explains why no
    reuse-ready output was emitted — "reuse-ready" therefore implies "will be
    accepted by convert".
- **Grid calibration mode (`estimate --grid`, `pipeline/film_base.rs`).**
  `estimate_grid(image, rect)` samples a fixed 5-cell grid (top-left,
  top-right, bottom-left, bottom-right, center; each cell 25% × 25% of the
  rectangle, ≥1 px, same 97th percentile as single-region sampling) over the
  full frame or `--base-region`. Returns `GridEstimate { base, cells, spread,
  tolerance, agreement }`:
  - combined `base` = per-channel **median** across cells (robust to one bad
    cell, deterministic);
  - `spread` = per-channel `(max − min) / max` across cells, judged against
    the documented `GRID_MAX_RELATIVE_SPREAD = 0.05`;
  - disagreement is **diagnostic, not fatal**: the CLI pushes a report warning
    naming the spread/tolerance and pointing at the per-cell evidence in
    `grid.cells` (never averaged away silently).
- **`--strict` on `estimate`.** Promotes any estimate warning (grid
  disagreement, decode notes, unusable-base) to exit 1 *after* the report is
  emitted — same contract as `convert`.

### Key decisions / notes for dependents
- **`--grid` is an estimate-only CLI mode, not a recipe key.** It configures a
  *measurement/diagnostic* of the `estimate` command (like `--report`), not a
  conversion knob: `convert` never grid-samples — the workflow is measure once
  with `estimate`, then freeze the explicit value via the emitted flag/fragment.
  So the four-spot knob wiring (Overrides/Params/merge/validate) deliberately
  does not apply; the recipe surface is unchanged. clap-conflicts with
  `--film-base` (nothing to sample) and `--auto-base` (grid replaces border
  detection); compatible with `--base-region` (grid within the rectangle).
- **Report `film_base_source` under `--grid`** is the overall rectangle sampled
  (`{"region":[x,y,w,h]}`, full frame when no `--base-region`); the new `grid`
  report object documents the per-cell method. No new `FilmBaseSource` variant —
  grid never enters the convert/recipe surface.
- `GridEstimate`/`GridCell` live in `pipeline/film_base.rs` (Serialize-only,
  embedded in `Report` like `DecodeInfo`), keeping report-shape types beside
  the stage that produces them.
- Verified on the committed real fixtures: `estimate --base-region 0,0,60,60`
  emits flag + fragment; full-frame `--grid` on a real (non-blank) frame
  disagrees as expected (spread ≈ 0.42–0.56 ≫ 0.05) → warning, exit 0, and
  exit 1 under `--strict`. E2E round-trip test feeds both the flag string and
  the fragment (as `--params` recipe) back into `convert` and asserts the same
  base and **byte-identical outputs** between the two reuse forms.
- Docs: design-spec §8 (estimate example now shows the real report shape and
  the grid mode), §9 ladder tier 1 + Global `--strict`, roadmap item 10 marked
  shipped — `.md` and `.html` edited together.

### Review
- Ran `pr-review-toolkit:review-pr` (code / tests / silent-failure / type-design
  / comments) — 1 full round + 1 confirmation round; confirmation came back
  clean.
  - **Fixed from round 1:** extracted the reuse-output computation into the
    pure `reuse_ready(rgb) -> Option<(String, FilmBaseParams)>` so the
    degenerate-suppression branch is unit-testable (and tested);
    `GridEstimate.cells` tightened from `Vec<GridCell>` to `[GridCell; 5]`
    (expresses the fixed layout at compile time, identical JSON, removes a
    latent wrong-median hazard if the count ever changed); the grid warning
    now distinguishes a *degenerate sample* (combined base channel ≤ 0) from
    genuine *disagreement* (light leak/falloff/dust) so the diagnostic names
    the actual problem; doc qualifiers (reuse fields are conditional; derived
    grid fields; spread-sentinel ambiguity); added tests — all-black-frame
    spread sentinel (guards against `0/0 = NaN` → `null` in the report),
    clean `estimate --strict` exits 0, grid runs emit the reuse fields.
  - **Deliberate, not fixed:** reuse fields are still emitted when a grid
    *disagrees* but the combined base is valid — the median is designed to
    resist one bad cell, the disagreement warning is loud, and `--strict`
    fails the run; scripted consumers that want hard safety use `--strict`.
  - **Disputed/deferred:** a plausibility floor for tiny-but-positive bases
    (e.g. `0.002` from a region on the dark holder) was suggested (silent-
    failure review, MEDIUM). Deferred: any threshold is arbitrary (a dense
    orange mask legitimately scans B ≈ 0.03 on real fixtures), the emitted
    value *is* accepted and correctly processed by `convert`, and design-spec
    roadmap item 8 (`auto-base-redesign`) already owns `--base-region`
    plausibility/uniformity warnings — recorded there rather than inventing a
    magic constant here.

### Post-merge review fix (2026-07-17) — grid degenerate base now hard-errors
Six-reviewer pass (Codex + 5 lenses) on the reuse-output change found one
warranted correctness gap: the `--grid` path only *warned* on a degenerate
combined base (any channel non-finite or ≤ 0 — e.g. `--grid --base-region` on the
dark holder) and exited 0 unless `--strict`, while the single-measurement path
hard-errors on the identical condition at birth via `film_base::estimate`'s
finite-and-positive guard. That asymmetry violated the CLAUDE.md film-base gotcha
and design-spec §11 fail-loudly.
- **Fix:** `cli::run_estimate` now, in the `--grid` branch, hard-errors after
  `emit_report(...)` when the combined base has any non-finite or ≤ 0 channel —
  **regardless of `--strict`** — using the same `NcError::Other` (exit 1) the
  single-path guard returns, so both estimate paths map a degenerate base to one
  exit code. The per-cell "grid measured non-positive transmission…" warning
  still rides the emitted report as diagnostics; the diagnostic report (with
  `grid.cells`) lands on stdout *before* the gate, same emit-before-gate contract
  as the `--strict` check.
- **Docs:** design-spec §8 (grid behavior) and §11 (exit code) updated in both
  `.md` and `.html` to state `--grid` errors (exit 1) on a degenerate combined
  base regardless of `--strict`.
- **Tests added:** `film_base.rs` — odd, non-square grid rect (cell sizing /
  center-origin arithmetic + in-bounds), single-channel disagreement drives the
  agreement verdict, and a note-pinning test that `estimate_grid` itself reports
  (not errors) a degenerate base (the hard error is the caller's job).
  `tests/pipeline.rs` — e2e `estimate --grid` on an all-black frame exits 1
  without `--strict`, matching the single-path degenerate exit, with the report
  emitted first. New committed fixture `tests/fixtures/black-48bit.tif` (64×64
  all-zero 16-bit RGB) supplies the all-black input (generated once via a
  throwaway in-crate generator, since integration tests can't reach the `tiff`
  crate).

### Post-merge follow-ups (2026-07-17) — type-design + doc from the same review
Three further items the user decided after the review:
- **(doc, no behavior change) Reuse output survives grid disagreement.** Made
  explicit that a cells-disagree *warning* does NOT suppress the reuse-ready
  output — the combined median resists a single bad cell, so the base is still
  offered; consumers check `warnings` or run `--strict`. Documented in
  design-spec §8 (md+html) and at the `reuse_ready` emission site in
  `run_estimate`. Only a *degenerate* base withholds the reuse forms (and it is a
  hard error).
- **(Type #1, done) Collapsed the parallel reuse `Option`s.** `Report`'s
  `film_base_flag: Option<String>` + `film_base_recipe: Option<FilmBaseParams>`
  (illegal flag-without-recipe was representable) became one
  `reuse: Option<ReuseReady>` where `struct ReuseReady { flag, recipe }` — both-or-
  neither is now unrepresentable (the CLAUDE.md parallel-`Option` anti-pattern).
  Wire shape is **byte-identical**: `#[serde(flatten)]` on the `Option` plus
  `#[serde(rename = "film_base_flag" / "film_base_recipe")]` on the struct fields
  keeps the two flat top-level keys (present together / both absent); the `reuse`
  wrapper name never appears on the wire. Safe because `Report` is serialize-only
  (no `Deserialize`/`deny_unknown_fields`, so no flatten conflict). Added a
  snapshot unit test (`report_reuse_flattens_to_flat_keys_or_nothing`) locking the
  shape; existing round-trip/e2e tests pass unchanged.
- **(Type #2, deferred to a task) `GridEstimate.agreement: bool` limitation.**
  Documented on the type that `agreement=false` conflates *disagree* vs
  *degenerate* and that the `spread` `1.0` value is an overloaded sentinel, so the
  CLI re-derives the case from the base. Kept as-is for now; created
  `docs/tasks/grid-verdict-enum.md` (replace the bool + sentinel with a
  `GridVerdict { Uniform | Disagree | Degenerate }` enum; deps
  `estimate-reuse-output`, `film-base-estimation`; post-MVP) and wired it into
  `TASKS.md` (Mermaid graph, dependency list, Phase 5 checklist as `[ ]`).

### Follow-ups / deferred (with reasons)
- Grid tolerance (0.05) is a documented constant, not a flag — make it a knob
  only if real blank-frame scans show legitimate falloff above 5%
  (`real-scan-verification` can inform this).
- `inspect`'s suggested-Dmin output does not carry the reuse fields (kept
  estimate-only per the task scope; trivial to add later if wanted).
- `--base-region` plausibility (dark-holder detection / uniformity warning)
  stays with `auto-base-redesign` (roadmap item 8), which owns that diagnostic.
- If grid sampling ever becomes a `convert`-usable source, it must join
  `FilmBaseSource` as a variant (one enum per the conventions), not a bool
  beside it.
- 2026-07-27: Epic-migration redirect — the grid-verdict-enum task path cited above
  is now [film-base/grid-verdict-enum](../tasks/film-base/grid-verdict-enum.md).
  The entry above is preserved verbatim.


## dmax-reference

Made the display-white anchor `Dmax` a **roll-fixed calibration** (like `Dmin`)
instead of a per-frame measurement, and changed the default render accordingly.
Uncommitted (the user runs the review loop, then reviews manually).

### What changed

- **`DmaxSource` enum extended** (`types.rs`) from `{ Auto(default), Explicit,
  None }` to `{ Fixed(default), Explicit, Auto, None }` — one enum, not parallel
  fields (the flags-win merge stays sound). Wire forms: `"fixed"` /
  `{ "explicit": <d> }` / `"auto"` / `"none"`. `DensityParams::default().dmax` is
  now `Fixed`.
  - **`Fixed`** = the roll-fixed **nominal** anchor `algo::density::NOMINAL_DMAX =
    2.0` (a scene-independent placement **in corrected-density units**, not base
    transmission + range). The default when nothing is calibrated.
  - **`Explicit(d)`** now documents the roll-fixed **calibrated** value (measured
    reference / per-stock constant), reused across the roll like an explicit
    `--film-base`. This is the form a roll recipe freezes.
  - **`Auto`** kept but **demoted** to an opt-in (`--auto-d-max`), documented as
    per-frame exposure normalization (the 99.5th-percentile measurement is
    unchanged — only its priority moved).
  - **`None`** unchanged (bit-exact scene-referred escape hatch preserved).
- **Resolution order for the default** (design §7.2): measured reference →
  per-stock constant → nominal. Realized as: a measured/stock value rides in
  `Explicit`; `Fixed` is the no-calibration nominal fallback. `resolve_dmax`
  (`density.rs`) gained the `Fixed => Some(NOMINAL_DMAX)` arm; `Fixed` ignores the
  buffer, so every frame gets the same anchor (the roll-fixed property).
- **Plan-phase reference measurement** (`estimate --d-max-region X,Y,W,H`), the
  mirror of `--base-region` for `Dmax`. Point it at a fully-exposed (near-opaque)
  reference frame — the light-struck leader — with the roll's `Dmin` as
  `--film-base`; it samples the region's **median** transmission
  (`film_base::sample_region_at`, now `pub(crate)`; median is robust to dust on a
  near-opaque frame without a uniformity gate — relative spread on near-zero
  transmissions is noise-dominated and would false-alarm), reduces the per-channel
  corrected density to **one scalar** via a gray/mean reduction
  (`algo::density::reference_dmax`), and reports it. A per-channel `Dmax` would
  smuggle in white balance (three different gains in the exponent), so the anchor
  stays a scalar by construction; the unit test
  `reference_derived_dmax_introduces_no_per_channel_correction` proves the applied
  gain is identical across channels.
- **Freeze = scalar, provenance = report.** `estimate --d-max-region` emits
  reuse-ready forms — `d_max_flag` (`--d-max <d>`) and `d_max_recipe`
  (`{ "dmax": { "explicit": <d> } }`, drops into a recipe's `density` section) —
  and records the sampled region as **`dmax_region` provenance only**. There is
  **no** `{ "reference": … }` recipe form: the frozen recipe carries the scalar so
  the apply phase re-reads nothing (the deterministic-apply / same-recipe-hash
  contract stays intact). The e2e test
  `estimate_measures_roll_fixed_dmax_from_a_reference_region_and_it_round_trips`
  drives estimate → freeze (flag and recipe) → convert and asserts byte-identical
  output.
- **Four-spot wiring** for the new `--fixed-d-max` flag and reworked group
  (`cli.rs`): CLI `DmaxOverrides` (added `--fixed-d-max`, all four mutually
  exclusive), recipe `DensityParams.dmax`, the `merge` arm, and `validate`
  (`Fixed`/`Auto`/`None` need no value check; `Explicit` stays positive-finite).
  `--fixed-d-max` exists so a recipe's explicit/auto is CLI-overridable back to the
  default (an absent presence flag never clobbers a recipe value). Merge test
  `merge_dmax_flags_map_to_the_source_enum` extended; conflict + validate tests
  updated.
- **Sigmoid** unchanged in behavior: it still requires a positive anchor, and the
  new default `Fixed` (2.0 > 0) satisfies it, so `--algorithm sigmoid` works by
  default now (previously the default `Auto` also satisfied it). `anchor_error`
  matches on the resolved `Option<f32>`, not the source enum, so adding `Fixed`
  needed no change there; the CLI usage message was reworded to mention the default
  fixed anchor.

### pipeline_version bump — reconcile note (needs orchestrator attention)

This **changes the default render** (frame-local `auto` 99.5th percentile → fixed
nominal `Dmax = 2.0`). The task said to bump `pipeline_version` by hand. **There is
no `pipeline_version` code constant to bump yet** — `conversion-versioning` has not
shipped (grep: `pipeline_version` appears only in `docs/`, and
`docs/reports/v0-baseline.md` records "pipeline_version 0" as prose). I did **not**
fabricate a constant: adding an unconsumed `const` trips clippy `-D warnings`
(dead_code), and wiring it into the report/telemetry record is `conversion-versioning`'s
design surface (and would collide with the parallel `perf-tel-fix` work). So:
- Documented the change as **superseding v0** in design-spec §12 (item 14 now
  "Implemented", and notes the default-render change must be `pipeline_version` ≥ 1).
  (§7.2 documents the roll-fixed anchor itself but does not mention v0 /
  `pipeline_version` — only §12 does.)
- `v0-baseline.md` already anticipates this exact rework as the future **`v1`**
  ("a later `v1` (auto white-balance, tone curve, Dmax rework)"), so its recorded
  numbers stand as the v0 reference and need no edit.
- **Action for `conversion-versioning`:** when it lands, this default must be
  labeled `pipeline_version 1` (not `v0`), and its golden-output gate should treat
  this commit as the v0→v1 boundary for the density default.

### Reconcile with parallel tasks

- **`roll-conversion`** (parallel worktree) consumes `density.dmax` from a frozen
  recipe. The frozen form this task produces is exactly
  `density.dmax = { "explicit": <scalar> }` (from `estimate --d-max-region`'s
  `d_max_recipe`, or a hand-set `--d-max`), which is what a roll recipe carries —
  no re-read directive, deterministic apply. No code dependency either way; the
  enum default change (`Fixed`) is transparent to a roll recipe that sets `dmax`
  explicitly.
- **`conversion-versioning`** — see the pipeline_version note above.

### Verification

- `cargo test`: 250 unit + 50 e2e = **300 passed, 0 failed**. New tests:
  `fixed_anchor_resolves_to_the_nominal_constant`,
  `reference_dmax_is_the_gray_mean_of_base_relative_density`,
  `reference_dmax_rejects_a_non_opaque_region`,
  `reference_derived_dmax_introduces_no_per_channel_correction` (density.rs);
  `dmax_reuse_fragment_round_trips_as_a_recipe`, `estimate_parses_d_max_region`
  (cli.rs); `estimate_measures_roll_fixed_dmax_from_a_reference_region_and_it_round_trips`,
  `convert_default_uses_the_fixed_roll_anchor_not_per_frame_auto` (pipeline.rs).
  The all-black fixture stands in for the near-opaque leader (no real leader frame
  committed; **real-leader verification, Ektar 1009 / Phoenix 1010, deferred to the
  user** per the task).
- CI gate (`fmt` / `clippy -D warnings` / `build` / `test`): clean.

### Review-fix loop (2026-07-19)

A two-engine review (Codex + 5 pr-review lenses) ran over the uncommitted changes;
the verified findings were applied (still uncommitted). What changed:

- **`reference_dmax` hardened against silent wrong-calibration** (`density.rs`).
  Previously it only rejected a non-positive *averaged* gray density. Now, on
  **every** channel, before the gray reduction: (a) a transmission that is
  non-finite or at/below `SCAN_EPSILON` is a **hard error** — a floored channel
  (dead sensor / clipped black / dark holder) must not be laundered into `D ≈ 6`
  and freeze a black-rendering anchor (the `Dmin` "zero channel errors loudly"
  gotcha, applied to `Dmax`); (b) each channel's base-relative density must be
  `> 0` (a colored/wrong region can average positive while one channel
  out-transmits the base). All hard errors are `NcError::Other` (exit 1),
  consistent with the mirrored `Dmin` guard and the existing test — **not** the
  `Usage`/exit-2 that finding 1(a) literally named (that contradicted its own
  "mirror the Dmin guard" instruction and the algo-layer convention).
- **Plausibility warning** (`density.rs` `MIN_PLAUSIBLE_REFERENCE_DMAX = 1.0`,
  warned in `cli::run_estimate`). A positive-but-implausibly-low reference density
  (a mid-tone region, not a leader) is **not** rejected (thin stock varies) but
  emits a loud, `--strict`-promotable warning. Threshold `1.0` is conservative — a
  full density decade below the base, well under a real leader's `≈ 2–3`.
- **Domain guard (finding 3 — fallback approach taken).** The real domain fix
  (thread `density_scale`/`offset` into the estimate path) is genuinely infeasible
  in scope: `estimate` resolves only a film base, has no density-correction params,
  and does *not* build a `ResolvedConfig` (the finding's premise was inaccurate).
  Instead, `cli::run_convert` now emits a `--strict`-promotable warning when an
  explicit `--d-max` is combined with non-default `density_scale`/`density_offset`
  on a density-domain algorithm — the anchor (raw `D`) would otherwise be in a
  different domain than the render's corrected `D′`. `reference_dmax`'s doc now
  documents both the scale/offset caveat and the spatial regional-balance caveat
  (a non-neutral balance can't fold into any scalar anchor).
- **Reuse gating consistency (finding 4).** The reuse-ready `--d-max` / `density.dmax`
  forms are now gated on the same `(0, 1]` base-usability check the film-base reuse
  uses (`validate_explicit_film_base`), not merely `finite && > 0`. A base in
  `(1, ∞)` still emits the diagnostic `dmax`/`dmax_region` but no longer advertises
  a reuse-ready `--d-max` measured against a base that isn't a valid `--film-base`.
- **Docs.** §7.3 MD sigmoid-anchor sentence aligned with the HTML (the default
  `fixed` nominal, not auto-by-default). §12 item 14 `pipeline_version` reworded as
  a **deferred** obligation (no constant exists yet) in md+html, and the stale
  "§12 item 12" cross-ref corrected to item 16. §8 example terminology fixed:
  raw `-log10(t/base)` is **base-relative density D** (= corrected density under
  default scale/offset), not "corrected density" (§4). Progress note above
  corrected: only §12 (not §7.2) mentions v0/`pipeline_version`.
- **Tests.** The reference-region e2e round-trip now synthesizes a **near-opaque
  non-zero** leader (~2% transmission) via a new `tiff` dev-dependency + in-test
  TIFF generator, instead of the all-black fixture (now a guarded error). Added:
  degenerate all-black region → hard error; per-channel out-transmitting region →
  hard error; floored/zero channel → hard error (`density.rs`); implausibly-low
  reference → strict-promotable warning (e2e); degenerate grid base → `--d-max-region`
  skipped, still hard-errors (e2e); `sample_region_at` median (`p = 0.5`) on a
  non-uniform region differs from the high/low percentiles (`film_base.rs`).

### Review-fix loop (2026-07-21)

A second review pass (Codex P2 findings) over the still-uncommitted changes. Two
verified findings applied:

- **B1 — regional balance folded into the Dmax domain guard** (`cli.rs`). The
  explicit-`Dmax` domain-mismatch warning previously fired only on non-default
  `density_scale`/`density_offset`. But the render subtracts `Dmax` from
  `D′ = B + shadow_balance·w_lo(D̄) + highlight_balance·w_hi(D̄)`, so a **non-neutral
  regional balance** also shifts that domain — a reference/explicit `--d-max` reused
  with a non-neutral shadow/highlight balance silently mis-anchored with no warning.
  This supersedes the prior "spatial balance can't fold into any scalar anchor,
  documentation-only" stance (2026-07-19 note): it can't be *folded into* the anchor,
  but a non-neutral balance still shifts `D′`, so the fixed anchor still mis-anchors —
  hence it now belongs in the guard. The guard decision was extracted into a pure,
  testable `explicit_dmax_domain_warning(&ResolvedConfig) -> Option<String>`; the
  message now names regional balance alongside scale/offset. Test:
  `explicit_dmax_domain_warning_fires_on_nonneutral_regional_balance`.
- **B2 — per-channel reference plausibility** (`density.rs` + `cli.rs`). The
  implausibly-low warning checked only the averaged scalar `dmax`, so a colored region
  with one dense channel and others barely above base cleared it (base `[1,1,1]`,
  transmissions ≈ `[0.001, 0.99, 0.99]` → per-channel densities ≈ `[3.0, 0.004, 0.004]`,
  avg ≈ 1.0 > threshold; the `d ≤ 0` hard error doesn't catch positives). `reference_dmax`
  now returns a `ReferenceDmax { scalar, per_channel }` (the scalar value is unchanged —
  still the gray mean); the `d ≤ 0` per-channel hard error is kept. The estimate-side
  plausibility decision was extracted into a pure
  `reference_dmax_plausibility_warning(&ReferenceDmax) -> Option<String>` with two
  distinct, mutually-exclusive shapes: (a) sub-floor gray mean → the existing frame-thin
  warning; (b) plausible mean but weakest channel below `MIN_PLAUSIBLE_REFERENCE_DMAX` →
  a new colored/wrong-region warning. Tests: `reference_dmax_exposes_a_weak_channel_a_plausible_scalar_hides`
  (`density.rs`, the data) and `reference_dmax_plausibility_warns_on_a_weak_channel_a_plausible_scalar_hides`
  (`cli.rs`, the wiring, covering all three branches).
- Determinism, `None` bit-exactness, four-flag mutual exclusivity, and the
  four-coupled-spots invariant are untouched — both fixes only add/route report
  warnings (no image-output change). CI gate (`fmt` / `clippy -D warnings` / `build` /
  `test`): clean.


## ir-holder-detection

### 2026-07-24 — IR-assisted film-holder mask + `--film-type` knob

Implemented the first real consumer of the decoded IR channel: a pure,
segmented IR film-holder mask that feeds the existing RGB rebate/base search,
gated on an explicit `--film-type` declaration.

**The `--film-type (silver | chromogenic | unknown)` knob (I own it).** A new
shared input-medium declaration, wired across all four coupled spots:
- CLI `*Overrides`: `InputOverrides.film_type: Option<FilmType>` (`--film-type`
  on `convert`); the flag is also added to `inspect` (`IoArgs`) and `estimate`
  (`EstimateArgs`) so those commands' auto-base/inspect paths can use IR too.
- Recipe `*Params`: `InputParams.film_type: FilmType` (recipe key
  `input.film_type`, design-spec §9 Input/decode section — it describes the input
  medium, parallel to `input.transfer`/`input.meaning`; deliberately **not** under
  `film_base`, because `bw-support` IR dust removal will reuse the same key).
- `merge` arm: flags-win over the recipe; absence never clobbers a recipe value.
- `validate`: nothing to check — the enum has no invalid states; the gate is a
  runtime `ir_transparent()` + IR-plane check, plus a `convert` warning when
  `chromogenic` is declared on a scan with no IR plane.
- Modeled as **one enum** (`FilmType`), not parallel bools. Default `Unknown`
  (off). The gate is `FilmType::ir_transparent()` (true only for `Chromogenic`) —
  silver blocks IR (dense silver → dark → misreads as holder) and unknown is the
  safe default off. HDR 48-bit (no IR plane) always falls back to RGB-only.
  A `// bw-support reuses this` note is on `FilmType` / `InputParams.film_type`.

**Segmented-threshold mask (`pipeline/film_base.rs`).** `ir_holder_mask(image,
film_type) -> Option<Vec<EdgeHolderMask>>` returns `Some` only for chromogenic +
an IR plane. Per edge, the along-edge extent is split into `IR_HOLDER_SEGMENTS`
(24) segments; each segment's **shallow** near-edge probe band (depth
`IR_HOLDER_PROBE_FRAC` = 0.5% of the short dim, floored at 2px) is reduced to its
median IR and classified holder (≤ `IR_HOLDER_MAX_TRANSMISSION` = 0.1) or film.
Segmenting *along* the edge (not one per-edge mean) lets a partially-covered edge
split into holder vs film runs; a whole-edge label is the degenerate
all-segments-agree case. `rebate_candidates(image, film_type)` then runs the
existing inward-scan **once per contiguous film run** (holder runs excluded), so
holder pixels never enter the rebate search; with no mask it is the single
full-extent scan per edge, byte-identical to before. `estimate` takes `film_type`
and threads it through the `Auto` branch.

**Probe-depth finding (important).** The classifier's probe depth was the one
real design decision. The rebate *scan* window is ~10% of the short dim (≈342px on
a 4666×3423 scan); probing that deep for the holder washes a real holder band out
with the bright film sitting behind it (the whole-window median reads film). A
**shallow** near-edge probe (~0.5%) is correct: the opaque holder occludes from
the very edge inward, so its darkness lives in a shallow band. Verified on real
scans (below).

**Verification (real scans, via a throwaway `#[ignore]` test — now removed;
derived numbers only, never pixels).**
- IR cleanly separates holder from film on both HDRi scans: holder/occluded
  segments read IR median ≈ 0.019–0.084, film segments ≈ 0.65–0.67 — a ~10–25×
  gap, so the 0.1 threshold sits with wide margin on both sides (matches the
  task's Phoenix 0.023 / Ektar 0.587 data).
- **Phoenix `933`** (4666×3423, role=unexposed): Top 25/25 holder (IR
  0.019–0.025), Bottom 1/25 (film, 0.073–0.666), Left 1/25 (film, 0.036–0.673),
  Right 25/25 holder (0.027–0.084) → **top & right = holder, bottom & left =
  film**, matching the task's expected Phoenix pattern (the right edge reads
  full-holder near the edge on this unexposed frame rather than a partial split;
  the partial-split *mechanism* is covered by the synthetic
  `ir_mask_recovers_the_rebate_on_a_partially_occluded_edge` test).
- **Ektar `1009`** (5184×3599, role=leader): all four edges read holder near the
  edge (IR 0.018–0.079). This **differs from the task's "classified all-film"
  expectation** — the real leader frame is genuinely held in a holder on all
  edges (near-edge IR ≈ 0.02); the bright fully-exposed film is the interior. Any
  correct opacity-based detector reads those edges as holder. The task's
  "all-film" most likely described the exposed picture area, not the frame edges.
- Neither frame yields rebate candidates (rgb-only and chromogenic both empty) —
  expected: both are *calibration* frames (unexposed / leader) without the
  `holder → rebate → picture` layered structure the rebate detector needs; you'd
  use `--base-region`/`--grid` on them, not auto rebate detection. The
  rebate-recovery integration is exercised by the synthetic partial-edge test.

**Notes for `bw-support`.** (1) `input.film_type` / `FilmType` is the shared knob
the IR dust-removal guard should key on (film type, not `color_model = mono`) —
already named consumer-agnostically. (2) The IR holder mask's **second consumer**
— auto-`Dmax` border exclusion (PR #21 finding 4: the dark holder/dust border can
capture the 99.5th-percentile anchor and dim the render) — is left as follow-up;
`ir_holder_mask` / `EdgeHolderMask` are `pub` and ready to reuse for excluding
holder pixels from the anchor statistics.

**Inspect surface.** `nc inspect --film-type chromogenic` on an HDRi scan now
reports `holder_mask` (per-edge segments with span/class/IR) in the JSON report.

Left uncommitted; TASKS.md checkbox not flipped.

### 2026-07-24 — Review-fix pass (uncommitted)

Addressed the `ir-holder-detection` review round (docs/comments/warnings/tests
only; the core mask logic was untouched). **The Codex P1** claiming the IR mask
excludes the spans the rebate detector needs was reviewed and **rejected as a
false positive**: `edge_candidate`'s depth-0 "holder run" is a dark *RGB* band
(can be dense film, IR-bright), while the IR mask excludes only *IR-dark* (opaque
carrier) spans — `ir_mask_recovers_the_rebate_on_a_partially_occluded_edge` proves
IR *recovers* a rebate RGB-only misses; no logic changed.

- **Warning scoping.** The chromogenic-without-IR note now fires only when the
  auto base detector actually runs (`FilmBaseSource::Auto`), so a valid
  `convert --strict --film-type chromogenic --film-base …` on a non-HDRi scan no
  longer fails on a path it never takes. The pre-existing "IR preserved but not
  used in Step 1" note is suppressed when the chromogenic + IR + auto-base path
  consumes IR for the holder mask (the claim would be false there). Matching
  chromogenic-on-no-IR notes added to `run_estimate` (auto path) and `run_inspect`
  for consistency.
- **Docs.** design-spec §6.1 now records the chromogenic film-base IR consumer
  (stage 2), and §9 Input/decode documents `--film-type` / `input.film_type` and
  the `inspect` `holder_mask` report field.
- **Comment accuracy.** Fixed the `bw-support` mischaracterization (it is the B&W /
  mono task, roadmap item 3 — *not* the IR dust-removal task, which is item 1;
  both reuse the shared `FilmType` axis) in `types.rs` / `cli.rs`; corrected the
  `edge_holder_segments` trailing-segment comment (smaller leftover, not an
  absorbed-larger one), the fully-occluded test's probe-depth comment (probe = 2 px,
  not scan_depth 10), and the all-film test's Ektar `1009` framing (synthetic
  uniformly-IR-bright film; the real leader's edges genuinely read holder).
- **Tests.** Added five synthetic-fixture tests: threshold boundary pinned to
  [0.09, 0.11) around `IR_HOLDER_MAX_TRANSMISSION`; thin (2 px) holder over bright
  film to pin the shallow probe; a film→holder→film bottom edge covering the
  >1-film-run / mid-loop-flush / >1-candidate-per-edge paths; an all-holder frame
  driving the loud empty-candidates error; and a `HolderClass`/`HolderSegment`/
  `EdgeHolderMask` lowercase-serde guard.

### 2026-07-26 — PR #56 Codex review-fix pass (uncommitted)

Four real findings on PR #56, plus one document-only deferral.

- **[P1] IR provenance gate.** The holder mask consumed any populated `image.ir`,
  but the decoder accepts a same-dimension 16-bit grayscale IFD as IR by **shape
  alone** when the `NewSubfileType=4` marker is absent (it only warned). Safe when
  IR was merely carried; unsafe now that stage 2 consumes it. Threaded the
  provenance: new `LinearImage::ir_verified` (set by `io::decode` from the marker,
  `false` for a shape-only plane), and `ir_holder_mask` now returns `None` unless
  the plane is verified → RGB-only fallback. Orchestrators (`convert`/`estimate`/
  `inspect`) emit a `--strict`-promotable warning for the shape-only+chromogenic
  case. Tests: `ir_holder_mask_requires_a_marker_verified_ir_plane` (shape-only →
  None, verified → built) + decode-boundary assertions on both provenance states.
- **[P2] Same-edge disagreement.** `select_auto_base` filtered other candidates by
  `other.edge != best.edge`; since one edge can now yield multiple candidates
  (multiple film runs), two differing bases from the same edge were silently
  ignored. Now excludes only `best` itself by pointer identity, so same-edge
  disagreement surfaces (and `--strict` can reject it). Test:
  `auto_warns_on_two_disagreeing_runs_on_one_edge`.
- **[P2] Inspect IR-note contradiction.** `run_inspect` fired the unconditional "IR
  carried but not used" note even under `--film-type chromogenic` on an HDRi scan
  where it now builds+uses the mask. Gated the same way `convert_frame` does
  (no-IR / shape-only / consumed cases each get the right note or none).
- **[P2] Inspect best-effort mask.** `ir_holder_mask(...)?` made `inspect` abort on
  a too-small chromogenic image; now caught and reported as a diagnostic warning,
  matching the candidate search, so `inspect` stays informational.
- **[P1 — document-only, deferred by user] Shallow-holder rebate exclusion.** Added
  a "Known limitation" note to the `film_base.rs` module doc (and here): a thin
  opaque holder margin IR-dark only in the shallow near-edge probe, with a rebate
  directly behind it, is excluded by the along-edge mask, so auto-base can miss a
  rebate RGB-only would find. Deliberate, accepted trade-off (the shallow probe is
  what separates a thin holder from bright film); failure is bounded (loud refusal
  or a correctable global cast, never a crossover — §8); workaround is
  `--base-region`/`--film-base`; roadmap fix is depth-aware occlusion (exclude a
  span only if IR-dark through the full scan depth). Mask logic, `film_along_ranges`,
  `median_ir_probe`, probe depth, and the `shallow_probe_…` test were left
  unchanged per the user's decision.

---


## auto-base-neutral-stock

**Status:** not started
**Updated:** —

- Goal: Harden auto film-base detection for film stocks whose base is **near-neutral** (e.g. Harman Phoenix, R/B ≈ 0.84) rather than orange — bright but not color-distinctive, so confidence signals keyed on base color can mis-anchor on bright neutral scene content.


## dense-base-dmax-plausibility

**Status:** not started
**Updated:** —

- Goal: Stop `nc estimate` from emitting spurious plausibility warnings on legitimately dense- / atypical-base film stocks.


## content-fallback

**Status:** not started
**Updated:** —

- Goal: Add an explicit, opt-in film-base source that estimates `Dmin` from the exposed image **content** when no unexposed film (dedicated frame, rebate, or holder-inset band) is available to sample — the design-spec §9 acquisition-ladder **Tier 3**.


## grid-verdict-enum

**Status:** not started
**Updated:** —

- Goal: Replace `GridEstimate.agreement: bool` (plus the overloaded `spread` sentinel) with a self-describing verdict enum, so `nc estimate --grid` reports *which* of the mutually-exclusive grid outcomes occurred and the CLI stops re-deriving it from the combined base.
