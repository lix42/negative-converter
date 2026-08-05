---
name: review-fix-loop
description: >-
  Run the two-engine review → fix → converge loop on the current checkout's
  changes (defaults to the current directory; a worktree path or task name is
  optional). Use when an implementation is done and needs review before a PR:
  scope the change against the merge-base with origin/main, run two independent
  reviewers (Codex + the dedicated nc-reviewer agent), aggregate and verify their
  findings, route the real ones to the nc-fixer agent, and re-run until clean —
  adding no commits, so the user does the final manual review. Invoke as
  `/review-fix-loop` or `/review-fix-loop <worktree-path-or-task>`.
---

# Review / fix loop

Our house process for hardening a change before it ships. Two **independent**
review engines run in parallel over the same diff, their findings are
consolidated and **verified against the code** (not trusted blind), genuine
issues go to **one** fix agent, and the loop repeats until the review is clean
or LOW-only. The loop adds no commits — the user does the final manual review
and merge.

This is the Claude Code variant, built on the Codex plugin plus the dedicated
**`nc-reviewer` agent** (`.claude/agents/nc-reviewer.md`), with fixes applied by
the **`nc-fixer` agent** (`.claude/agents/nc-fixer.md`). A tool-agnostic variant
lives at `.agents/skills/review-fix-loop/` and intentionally diverges — do not
symlink them together.

**Why two engines.** A different model reviewing the same diff catches a
different class of bug. In practice Codex has caught issues the second engine
missed (a `+inf` non-finite laundering, a JSONL atomic-append race, a
case-only path collision), and local builds have disproved a Codex
false-positive P0. Running both, and verifying before acting, is the point — do
not drop one. The engines are also asymmetric in a useful way: Codex sees the
diff cold with no repo priors, while `nc-reviewer` is primed with this
project's conventions, gotchas, and doc map — plus the Step 1 framing you hand
it — so it is better at catching convention violations (four-coupled-spots
misses, determinism-breaking tests) and docs the change made stale, and worse
at questioning an assumption the project itself has baked in. That is the
blind spot Codex covers.

## Inputs

- **Which checkout — defaults to the current directory.** With no argument,
  review right here: the current project, worktree, or folder, whatever `cwd`
  is in. No worktree is required.
- **An optional argument** narrows it: a checkout path (a sibling like
  `../<name>`, or an agent worktree under `.claude/worktrees/agent-…`) or a task
  name to resolve to one. For a worktree name, get the real checkout path from
  `git worktree list` — not `.git/worktrees/<name>`, which is Git's internal
  metadata dir, not the working tree — then grep the diff/task docs to identify
  it.
- **Nothing else is a precondition.** Green CI and a fresh rebase are *nice*,
  not required — the whole point of this loop is to run before things are tidy.
  Note in the final report if the base lags `origin/main` badly or the gates were
  red going in; the fix agent runs them at the end regardless.

## Step 1 — Scope and frame the change

Everything runs from **inside the target checkout**. Scope is *this branch's
work*: whatever has diverged from the shared history, committed or not.

```
git fetch origin main                      # optional but preferred: freshens the base ref
base=$(git merge-base HEAD origin/main)    # the common ancestor to compare from
git log --oneline "$base"..HEAD            # local commits (often none)
git diff --stat "$base"                    # commits + staged + unstaged, tracked files
git status --short --untracked-files=all   # staged/unstaged/UNTRACKED — the authoritative list
git ls-files --others --exclude-standard   # untracked paths alone, script-friendly
```

Two facts that decide the whole scope, both verified:

- **`git diff "$base"` (no `..`) compares the merge-base to the *working tree*** —
  so one command covers local commits, staged, and unstaged changes together.
  `git diff "$base"..HEAD` would silently drop everything uncommitted; don't use
  the two-dot form here.
- **No diff form includes untracked files, and `--untracked-files=all` is not
  optional.** A brand-new module appears only as `??` in `git status` — and bare
  `git status --short` collapses a whole new directory to a single `newmod/`
  entry, hiding every file inside it. That is precisely the new-module case this
  check exists to catch, so always pass `--untracked-files=all` (or use
  `git ls-files --others --exclude-standard`, which lists files individually).
