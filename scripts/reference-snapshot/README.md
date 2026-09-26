# The frozen reference build

The pre-migration pipeline is preserved by **git, not by code** (CLAUDE.md's
migration rule): once the new chain replaces a path, the old behaviour lives only in
a build of the reference commit. This directory is how that build is made and run.

| | |
|---|---|
| **Reference** | the head of the `reserve` branch |
| **Where it started** | tag `pre-new-flow` = `0da32d0` (#130), `pipeline_version` 5, the last commit before `--new-flow` |
| **Binary name** | `nc` — it predates the Hanten rename, and prints the `nc` banner |
| **Reference config** | `--preset sigmoid-knees --output-preset display-p3` |

`reserve` may take cherry-picked fixes; the tag never moves. So "the reference" is
always named by **commit**, never by the branch: the build is cached per commit, and
every cell rendered from it is labelled with the commit it reports.

## 1. Build it

```sh
NC_REF=$(scripts/reference-snapshot/build.sh)
```

This fetches `reserve`, builds it with `cargo build --release --locked` in a
temporary worktree, and caches the binary at
`~/Library/Caches/hanten-reference/reserve/<commit>/nc`
(`${XDG_CACHE_HOME:-~/.cache}/hanten-reference/…` off macOS). Then it prints the
path. The worktree and target dir are removed afterwards. A cached commit is
re-verified rather than rebuilt. The fetch is silent, and the script only says it could
not fetch. Run it every time: if `reserve` has moved, it prints `building` and a note
instead of `cached`. A fresh build takes about a minute on Apple silicon.

Before a binary goes into the cache, its `--version` must report **exactly** that
commit, from a clean tree. `BUILD_INFO` beside the binary records the commit, the
`rustc` version, the host triple and the binary's sha256.

| Variable | Effect |
|---|---|
| `HANTEN_REFERENCE_CACHE` | cache root |
| `HANTEN_REFERENCE_TARGET` | build in this cargo target dir, and keep it |
| `HANTEN_REFERENCE_REBUILD=1` | rebuild even if the commit is cached |

A different ref is `build.sh <branch-or-tag>`, cached under its own name.

**Determinism is per build and architecture** (CLAUDE.md). Two builds of the same
commit with the same toolchain were byte-identical — even the binaries were. A different
`rustc` or host may move pixels by an ulp, so compare within one cache entry.

## 2. Run the reference

```sh
"$NC_REF" convert <frame.tif> --preset sigmoid-knees --output-preset display-p3 \
  --film-base <r,g,b> -o <out>.tiff
```

- **`sigmoid-knees`** is the best result reviewed before the migration
  (`docs/design-update.md`, "Reference for the migration").
- **`display-p3`** is the same destination `--new-flow` writes: 16-bit TIFF, Display P3
  ICC. That means a reference cell and a new-flow cell differ in the pipeline and not in
  the container. To view one in a browser, convert it to sRGB JPEG the same way as another
  tool's output (`render-review-set`, §5 step 1). The source is a 16-bit **Display P3**
  TIFF with its profile embedded.
- **Frames are not pinned.** A consuming task brings its own, with the film base measured
  for its roll (the review generator's `{dmin}`).

**Check your build against the fixture.** On `aarch64-apple-darwin` with rustc 1.98.1,
`reserve` @ `0da32d0` gives:

```sh
"$NC_REF" convert tests/fixtures/hdr-48bit.tif --preset sigmoid-knees \
  --output-preset display-p3 --film-base 1,1,1 -o /tmp/ref.tiff
shasum -a 256 /tmp/ref.tiff
# e0355a9a2513b947bc7cfea3de6365a23948642399d176415cef0ca0fb79f236
```

On another host or toolchain, record your own value rather than expecting this one.

## 3. Convert negatives with it

Once the old paths are retired, the live `docs/using-nc.md` describes only the new CLI.
**The reference's own guide travels with it** and was checked against that binary:

```sh
git show origin/reserve:docs/using-nc.md | less   # §4 workflow, §5 recipes, §6 presets
"$NC_REF" convert --help                           # likewise roll, estimate, inspect
```

The workflow for the reference config, run against the cached binary (write `nc` below
as `"$NC_REF"`). It is also a way to render a whole roll for comparison without a review
matrix:

```sh
# 1. What the scan is; base_candidates, when present, are rebate rectangles for step 2
"$NC_REF" inspect scan.tif | jq '{decode, input_color, base_candidates, warnings}'

# 2. The film base, measured once per roll. Mandatory: convert and roll refuse without one.
#    Best from an unexposed frame; --grid warns if its cells disagree.
"$NC_REF" estimate base.tif --grid | jq -r .film_base_flag     # → --film-base R,G,B
#    or from a known border:  estimate scan.tif --base-region X,Y,W,H

# 3. One frame, and freeze the resolved recipe: sigmoid-knees expanded, the base explicit
"$NC_REF" convert scan.tif --preset sigmoid-knees --output-preset display-p3 \
  --film-base R,G,B --dump-params roll-recipe.json -o scan_positive.tiff

# 4. The roll. It takes no conversion flags, only the recipe
"$NC_REF" roll frames/*.tif --out-dir positives/ --params roll-recipe.json
```

Checked on the committed fixture: a frame from step 4 is byte-identical to the same
frame from step 3.

- **Freeze only explicit values.** `--dump-params` writes *modes*, not measurements, so
  `--auto-base` dumps `"auto"`, and the roll would re-measure every frame. Pass the
  number from step 2.
- **`Dmax` stays at the fixed nominal 1.3**, which is what `sigmoid-knees` was reviewed
  with. Measuring it from a leader (`estimate --d-max-region`) is optional; the
  reference build's own guide documents that step and its caveat
  (`git show origin/reserve:docs/using-nc.md`, §4 step 3 and §6) — the live guide
  dropped them when the reference density retired.
- **A reference recipe only means something to the reference binary.** The new chain
  refuses it because it has no `"recipe_version": 2`. Keep it beside the roll's output,
  not in the tree.
- Every output's `<out>.json` sidecar carries the build identity and the full recipe,
  and `--params <sidecar>` reproduces the output.

## 4. Put it in a review set

Add it to a matrix as one arm of a build axis (`render-review-set`, §3):

```json
"builds": [
  {"id": "ref", "label": "reference", "nc": "<path from build.sh>",
   "expect_commit": "0da32d0"},
  {"id": "new", "label": "candidate", "nc": "target/release/hanten"}
]
```

The rest of the matrix (`schema_version`, `output_preset`, `common_args`, `configs`) is
as `render-review-set` §3 describes. Paths resolve against the working directory (run
from the repo root), and the candidate arm must already be built. The `ref` arm's `nc`
is only a placeholder: point it at the cache at render time rather than committing a
machine path:

```sh
PYTHONPATH=scripts/analysis .venv/bin/python -m nctool review generate <matrix> \
  --fixtures <fixtures> --build ref="$(scripts/reference-snapshot/build.sh)" --out <dir>
```

**Always state `expect_commit` on the reference arm.** Without it, a wrong path still
renders. Its cells carry the right commit in their `producer` block, but they sit under
the name "reference" and nothing refuses them. With it, a binary whose `--version`
names another commit is refused before anything renders (exit 2), and every cell's
own report is checked again as it renders. Any 7+ digit prefix works, and a dirty
build of the right commit is refused too. When `reserve` moves, `build.sh` builds the
new commit and says so; update `expect_commit` on purpose.

A config only one build accepts opts out of the other arm — `"builds": ["new"]` for a
flag added after the tag, `"builds": ["ref"]` for one retired since. `--preset
sigmoid-knees` is the latter: the candidate refuses it since `nf-retire/sigmoid-and-simple`,
so it renders only on the reference arm.

**Known gap: one matrix cannot hold a reference cell and a `--new-flow` cell.** A
matrix states one output target for every cell: an `output_preset` (the current chain,
which the reference arm needs) or a `destination` (the new chain, which passes
`--new-flow` and the destination flags — a flag the reference build does not have).
Render the new-flow side as its own `destination` matrix, or with `hanten convert
--new-flow` directly, and add its cell to the reference set's `review.json` by hand.
