---
name: review-fix-loop
description: >-
  Run the two-engine review → fix → converge loop on a feature worktree's
  uncommitted changes. Use when a worktree's implementation is done and needs
  review before a PR: run two independent reviewers (Codex + an in-session
  review), aggregate and verify their findings, route the real ones to a single
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

This is the Claude Code variant, built on the Codex plugin plus an **in-session
review you perform yourself**. A tool-agnostic variant lives at
`.agents/skills/review-fix-loop/` and intentionally diverges — do not symlink
them together.

**Why two engines.** A different model reviewing the same diff catches a
different class of bug. In practice Codex has caught issues the in-session
review missed (a `+inf` non-finite laundering, a JSONL atomic-append race, a
case-only path collision), and in-session builds have disproved a Codex
false-positive P0. Running both, and verifying before acting, is the point — do
not drop one. The engines are also asymmetric in a useful way: Codex sees the
diff cold with no conversation history, while the in-session review has the
session's context — so it knows *why* the code is shaped the way it is, and is
correspondingly better at spotting a change that contradicts an intent it has
seen and worse at noticing an assumption it has already absorbed. That is the
blind spot Codex covers. "A reviewer that has the session context" is the
second engine's whole job description, and you are one.

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

## Step 2 — Run both engines (review-only)

Start Codex **first** as a background job, then do the in-session review
yourself while it works — that keeps the two overlapping instead of serialized.

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

**In-session review** (second engine) — **you** review the working diff, with
this session's context. Nothing to install and nothing to invoke.

**Do not try to call `/code-review`.** It is marked
`disable-model-invocation`: an agent cannot run it through the Skill tool and
will just get `Skill code-review cannot be used with Skill tool due to
disable-model-invocation`. Only the *user* can type it. If the user wants the
built-in specifically, they type `/code-review` themselves and paste (or leave
in-session) its findings for Step 3 to aggregate — treat that as a bonus third
input, not a prerequisite. The loop must run without it.

Doing the review yourself, hold to the same bar a dedicated reviewer would:

- **Cover the untracked files.** Like `git diff HEAD`, a working-diff review
  drifts toward the modified files — and on a feature branch the new module *is*
  the change. Walk every `??` path from Step 1 explicitly.
- **Read the worktree's `CLAUDE.md` first**, and review against the Step 1
  framing. Go slower and deeper on a dense or safety-critical diff.
- **Report by severity with file:line**, and mark anything you could not settle
  as a *lead*, not a finding. A lead goes to Step 3 for verification and never
  straight to the fix agent. (User-supplied `/code-review` output uses
  `CONFIRMED` / `PLAUSIBLE` for the same distinction — `PLAUSIBLE` is a lead.)

**Optional built-in adjacents** — these *are* agent-invocable. Use one only when
the diff clearly warrants it, and still review-only:

| Command | Run when |
|---|---|
| `/security-review` | the change touches input parsing, file writes, or anything attacker-reachable |
| `/simplify` | optional final polish, *after* the loop is otherwise clean — it does quality only and explicitly does not hunt bugs, so it never substitutes for the review pass |

**Standing rule for every reviewer**, whichever engine: scope is **all
uncommitted changes, no GitHub PR** — `git diff HEAD` *plus* the untracked files
from `git status --short`; findings reported **by severity with file:line**; and
**do NOT modify any files — review only.** Only the fix agent edits.

## Step 3 — Aggregate and VERIFY

When both engines have reported — your in-session review written up, Codex's
background job collected — consolidate into one severity-ranked list (Critical /
High / Important / Medium / Low), deduping overlaps. Note where the two engines
agree: independent convergence on the same line is the strongest signal
available here.

**Verify each non-trivial finding against the actual code before acting.**
Reviewers produce false positives (a Codex "won't compile" P0 was wrong — a
`Copy` field destructure, not a move). Read the cited lines; if a finding is
wrong, reject it and say why — do not forward it to the fix agent. Confirm real
ones with a concrete failure scenario (inputs → wrong output).

This step is not optional for your own findings either — self-verification comes
from the same engine that raised them, and the same goes for a `CONFIRMED`
verdict in user-supplied `/code-review` output. Cross-engine verification is
what this step buys.

## Step 4 — Route real findings to ONE fix agent

Spawn (or `SendMessage`-resume) a single named fix agent — never have the
reviewers fix their own findings. Hand it a precise, itemized set and these
standing constraints:

- Keep everything **uncommitted** (the user reviews before any commit/PR).
- Finish with **all four CI gates green**, in order:
  `cargo fmt --all --check` → `cargo clippy --all-targets -- -D warnings` →
  `cargo build` → `cargo test`.
- `docs/design-spec.md` is the **sole maintained design source** — edit it there.
  The rendered HTML companion is retired and may be regenerated after the feature
  roadmap stabilizes; do not recreate or hand-edit it.
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

- Reviewers **never** modify files; only the fix agent does. That includes the
  in-session pass: review, write up, hand off — **do not fix as you read.** Fixes
  go through the single fix agent in Step 4, so one actor owns the tree and the
  gate run.
- **Verify before forwarding** — a rejected false-positive is a good outcome.
- Everything stays **uncommitted**; the user does the final review and merge.
- Two engines, always — Codex *and* the in-session review — because they miss
  different things, and because one of them has this session's context and the
  other deliberately does not.
- Name every agent you spawn so later rounds resume it with context
  (`SendMessage` by name). The in-session review has no name to resume; redoing
  it re-reviews the current tree, which is what a later round wants anyway.
