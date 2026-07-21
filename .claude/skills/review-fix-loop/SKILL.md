---
name: review-fix-loop
description: >-
  Run the two-engine review → fix → converge loop on a feature worktree's
  uncommitted changes. Use when a worktree's implementation is done and needs
  review before a PR: fan out independent reviewers (Codex + pr-review-toolkit
  lenses), aggregate and verify their findings, route the real ones to a single
  fix agent, and re-run until clean — leaving everything uncommitted for the
  user's manual review. Invoke as `/review-fix-loop <worktree-path-or-task>`.
---

# Review / fix loop

Our house process for hardening a feature worktree before it ships. Two
**independent** review engines run in parallel over the same uncommitted diff,
their findings are consolidated and **verified against the code** (not trusted
blind), genuine issues go to **one** fix agent, and the loop repeats until the
review is clean or LOW-only. Nothing is committed — the user does the final
manual review and merge.

This is the Claude Code variant, built on the Codex plugin plus the
pr-review-toolkit engines. A tool-agnostic variant lives at
`.agents/skills/review-fix-loop/` and intentionally diverges — do not symlink
them together.

**Why two engines.** Codex and the pr-review-toolkit lenses catch different
classes of bug. In practice Codex has caught issues every pr-review lens missed
(a `+inf` non-finite laundering, a JSONL atomic-append race, a case-only path
collision), and the pr-review builds have disproved a Codex false-positive P0.
Running both, and verifying before acting, is the point — do not drop one.

## Inputs

- **Which worktree.** A checkout path (a sibling like `../<name>`, or an agent
  worktree under `.claude/worktrees/agent-…`) or a task name to resolve to one.
  Get the real checkout path from `git worktree list` — not `.git/worktrees/<name>`,
  which is Git's internal metadata dir, not the working tree — then grep the
  diff/task docs to identify it.
- Confirm before reviewing: the worktree is **rebased onto current
  `origin/main`**, its **CI gates are green**, and the changes are
  **uncommitted** (`git status` = modified/untracked, no commits ahead). If the
  base lags, rebase first (commit-WIP method — see CLAUDE.md / progress notes).

## Step 1 — Scope and frame the change

From inside the worktree:

```
git status --short            # AUTHORITATIVE change list — includes untracked (??) files
git diff --stat HEAD          # sizes for tracked (modified) changes
```

**`git diff HEAD` omits untracked files** — a brand-new module or test file (the
whole point of a feature) shows only as `??` in `git status`, never in the diff.
Scope from `git status --short`, and enumerate every untracked file so a new file
can't slip through unreviewed. (If you prefer diff-based framing, `git add -N .`
makes new files appear in `git diff HEAD` as intent-to-add — no commit — but then
`git reset` afterward to leave the tree exactly as found.)

Read the worktree's `CLAUDE.md` (conventions differ per branch after rebases).
Write a 2–3 sentence framing of *what the change does* — you will paste it into
every reviewer prompt so they share context. Note new files/modules, new types,
new CLI knobs (four-coupled-spots), new error paths, and which docs changed.

## Step 2 — Launch reviewers in parallel (all background, all review-only)

**Codex** (independent engine) — run from *inside* the worktree so it reviews the
right git state. The portable way is the plugin **command** `/codex:review`,
which resolves its own plugin path; pass `--scope working-tree` to diff the
uncommitted changes vs `HEAD`. To run it as a captured background job, invoke the
companion script the command wraps — but **discover** the path, never hard-code
the version (the cache dir is `~/.claude/plugins/cache/openai-codex/codex/<ver>/`
and the plugin auto-updates):

```
# Resolve the newest installed companion script, then review the worktree.
# `command ls` bypasses any `ls` alias (eza/lsd/uutils read `-t` as "--time
# <FIELD>" and error) while still invoking the real `ls`, which handles `-1t`.
# `-t` (sort by mtime, newest first) + `head -1` picks the most-recently-installed
# version — a lexical sort would mis-rank (0.10 < 0.9). No GNU-only `sort -V`, no
# hard-coded path. `/codex:review` resolves the path itself and is preferred.
codex_mjs=$(command ls -1t ~/.claude/plugins/cache/openai-codex/codex/*/scripts/codex-companion.mjs 2>/dev/null | head -1)
node "$codex_mjs" review --wait --scope working-tree
```

