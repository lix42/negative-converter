---
name: update-usingnc-doc
description: >-
  Bring docs/using-nc.md back in step with the binary after a change to nc's
  user-visible surface — a flag, subcommand, default, recipe key, output preset,
  exit code, or a report field or error message a user acts on. Use when finishing
  such a change, when asked to "update the usage guide", "check using-nc", or
  "does the guide still match", and after rebasing onto work that shipped any of
  those. Verifies every claim by running nc, never by reading the diff.
---

# Update the usage guide

`docs/using-nc.md` is the user-facing guide, and its contract is that **every
claim in it was verified by running the binary**. That contract is the whole
value: the design spec describes intent, and the guide describes what the CLI
actually accepts today. Editing it from a changelog breaks the one thing it is
for.

## When this applies

Any change to what a user can type or read back:

- a flag added, removed, renamed, or given different values
- a **default** changed — these have been the most damaging
- a subcommand added, renamed, or removed
- a recipe key, or the shape of the resolved recipe
- an output preset, its container, or its suffix rule
- an exit code, a report field, or an error message a user acts on

Internal refactors that leave all of the above identical are exempt. So is a
**speculative or unimplemented target** — a design recorded in design-spec's target
subsection is not current behaviour and must not be documented as if it were.

What is *not* exempt is a user-visible change still on a feature branch: the guide
update belongs in that same PR, so `main` never gains the change without the
documentation for it. Describe the behaviour the branch actually ships, verified
against the binary built from it.

## Steps

**1. Build at the change.** `cargo build`. Every check below runs the binary you
just built, not the one in another worktree — a stale binary from a sibling
checkout has produced wrong "verification" before.

**2. Re-read the header pin.** The guide's header records `nc --version` and a
commit. If they disagree with `nc --version` now, treat the whole document as
suspect rather than only the section you touched.

**3. Re-run the examples in every section your change could reach.** Not just the
prose — *the commands*. Each time this guide has gone stale, re-verification
found **two or three of its own examples had broken** in ways the changelog never
mentioned, because an example composes several behaviours and any of them can
move. Worked cases:

- a default curve change silently broke the "minimal recipe is byte-identical"
  claim, because a flag form and a recipe form stopped resolving to the same curve
- a default preset change made `-o out.tiff` fail outright, invalidating every
  example in the workflow section
- a render-path change moved a quoted `--auto-wb` output value twice

**4. Check these specifically** — they have each gone stale at least once:

| | How to check |
|---|---|
| the default recipe in §5 | `nc params` |
| accepted preset names and suffixes | try each; the help text has been wrong when the parser was right |
| exit codes | provoke each one; do not copy them from the spec |
| quoted report values | re-run the command that produced them |
| "nothing is silently ignored" claims | provoke the error and paste what it says |

**5. Refresh the header pin** — `pipeline_version` and commit — so the next reader
can tell at a glance whether the guide is current.

**6. Prefer deleting to hedging.** If a section documents something that has
become provisional, say so plainly with a pointer to the owning task, the way §8
does. If it documents something that no longer exists, remove it.

## Traps

- **`--help` is not authoritative.** It has shipped wrong twice while the parser
  was right (preset names, `--d-max`'s description). Verify behaviour, then fix
  the help text as a separate change if it disagrees.
- **Do not cite unmerged files.** A reference to a doc that exists only on a
  branch is a dangling link on `main`; a reviewer has already caught this once.
- **Do not document a target as if it shipped.** `design-spec.md` §8 carries a
  target subsection; the guide carries only current behaviour.
- **Keep it short.** Per `CLAUDE.md`, write what will still matter in six months.
  A question answered in one conversation is not automatically a section.

## Finishing

The guide's changes belong in the **same PR** as the change that caused them, not
a follow-up. Report which sections you re-verified and which examples you had to
correct — a correction found here is the signal the process worked.
