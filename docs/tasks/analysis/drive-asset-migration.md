# Drive Asset Migration

## Goal

Make working from the Google Drive-hosted asset folder robust across machines,
now that the assets — inputs *and* conversion outputs — physically live there
(moved 2026-07-24 to `…/GoogleDrive-…/My Drive/temp/nc-assets`, reorganized into
`rolls/ samples/ converted/{nc,nlp}/`, with a self-relative `manifest.json` at the
root). The move and reorg are **done**; this task covers the remaining
robustness/tooling and the repo path-convention decision.

## Design

The manifest is self-locating (paths relative to its own directory), so no
absolute root is baked into any data. The remaining work:

- **Repo path convention (decide first).** The codebase, `CLAUDE.md`, the harness
  default (`A=../nc-assets`), and the reports all assume a local `../nc-assets`.
  The Drive path has spaces and is machine/account-specific, so it must not be
  hard-coded. Recommended bridge: a machine-local symlink
  `~/src/nc/nc-assets → <Drive>/temp/nc-assets` so `../nc-assets` keeps working
  for every worktree unchanged; the tool also accepts an explicit
  asset-root override (`NC_ASSET_ROOT` env or a positional arg today; a
  `--asset-root` flag is planned once the generator folds into `nctool`). The
  symlink is not committed (machine-local).
- **Path portability** — resolve the asset root from the symlink/env/flag and
  confirm the manifest's relative paths hold under the Drive mount on each machine.
- **Stream-on-demand vs materialized files** — Drive (File Stream) may present
  placeholder files not yet downloaded. `manifest generate`/`validate` and the
  metrics tool must detect a non-materialized file and either trigger/await
  download or fail loudly — never checksum or analyze a placeholder as if it were
  real data.
- **Checksum semantics** — decide whether sha256 is computed on the materialized
  bytes (correct but forces download) and how drift is interpreted when Drive
  re-syncs.
- **Write-back** — conversion outputs written into the Drive-backed `converted/`
  must flush/sync so other machines see complete files; document the workflow.
- **I/O cost** — larger latency over Drive; keep the "one image at a time"
  discipline and avoid gratuitous full-tree walks.

## Implementation Suggestion

- Keep the default local; make Drive an explicit opt-in (`NC_ASSET_ROOT` /
  `--asset-root`) so existing local workflows don't change under people's feet.
- Test the materialization-guard against an actually-dehydrated Drive file, not
  just a present one.

## How to Verify

- Pointing `asset_root` at the Drive mount, `manifest validate` passes on a fully
  materialized tree and **fails loudly** (not silently) on a dehydrated
  placeholder.
- A conversion output written to the Drive-backed `converted/` is complete and
  readable from a second machine after sync.
- Local-default behavior is unchanged when the opt-in is not set.

## Dependencies

- [Asset Manifest](asset-manifest.md)
