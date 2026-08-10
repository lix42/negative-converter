# Derive the output suffix from the resolved preset

## Goal

Let `-o` name the output without knowing the container. `-o out` should write
`out.tiff` or `out.jpg` or `out.avif` according to the resolved preset. An
explicitly given suffix is still checked: if it matches the preset, honour it —
including the user's spelling, so `-o out.jpeg` stays `.jpeg` — and if it does
not, fail as it does today.

## Why now

Every preset now pins a required suffix (completed 2026-08-09; `legacy` and
`film-master` state one too). That closed a real hole — `nc convert -o out.jpg`
used to write a TIFF named `.jpg` — but it left the user responsible for knowing
which container a preset resolves to, which is exactly the knowledge the preset
exists to encapsulate. The check should stay; supplying the suffix should become
optional.

## What is known

- `cli::required_extensions` already lists the accepted spellings per preset, and
  the mismatch error already names them. This task changes what happens when the
  suffix is **absent**, and which spelling wins when it is present.
- **`roll` already derives suffixes, and answered question 1 for its own names**
  (`output/presets`, 2026-08-09). `cli::derived_extension` picks one canonical
  spelling per container — `tiff`, `jpg`, `avif` — kept deliberately separate from
  `required_extensions`, whose head is `tif` and would have renamed every existing
  `_positive.tiff`. A test asserts the derived spelling is always a member of the
  accepted set. Reuse it rather than deciding the spellings again; its doc frames
  it as roll-only, which is a framing to widen, not a constraint.
- There is **no `roll` refusal left** to disturb: every preset is roll-capable
  since the same change, so suffix handling and roll's gate are fully decoupled.
- The **default is now `gain-map-hdr`**, so a bare `-o out` derives `out.jpg`, not
  `out.tiff`. Extensionless paths are currently rejected outright (exit 2) — that
  rejection is precisely what this task proposes to replace, so re-read
  `reject_suffix_mismatch`'s `SuffixContext::Default` arm before changing it.

## Open questions

1. **Which spelling is canonical when deriving?** `.tif` or `.tiff`; `.jpg` or
   `.jpeg`. One choice per container, and it becomes visible in every report.
2. **When is a trailing dot-segment a suffix?** `-o out.v2` and `-o roll-1.2` are
   plausible stems, not containers. Decide the rule — probably "known suffix for
   *some* preset ⇒ treat as a suffix and validate; otherwise treat the whole thing
   as a stem" — and decide what a suffix belonging to a *different* preset means
   (almost certainly the existing mismatch error, not a silent stem).
3. **Does this refine `output/presets`' "the output path … is never silently
   renamed"?** Completing an absent suffix is arguably not renaming, but that
   sentence is the governing statement and lives in a task still in progress.
   Settle the wording with that task rather than around it.
4. **Should `roll` use the same derivation?** It builds `<stem>_positive.tiff`
   today, and container-aware roll naming is explicitly `output/presets`' scope.
   Likely: share one helper, let presets own when roll adopts it.
5. **Does the report gain the resolved path?** An agent piping JSON currently
   knows the output path because it passed it. Once nc completes it, the report
   probably has to say what was actually written.
6. **Sidecar and staged-write ordering.** Both derive from the final image path;
   confirm they see the completed one, not the stem.

## How to Verify

The shape of the evidence, not an exhaustive list:

- A bare stem under each preset writes the expected container, and the report and
  sidecar name that same final path.
- An explicit matching suffix is preserved verbatim, including the non-canonical
  spelling.
- A mismatched suffix still fails with the existing message — this task must not
  weaken the check it builds on.
- Whatever rule question 2 settles on has a test pinning a stem that contains a
  dot.

## Dependencies

- [HDR AVIF output](hdr-avif-output.md) — introduced `cli::required_extensions`,
  the table this task changes.

Coordinate with [Output presets and guidance](presets.md), which is in progress
and owns both the "never silently renamed" statement (question 3) and
container-aware `roll` naming (question 4). Not declared a dependency: the table
this task needs has already shipped, and blocking on the full preset migration
would stall a change that stands alone for `convert`.
