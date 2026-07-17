---
name: pr-comments
description: >-
  Work a GitHub PR's review comments to done: gather every comment, verify each
  against the code, triage (fix / push back / defer), update the PR where
  warranted, then reply to and resolve every thread — looping until the PR is
  green and quiet. Use when asked to "check PR comments", "address review
  feedback", "reply and resolve comments", or to drive a PR through its review
  bots. Invoke as `/pr-comments <pr-number-or-url>`.
---

# Address PR review comments

Carry a pull request's review feedback from "comments posted" to "all threads
resolved, checks green" — without rubber-stamping. Every comment is **verified
against the code**, then triaged: fix the real ones, push back with reasoning on
the wrong ones, defer the out-of-scope ones — and **reply to every thread** so
the record says what happened. Never merge the PR; that's the user's call.

## Inputs

- **Which PR.** A number or URL. Derive `OWNER/REPO` from the remote
  (`gh repo view --json nameWithOwner`) or the URL. Everything below uses
  `gh` / `gh api`.

## Step 1 — Gather everything

Pull the full picture before touching anything:

```
gh pr view <n> --json title,state,headRefName,mergeStateStatus,reviewDecision
gh pr checks <n>                                    # CI + review-bot check states
gh pr view <n> --json comments  -q '.comments[]'    # issue-level (conversation) comments
gh pr view <n> --json reviews   -q '.reviews[]'     # review summaries (bodies)
gh api repos/OWNER/REPO/pulls/<n>/comments          # INLINE review comments (has .id, .path, .line, .in_reply_to)
```

For resolve state and thread node IDs, use GraphQL (REST doesn't expose either):

```
gh api graphql -f query='
query($o:String!,$n:String!){ repository(owner:$o,name:$n){ pullRequest(number:<n>){
  reviewThreads(first:100){ nodes { id isResolved isOutdated
    comments(first:1){ nodes { databaseId author{login} body } } } } } } }' -f o=OWNER -f n=REPO
```

Note two distinct IDs: the **comment** `databaseId`/`.id` (REST, for replying via
`in_reply_to`) and the **thread** node `id` (`PRRT_…`, GraphQL, for resolving).
Don't cross them.

## Step 2 — Filter to what's actionable

Drop, without ceremony: already-**resolved** threads, **outdated** threads whose
code changed out from under them, and bot **boilerplate** (e.g. a "this bot has
been sunset" notice, CI-sandbox "couldn't run tests" asides). What remains is the
real work list. Note each comment's severity label if it has one.

## Step 3 — VERIFY each finding against the code

**This is the step that separates this skill from blind compliance.** Reviewers —
especially LLM review bots — produce false positives and routinely re-flag
*deliberate, documented* design choices as bugs. For each finding:

- Open the cited `file:line` and read the actual code.
- Confirm a real defect with a concrete failure scenario (inputs → wrong result),
  or establish that it's already handled / intended.
- Watch for the common false-positive shapes: a "won't compile" that does (e.g. a
  `Copy`-field destructure read as a move); a "silent failure" that's actually
  reported one call up; a guarantee "violated" that a code comment already carves
  out as an exception.

A finding you can refute is a *good* outcome — it just routes to "push back," not
"fix."

## Step 4 — Triage and decide, per comment

| Verdict | Action |
|---|---|
| Real bug / clear improvement | **Fix it** (Step 5). |
| Valid concern, but the current behavior is deliberate/documented | Usually **make the doc/comment honest** (state the real guarantee + its boundary) rather than refactor sound code; reply explaining. Sometimes a small hardening is warranted — judge it. |
| False positive / factually wrong | **Push back** with reasoning; change nothing. |
| Correct but out of scope for this PR | **Acknowledge + defer** (file/here-note a follow-up); change nothing now. |

Push back when you disagree — don't just comply. But when the fix is cheap and the
reviewer is right, do it; don't argue to win.

## Step 5 — Update the PR (only where warranted)

- Make the change on the PR's `headRefName` branch (check it out; it may be a
  worktree). For a docs-honesty fix, edit the doc/comment; for a code fix, the code.
- **Re-run the project's quality gates** and get them green before pushing (match
  CI — e.g. for this repo: `cargo fmt --all --check` → `cargo clippy --all-targets
  -- -D warnings` → `cargo build` → `cargo test`). Never push past a red gate.
- Commit with a message naming the finding it addresses; push.
- **Before pushing, `git fetch` the branch** — reviewers/users can accept
  *suggestion commits* through the GitHub UI, so the remote may be ahead. If it
  diverged, rebase your change on top and reconcile (keep the superset; don't
  clobber their commit), then push (`--force-with-lease` after a rebase).
- If the branch is **behind base** and that blocks checks, rebase onto the base
  branch, re-run gates (the base moved), and force-push.

## Step 6 — Reply to every thread

Reply to **each** actionable thread — the ones you fixed *and* the ones you
declined — saying what you did (cite the commit SHA) or why you didn't:

```
gh api repos/OWNER/REPO/pulls/<n>/comments -f body="<reply>" -F in_reply_to=<comment_databaseId>
```

A thread with a change but no reply reads as ignored; a decline with no reasoning
reads as dismissive. Neither is acceptable.

## Step 7 — Resolve threads

Resolve each thread you've handled (GitHub auto-resolves ones whose lines changed
/ went outdated — resolve the rest):

```
gh api graphql -f query='mutation($id:ID!){ resolveReviewThread(input:{threadId:$id}){ thread { isResolved } } }' -f id=<thread_node_id>
```

Shell note: **zsh doesn't word-split unquoted variables**, so a multi-ID string
won't iterate as separate args — loop with `for id in $(…)` / `while read -r id`
and pass one ID per call. Verify at the end:

```
gh api graphql -f query='query($o:String!,$n:String!){repository(owner:$o,name:$n){pullRequest(number:<n>){reviewThreads(first:100){nodes{isResolved}}}}}' \
  -f o=OWNER -f n=REPO -q '[.data...reviewThreads.nodes[].isResolved]|"resolved=\(map(select(.==true))|length)/\(length)"'
```

## Step 8 — Loop until quiet

Pushing a fix makes the review bots **re-run** and often post *new* comments (a
fix can introduce or reveal a new nit). Re-check (Step 1) after the checks settle;
new actionable comments → repeat from Step 3. **Stop** when: CI is green, and
there are no unresolved actionable threads. Then report.

## Step 9 — Report

Summarize for the user: which comments were **fixed** (with SHAs), which were
**pushed back on** (and why), which were **deferred**, the final check status, and
the PR's merge-readiness. Leave the **final merge to the user**.

## Invariants (do not break)

- **Verify before acting** — never fix or forward a finding you haven't confirmed
  against the code. A refuted finding is a valid outcome.
- **Reply to every thread**, resolved or declined; resolve only after replying.
- **Push back with reasoning** when the reviewer is wrong or the behavior is
  intended — don't comply just to clear the thread.
- **Never merge** the PR. Get it green and hand back.
- **Never push past a red quality gate.**
- **Fetch before you push** — reconcile UI suggestion-commits, don't clobber them.
