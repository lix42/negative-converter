---
name: review-fix-loop
description: >-
  Review, fix, and converge on a feature worktree's uncommitted changes before a
  PR. Use when implementation is complete and needs independent review
  passes, verified findings, one coordinated fixer, targeted re-review, and
  green Rust quality gates while leaving all changes uncommitted for the user's
  manual review. Invoke as `$review-fix-loop [worktree-path-or-task]`.
---

# Review / fix loop

Harden a completed feature worktree with independent review contexts, verified
findings, and a single writer. Run two reviewer subagents in parallel while the
coordinating agent performs its own review. Consolidate the three passes, reject
false positives, route genuine findings to one fixer, and repeat until clean or
Low-only. Do not commit, push, open a PR, or merge.

This is the tool-agnostic variant, for runtimes that scan `.agents/skills`
(e.g. Codex CLI). The Claude Code variant lives at `.claude/skills/review-fix-loop/`
as a real directory and intentionally diverges — do not replace either with a
symlink to the other.

## Inputs

- Resolve a supplied path or task name to a real checkout using
  `git worktree list --porcelain`. Never confuse it with Git's internal
  `.git/worktrees/<name>` metadata directory.
- Determine the intended base branch and inspect ahead/behind state. Do not
  fetch, rebase, commit, or alter the index unless the user explicitly included
  that operation in scope. Report stale-base uncertainty instead of silently
  changing history.
- Require uncommitted implementation changes. If the work is already committed,
  stop and ask whether to review the commit/branch instead.

## Step 1 — Scope and frame the change

Run from the target worktree:

```bash
git status --short --untracked-files=all
git diff --stat HEAD
git diff HEAD
```

Treat `git status` as authoritative: `git diff HEAD` omits untracked files. Read
every untracked file directly; do not use `git add -N` because that changes the
index. Read applicable `AGENTS.md` files and, when present, the repository's
`CLAUDE.md` for project-specific conventions.

Write a short shared frame describing the intended behavior, changed files,
new types or CLI knobs, error paths, tests, and documentation impact.

## Step 2 — Run three independent review passes

Spawn two named, read-only reviewer subagents in parallel with minimal forked
conversation context. Give both the worktree path, shared frame, complete status
list including untracked files, applicable instruction files, and these rules:
report only actionable findings by severity with `file:line` evidence and a
concrete failure scenario; do not edit files.

- **Correctness reviewer:** inspect behavior, edge cases, invariants, error
  propagation, unsafe assumptions, concurrency, and compatibility.
- **Tests and maintainability reviewer:** inspect missing regression coverage,
  weak assertions, API/type design, documentation accuracy, needless complexity,
  and project-convention compliance.

While they run, independently review the entire change as the coordinating
agent. Focus extra attention according to the diff: silent failures for fallback
or warning paths, type invariants for new types, and synchronization for shared
state. Do not send reviewers one another's findings before they finish.

## Step 3 — Aggregate and VERIFY

Consolidate all passes into one severity-ranked list and deduplicate overlaps.
Verify every finding against the code before forwarding it. Confirm a concrete
failure or maintenance cost; reject false positives with a brief reason. A
reviewer's confidence or severity label is not evidence.

## Step 4 — Route real findings to ONE fix agent

Spawn one named fixer subagent, or resume the same fixer in later rounds. Never
let multiple agents edit concurrently. Give it the verified, itemized findings
and these constraints:

- Keep everything **uncommitted** (the user reviews before any commit/PR).
- Finish with **all four CI gates green**, in order:
  `cargo fmt --all --check` → `cargo clippy --all-targets -- -D warnings` →
  `cargo build` → `cargo test`.
- Edit `docs/design-spec.md` and `docs/design-spec.html` **together**.
- Respect four-coupled-spots for any knob (CLI field + `*Params` + merge arm +
  validate + a merge test).
- Preserve unrelated user changes and report a per-item summary plus verbatim
  gate results.

## Step 5 — Converge

- Inspect the fixer's diff yourself before accepting it.
- For correctness changes, send the delta to both original reviewers for a
  targeted read-only re-review. Resume them by name so they retain their lens.
- For test/doc/comment-only changes, run a focused coordinating-agent review;
  involve the relevant reviewer when the change is non-trivial.
- Send newly verified findings back to the same fixer and repeat.
- Stop when the round is **clean or LOW-only**. Declare the loop **converged**.
- If a bounded finder was capped (top-N, sampling), say so — never present a
  capped pass as exhaustive.

## Step 6 — Report

Give the user the verified findings, rejected false positives and reasons, fixer
changes, final gate results, and remaining Low-only observations. State plainly
that the worktree remains uncommitted for manual review. Do not open a PR or
merge.

## Invariants (do not break)

- Reviewer subagents never modify files; only the designated fixer does.
- **Verify before forwarding** — a rejected false-positive is a good outcome.
- Everything stays **uncommitted**; the user does the final review and merge.
- Use independent contexts: two reviewer subagents plus the coordinating review.
- Name every subagent so later rounds can resume it with context.