- **Gitignored new files are invisible to both**, by design. If the change
  deliberately adds an ignored file, `git status --ignored=matching` is the only
  way to see it; otherwise say ignored paths were out of scope.

**Don't reach for `git add -N` here.** Earlier versions of this skill staged
untracked files to force them into a diff. Step 2 explains why that is wrong for
the Codex path and can actively change what gets reviewed. If you ever do use
intent-to-add for some other reason, undo it **path-scoped** —
`git reset -- <path>`, never a bare `git reset`, which also unstages files *the
user* had staged, and in this repo staging is the user's review queue (see
`~/.claude/CLAUDE.md`).

**Resolve the base defensively — an empty `$base` fails loudly one command
later.** `git merge-base` exits non-zero when `origin/main` is absent or history
is shallow, leaving `$base` empty and turning the next command into
`fatal: ambiguous argument ''`. Walk the fallbacks in the shell, not in your head:

```
git fetch origin 2>/dev/null || true    # bare `origin`: `origin main` errors outright if main is absent
base=""
for ref in origin/main origin/HEAD main master; do
  base=$(git merge-base HEAD "$ref" 2>/dev/null) && [ -n "$base" ] && break
  base=""
done
[ -z "$base" ] && echo "NO SHARED BASE — review uncommitted work only, and say so"
```

A stale base (fetch failed, offline) still works but widens the diff with
already-merged work, which reads as confusing reverse-diffs — note it if so. Note
too that `merge-base` without `--all` returns one ancestor; with criss-cross
history that can pull in unrelated changes, though squash-merged PRs make it
unlikely here.

Then read the checkout's `CLAUDE.md` (conventions differ per branch after
rebases) and write a 2–3 sentence framing of *what the change does* — you will
paste it into every reviewer prompt so they share context. Note the resolved
base SHA, whether there are local commits, new files/modules, new types, new CLI
knobs (four-coupled-spots), new error paths, and which docs changed.

## Step 2 — Run both engines (review-only)

Start Codex as a background job and spawn `nc-reviewer` in the same breath —
both run in the background, so the two overlap instead of serializing.

**Codex** (independent engine) — run from *inside* the target checkout so it
reviews the right git state. **`/codex:review` and `/codex:adversarial-review`
are both `disable-model-invocation: true`** — the same human-only restriction
documented for `/code-review` below — so calling the companion script directly is
your *only* route, not a fallback. **Discover** the path; never hard-code the
version (the cache dir is `~/.claude/plugins/cache/openai-codex/codex/<ver>/` and
the plugin auto-updates):

```
# Resolve the newest installed companion script, then review this checkout.
# `command ls` bypasses any `ls` alias (eza/lsd/uutils read `-t` as "--time
# <FIELD>" and error) while still invoking the real `ls`, which handles `-1t`.
# `-t` (sort by mtime, newest first) + `head -1` picks the most-recently-installed
# version — a lexical sort would mis-rank (0.10 < 0.9). No GNU-only `sort -V`, no
# hard-coded path. `/codex:review` resolves the path itself and is preferred.
codex_mjs=$(command ls -1t ~/.claude/plugins/cache/openai-codex/codex/*/scripts/codex-companion.mjs 2>/dev/null | head -1)
node "$codex_mjs" review --wait --scope working-tree
```

**`review` sends Codex a *target*, not a diff — so the scope you pass decides
which half of Step 1's span it looks at, and the default silently picks one.**
`executeReviewRun` takes the native branch for `reviewName === "Review"`
(`codex-companion.mjs`), handing `runAppServerReview` only the object below;
Codex's built-in reviewer then resolves the change set on its own. Scope
selection itself is `resolveReviewTarget` in `scripts/lib/git.mjs`:

| Codex invocation | Target object sent | Looks at |
|---|---|---|
| `--scope working-tree` | `{type: "uncommittedChanges"}` | uncommitted work — **not** local commits |
| `--scope branch --base origin/main` | `{type: "baseBranch", branch: "origin/main"}` | the branch vs that base — commit history |
| `--scope auto` (the default) | whichever of the two `resolveReviewTarget` picks | working-tree **whenever the tree is dirty at all** (staged, unstaged, *or* untracked), else branch |

Two consequences, both load-bearing:

