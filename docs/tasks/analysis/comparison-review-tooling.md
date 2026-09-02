# Comparison Review Tooling

## Goal

Promote the ad-hoc review pages built during `algo/reference-anchored-sigmoid` into a
maintained tool for comparing rendering configurations by eye. It was requested explicitly:
"we should keep this tool, reuse it, improve it… I think we'll continue to compare different
configure for a while."

## Design

**What exists** (in `scripts/sigmoid-baseline/`, grown incrementally and therefore uneven):

- `patch-review.sh` + `build_patch_review.py` — frames with candidate patch rectangles drawn
  on them, magnified crops via CSS `background-position`, and an exposure-variant row.
- `candidate-review.sh` + `build_candidate_review.py` — one tile per rendering configuration,
  with a shared click lightbox and prev/next.

**Lessons already paid for, which the tool should keep:**

- **Render through the path being measured.** Previews originally used the *legacy* path while
  metrics measured `pipeline::sdr::render` — reviewing one renderer while measuring another.
  They now use `--output-preset ultra-hdr-v1`, whose JPEG base *is* the SDR rendition.
- **Click, not hover.** A hover popover covers the thumbnail you are trying to leave, and the
  gaps between thumbnails make it flicker.
- **One shared lightbox**, which is what makes prev/next possible at all.
- **Single-quote CSS `url()`** inside a double-quoted `style` attribute, or the attribute
  terminates and the element silently renders black.
- **Never publish these pages.** They are rendered personal photographs; output goes to a
  throwaway directory, never `../nc-assets` (a Drive symlink) and never the repo.
- **`sips` cannot downscale a gain-map JPEG** without destroying the gain map, so HDR review
  needs full-size files.

**What it needs to become a tool rather than a script pair:** one entry point instead of two
overlapping ones; a configuration matrix described as data rather than edited in code; HDR
review (full-size gain-map files for the frames whose range exceeds SDR); and the ability to
compare *builds* as well as configurations, so a future default change can be reviewed the
same way.

## What has shipped

The **viewer** half landed 2026-09-02 as `tools/review-app/` (Vite+ / Solid / StyleX, its own
CI job). It reads a `review.json` — the format is `tools/review-app/SCHEMA.md` — and renders
every configuration of a frame into one grid cell, so switching between them cannot move the
picture; `fullsize` has pan controls and a mini-map. That settles "the matrix as data" from the
viewing side and gives the rest of this task a contract to target.

**Still open, and the reason this task is not done:** the **generator** that renders a described
matrix and emits the JSON (today images and `review.json` are produced ad hoc), HDR review for
frames whose range exceeds SDR, and build-vs-build comparison. The lessons in *Design* above still apply to
the generator — especially *render through the path being measured*, which is now its
obligation, and the `sips`-destroys-a-gain-map constraint that gates HDR review.

## Implementation Suggestion

- Keep the derived-numbers discipline: the page may *display* images for a human, but no tool
  here reads sample pixels into an agent context.
- The anchor/parameter rules already live in one place per script so renders and captions
  cannot disagree — preserve that property when unifying.
- Consider driving it from the same fixture declaration
  (`scripts/sigmoid-baseline/fixtures.json`) the metrics use, so the two cannot drift.

## How to Verify

- One documented entry point renders a described matrix and emits a page, with no code edit
  needed to change the matrix.
- Regenerating twice from the same inputs produces the same page.
- Works with assets absent (clear skip) and does not write outside its output directory.
- HDR review is possible for at least the frames whose scene range exceeds SDR.

## Dependencies

- [Reference-anchored sigmoid calibration and redesign](../algo/reference-anchored-sigmoid.md)
