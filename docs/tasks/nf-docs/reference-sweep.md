# Re-point references to retired and superseded tasks

## Goal

Nine tasks were superseded and seven retired on 2026-09-19. Their files stay so
links resolve, but code comments and docs still cite them as live owners of
decisions. Re-point each such pointer at the task that owns the decision now, or
drop it.

## Design

- **The target is a claim of liveness or ownership, not a mention.**
  `src/algo/density.rs` names `film-base/dmax-anchor-reliability` as owning
  `NOMINAL_DMAX`; `src/pipeline/film_base.rs` names `film-base/white-holder-support`
  for a load-bearing IR premise. Both send a reader to an open task that is closed. A
  sentence reading as *history* — "decided under X on <date>" — is fine and is left
  alone; rewriting history is the opposite of the point.
- **Scale.** A grep over the retired and superseded ids finds roughly eight `src/**`
  doc comments, three `docs/design-spec.md` pointers and one in `docs/using-nc.md`,
  across `types.rs`, `cli.rs`, `film_base.rs`, `stages.rs`, `shadow_metrics.rs`,
  `density.rs` and `film_stock/`. The grep finds candidates; the judgement is
  per site.
- **Only one kind of rot is loud.** A changed *stem* stops resolving; a task that
  merely gained an `nf-*` prefix still substring-matches, so the old id keeps hitting
  and looks healthy. CLAUDE.md's rule against bulk-rewriting ids applies —
  `color-management` is also ordinary English for ICC work.
- **A pointer may have no successor.** Where a retirement removed the question
  rather than moving it, the comment loses the pointer and keeps the fact. Do not
  invent an owner to fill the slot.
- **No gate reads prose**, so this whole class is invisible to `fmt`, `clippy`,
  `build` and `test`. It is cheap to do early and goes stale again with every
  retirement, so expect a second pass once `nf-retire` has landed.

## How to Verify

- Every backticked task id in `src/`, `docs/design-spec.md` and `docs/using-nc.md`
  resolves to a file, and no surviving pointer names a superseded or retired task as
  an open owner.
- `cargo doc --no-deps 2>&1 | grep "unresolved link"` adds nothing against the
  16-link baseline.
- The four CI gates pass.

## Dependencies

None — can run at any point.
