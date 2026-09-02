# hdr-tone-review

The HDR visual-review set for
[`display-tone-mapping`](../../docs/tasks/output/display-tone-mapping.md): renders the
same frames as real **gain-map JPEGs** through the `nc` binary and builds a page for
comparing them by eye.

```sh
cargo build --release
python3 scripts/hdr-tone-review/generate.py          # writes ../temp/tonemap-hdr-review/
open ../temp/tonemap-hdr-review/index.html
NC_TONEMAP_FRAMES=P3,G2 python3 scripts/hdr-tone-review/generate.py   # a subset
NC_TONEMAP_OUT=/tmp/review python3 scripts/hdr-tone-review/generate.py
```

**It fails loudly rather than writing a partial page.** A missing or broken `exiftool` /
`sips` exits non-zero with one line; a frame set that matches nothing writes no page at all.
That is deliberate and specific to this directory: the plateau share below is the column the
reader is told to use, so a run that silently degraded it to `nan` would produce a page
that looks complete and answers the review question with nothing.

**macOS-only, and deliberately not part of CI** — it needs `../nc-assets`, `exiftool`,
`sips`, and an HDR display to be worth looking at. Prints derived numbers only, never
pixels.

Three configs per frame, the three outcomes the task established:

| config | reconstruction | display tone | gain map |
|---|---|---|---|
| `default` | shipped sigmoid | shipped shoulder | inert (1.00x) |
| `s0-shoulder` | `--sigmoid-shoulder 0` | shipped shoulder | live but plateaued |
| `s0-reinhard` | `--sigmoid-shoulder 0` | `--display-tone reinhard` | live and separated |

## Two traps

**Do not convert the JPEGs to PNG.** The SDR review page did exactly that, correctly —
here it would discard the gain map and make every config look alike. The page loads the
JPEGs as written so the browser HDR-decodes them, and it reports whether the display
actually claims `dynamic-range: high`, because otherwise "they all look the same" is a
verdict on the decode rather than on the operator.

**`GainMapMax` does not discriminate, and reading it as the headline is a mistake this
directory exists to prevent.** It is a single extremum, so any near-asymptotic highlight
reaches it: measured across four frames the shouldered render reports 4.87x and the
unbounded one 4.79x, both identical on every frame. What differs is the *distribution* —
the shoulder pins **6.6–15.2%** of the frame onto the top gain code while the unbounded
tone pins **0.26–0.61%**. That plateau share is the flat blob in numbers, and it is the
column to read. Note it degenerates by construction once the plateau exceeds 10%: the
90th percentile then lands on the top code too, so the reported code spread collapses to
zero for a reason that has nothing to do with the frame.
