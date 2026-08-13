# Sigmoid baseline review tools

These scripts generated the human-review evidence for the reference-anchored
sigmoid work. They render selected personal photographs into throwaway local
directories and build interactive HTML pages. Nothing produced here belongs in
the repository or `nc-assets`.

`fixtures.json` is the curated study declaration: roll calibration, source
checksums, reviewed shadow/mid/white rectangles, patch-validity decisions, and
exposure preferences.

## Patch review

`patch-review.sh` answers whether deterministically proposed rectangles actually
represent textured shadow, mid-grey-equivalent material, and diffuse white. It
also renders a real `--print-exposure` sweep at sigmoid contrast 2.0.

First capture the ignored proposal test:

```sh
cargo test --release --quiet shadow_metrics::propose \
  -- --ignored --nocapture --test-threads=1 > /tmp/propose.txt
bash scripts/sigmoid-baseline/patch-review.sh /tmp/propose.txt
```

The default page is `../temp/patch-review/index.html`. Pass a second argument to
choose another output directory. Set `EVS` to override the exposure matrix. The
Python helper `build_patch_review.py` parses the proposal output and builds the
page; the shell runner is the supported entry point.

## Candidate-anchor review

`candidate-review.sh` renders every candidate sigmoid anchoring rule declared by
`build_candidate_review.py` for every fixture frame, then builds a click-to-zoom
comparison page:

```sh
cargo build --release
bash scripts/sigmoid-baseline/candidate-review.sh
```

The default page is `../temp/candidate-review/index.html`. Pass an output
directory as the first argument. To compare the two surviving anchor forms over
three shoulder widths instead of the full anchor matrix:

```sh
STUDY=--shoulder bash scripts/sigmoid-baseline/candidate-review.sh
```

`build_candidate_review.py --plan` emits the exact render matrix consumed by the
shell script; `--page` uses the same rules for captions, preventing the rendered
configuration and displayed label from drifting apart.

## Requirements and behavior

- macOS `sips`, Python 3, a release `nc`, `../nc-assets`, and the frozen
  `scripts/real-scan-verify/recipes` are required.
- Both runners delete old JPEGs in their selected output directory before a run,
  preventing stale images from making a partial render look complete.
- They render through `ultra-hdr-v1` because its JPEG base uses the same SDR path
  the study measured. `sips` discards the gain map while downsizing; these pages
  therefore review SDR appearance only.
- A partial page is still generated if a render fails, but the runner exits
  non-zero so automation cannot mistake it for a complete comparison.