- **No local diff assembly happens on this path.** The companion does not read
  your files, so there is nothing to pre-stage and no size cap to work around —
  and `git add -N` would be *actively harmful*, because turning an untracked file
  into a tracked-unstaged one changes what `uncommittedChanges` resolves to.
  (Local assembly, with a 24 KiB `MAX_UNTRACKED_BYTES` cap that degrades a large
  untracked file to a `(skipped: …)` marker, happens only on the
  `adversarial-review` path — see below.)
- **The "Looks at" column is what the target *means*, not a diff span this repo
  verified.** How Codex's internal reviewer expands `baseBranch` — whether it
  folds in uncommitted work — is decided inside Codex, not here. Don't assert it.

So match the invocation to what Step 1 found:

- **Uncommitted only** (the common case) → one run, `--scope working-tree`.
- **Local commits only, clean tree** → one run,
  `--scope branch --base origin/main`. Pass the base as a **ref name**, not the
  resolved `"$base"` SHA: `--base` short-circuits `resolveReviewTarget` (so it
  does override `--scope` and skip Codex's own default-branch detection) but the
  value lands in a protocol field named `branch`, and whether a 40-hex SHA is
  accepted there is **not verified** — the type lives in a generated module absent
  from the plugin cache. Letting Codex resolve the fork point from a ref is the
  safe form; if you do test the SHA form, record the result here.
- **Both** → **two runs**, one of each. A single `auto` run would take the
  working tree and never look at the commits. Aggregate both reports in Step 3.

Launch each with `run_in_background: true` (the `--wait` keeps it foreground
*inside* that background shell so the output is captured verbatim). If you need
custom framing/focus, `review` rejects focus text outright, so use the
adversarial subcommand — again by script, not by slash command:
`node "$codex_mjs" adversarial-review --wait --scope working-tree "<focus>"`.
That path is the one that assembles the diff locally (`collectReviewContext`), so
it *is* subject to the 24 KiB `MAX_UNTRACKED_BYTES` untracked-file cap and to
`buildBranchComparison`'s two-dot `merge-base..HEAD` range under `--scope branch`
— neither of which applies to plain `review`. Gotcha:
if the review 400s on the reviewer model, the Codex CLI is too old / its default
model needs a switch (see CLAUDE.md "Codex review on a worktree").

**`nc-reviewer`** (second engine) — the project's dedicated review agent
(`.claude/agents/nc-reviewer.md`). Spawn it **named** via the Agent tool
(`subagent_type: nc-reviewer`; background is the default) so later rounds can
resume it with `SendMessage`. Its definition already carries the project
primer, the doc map, and the review protocol (untracked-file coverage,
severity + file:line, leads vs findings, the doc-staleness pass) — your prompt
supplies only what the session knows:

- the target checkout's **absolute path** (it runs `git` from inside it);
- the **resolved base SHA** from Step 1 and whether local commits exist — so it
  scopes to `git diff "$base"` plus untracked, not `git diff HEAD`;
- the Step 1 framing, verbatim;
- the task id / docs the change claims to implement, if any;
- the standing rule below (review-only, whole change span, no PR).

Its report includes a doc-staleness section — docs this diff made stale, plus
pre-existing stale content it noticed along the way. Diff-caused staleness is
an ordinary finding; treat the pre-existing items as report material for the
user, not work for the fix agent (unless the user asked for a docs cleanup).

**Do not try to call `/code-review`.** It is marked
`disable-model-invocation`: an agent cannot run it through the Skill tool and
will just get `Skill code-review cannot be used with Skill tool due to
disable-model-invocation`. Only the *user* can type it. If the user wants the
built-in specifically, they type `/code-review` themselves and paste (or leave
in-session) its findings for Step 3 to aggregate — treat that as a bonus third
input, not a prerequisite. The loop must run without it.

**Optional built-in adjacents** — these *are* agent-invocable. Use one only when
the diff clearly warrants it, and still review-only:

| Command | Run when |
|---|---|
| `/security-review` | the change touches input parsing, file writes, or anything attacker-reachable |
| `/simplify` | optional final polish, *after* the loop is otherwise clean — it does quality only and explicitly does not hunt bugs, so it never substitutes for the review pass |

**Standing rule for every reviewer**, whichever engine: scope is **this branch's
whole divergence, no GitHub PR** — `git diff "$base"` (merge-base vs working
tree, which covers local commits + staged + unstaged) *plus* every untracked file
from `git status --short --untracked-files=all` (bare `--short` hides files inside
a new directory); findings reported **by severity with file:line**; and
**do NOT modify any files — review only.** Only the fix agent edits.

## Step 3 — Aggregate and VERIFY

When both engines have reported — the `nc-reviewer` agent's report and Codex's
background job(s) collected — consolidate into one severity-ranked list (Critical /
High / Important / Medium / Low), deduping overlaps. Note where the two engines
agree: independent convergence on the same line is the strongest signal
available here. If you ran two Codex scopes, they are still *one* engine — an
overlap between them is not cross-engine corroboration.

**Verify each non-trivial finding against the actual code before acting.**
Reviewers produce false positives (a Codex "won't compile" P0 was wrong — a
`Copy` field destructure, not a move). Read the cited lines; if a finding is
wrong, reject it and say why — do not forward it to the fix agent. Confirm real
ones with a concrete failure scenario (inputs → wrong output).

Neither engine's own confidence substitutes for this step — an `nc-reviewer`
finding, a Codex finding, and a `CONFIRMED` verdict in user-supplied
`/code-review` output all get the same treatment (a `/code-review` `PLAUSIBLE`
maps to a *lead*). Independent verification against the code is what this step
buys. Verify doc-staleness findings the same way — read the doc and the code it
describes; only diff-caused staleness goes to the fix agent.

## Step 4 — Route real findings to the ONE fix agent

Spawn (or `SendMessage`-resume) a single named **`nc-fixer`** agent
(`.claude/agents/nc-fixer.md`, `subagent_type: nc-fixer`) — never have the
reviewers fix their own findings. Its definition already carries the standing
constraints (add no commits, all four CI gates green in order,
four-coupled-spots, design-spec is the sole design source, per-item report with
verbatim gate results). Your prompt supplies:

- the target checkout's **absolute path**;
- the precise, itemized set of verified findings (`file:line`, the claim, the
  failure scenario) — verified findings only, never raw leads;
- any diff-caused doc-staleness items that belong in this round.

## Step 5 — Converge

- A round with only test/doc/comment fixes (no correctness change) does **not**
  need a fresh full review round — the fix is its own evidence, plus its new test.
- A round that changed **correctness** gets a targeted re-review of the delta.
- Stop when the round is **clean or LOW-only**. Declare the loop **converged**.
- If a bounded finder was capped (top-N, sampling), say so — never present a
  capped pass as exhaustive.

## Step 6 — Report

Give the user: the review scope you settled on (base SHA, and whether it spanned
local commits, staged/unstaged, untracked — plus which Codex scope(s) ran), the
consolidated aggregate (with any false-positives you rejected and why), what the
fix agent changed, and the final gate results. State plainly that the loop added
**no commits** and the fixes sit in the working tree **awaiting their manual
review** before PR / merge. Do not open a PR or merge here — that's `/ship` and
the user's call.

## Invariants (do not break)

- Reviewers **never** modify files; only `nc-fixer` does. That includes you,
  the orchestrator: aggregate, verify, hand off — **do not fix as you read.**
  Fixes go through the single fix agent in Step 4, so one actor owns the tree
  and the gate run.
- **Verify before forwarding** — a rejected false-positive is a good outcome.
- **The loop adds no commits and pushes nothing.** Pre-existing local commits are
  fine — they're part of the review scope — but every fix lands as a working-tree
  change on top of them, and the user does the final review and merge.
- **Scope from the merge-base, never from `HEAD` alone**, or work already
  committed on the branch goes unreviewed — and never from a two-dot
  `"$base"..HEAD` diff, or the uncommitted work does.
- Two engines, always — Codex *and* `nc-reviewer` — because they miss different
  things: one is primed with this project's conventions and doc map, the other
  deliberately sees the diff cold with no repo priors.
- Name every agent you spawn — the `nc-reviewer` and the `nc-fixer` — so later
  rounds resume them with context (`SendMessage` by name). A resumed
  `nc-reviewer` re-reviews the *current* tree with its earlier findings in
  context, which is exactly what a Step 5 targeted re-review wants.