Launch it with `run_in_background: true` (the `--wait` keeps it foreground
*inside* that background shell so the output is captured verbatim). If you need
custom framing/focus, use the **`/codex:adversarial-review`** command instead
(the plain `/codex:review` takes no focus text). Gotcha: if the review 400s on
the reviewer model, the Codex CLI is too old / its default model needs a switch
(see CLAUDE.md "Codex review on a worktree").

**pr-review-toolkit lenses** — spawn each as a named background `Agent` (so it's
addressable for follow-up rounds via `SendMessage`). Pick the lenses the diff
warrants:

| Lens (`subagent_type`) | Run when |
|---|---|
| `pr-review-toolkit:code-reviewer` | always (general quality + CLAUDE.md compliance) |
| `pr-review-toolkit:pr-test-analyzer` | tests changed / new behavior needs coverage |
| `pr-review-toolkit:silent-failure-hunter` | error handling / warnings / fallbacks touched |
| `pr-review-toolkit:type-design-analyzer` | new or changed types |
| `pr-review-toolkit:comment-analyzer` | docs/comments/design-spec changed |
| `pr-review-toolkit:code-simplifier` | optional final polish, after it's otherwise clean |

Every reviewer prompt must include: the worktree **path**; scope = **all
uncommitted changes, no GitHub PR** — the `git diff HEAD` *plus the untracked
files listed by `git status --short`* (name the new files explicitly so they're
reviewed, not just the modified ones); the shared change-framing; a **per-lens
focus**; "read the worktree's CLAUDE.md first"; "report findings **by severity**
with file:line"; and **"do NOT modify any files — review only."** Only the fix
agent edits.

## Step 3 — Aggregate and VERIFY

When all reviewers report, consolidate into one severity-ranked list
(Critical / High / Important / Medium / Low), deduping overlaps.

**Verify each non-trivial finding against the actual code before acting.**
Reviewers produce false positives (a Codex "won't compile" P0 was wrong — a
`Copy` field destructure, not a move). Read the cited lines; if a finding is
wrong, reject it and say why — do not forward it to the fix agent. Confirm real
ones with a concrete failure scenario (inputs → wrong output).

## Step 4 — Route real findings to ONE fix agent

Spawn (or `SendMessage`-resume) a single named fix agent — never have the
reviewers fix their own findings. Hand it a precise, itemized set and these
standing constraints:

- Keep everything **uncommitted** (the user reviews before any commit/PR).
- Finish with **all four CI gates green**, in order:
  `cargo fmt --all --check` → `cargo clippy --all-targets -- -D warnings` →
  `cargo build` → `cargo test`.
- Edit `docs/design-spec.md` and `docs/design-spec.html` **together**.
- Respect four-coupled-spots for any knob (CLI field + `*Params` + merge arm +
  validate + a merge test).
- Report back with a per-item summary and the **verbatim** final gate results.

## Step 5 — Converge

- A round with only test/doc/comment fixes (no correctness change) does **not**
  need a fresh full review round — the fix is its own evidence, plus its new test.
- A round that changed **correctness** gets a targeted re-review of the delta.
- Stop when the round is **clean or LOW-only**. Declare the loop **converged**.
- If a bounded finder was capped (top-N, sampling), say so — never present a
  capped pass as exhaustive.

## Step 6 — Report

Give the user: the consolidated aggregate (with any false-positives you rejected
and why), what the fix agent changed, and the final gate results. State plainly
that the worktree remains **uncommitted, awaiting their manual review** before PR
/ merge. Do not open a PR or merge here — that's `/ship` and the user's call.

## Invariants (do not break)

- Reviewers **never** modify files; only the fix agent does.
- **Verify before forwarding** — a rejected false-positive is a good outcome.
- Everything stays **uncommitted**; the user does the final review and merge.
- Two engines, always — Codex *and* pr-review — because they miss different things.
- Name every agent so later rounds resume it with context (`SendMessage` by name).
