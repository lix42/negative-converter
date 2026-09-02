# The review-set format

A **review set** is one `review.json` plus the images it names. The app loads it
from `?data=<path to review.json>` and resolves every image path **relative to
that file**, so a set is a self-contained directory you can move, copy or serve
from anywhere.

Keys are `snake_case`, matching nc's own reports and recipes — the producers are
nc-adjacent scripts, not JavaScript.

## Shape

```json
{
  "schema_version": 1,
  "title": "Display tone: shoulder vs none",
  "description": "Same reconstruction on both; only the display stage differs.",

  "configs": [
    { "id": "shoulder", "label": "shoulder", "note": "shipped Hermite shoulder" },
    { "id": "none", "label": "none", "note": "--display-tone none" }
  ],

  "images": [
    {
      "id": "E1",
      "label": "E1 — Ektar 100 · 20260713-nikon-971.tif",
      "note": "blown 6.86% → 5.65% · code sep 11.6 → 15.2",
      "renditions": {
        "shoulder": "E1-shoulder.jpg",
        "none": { "src": "E1-none.jpg", "preview": "E1-none-thumb.jpg" }
      }
    }
  ]
}
```

## Fields

| Field                  | Required | Meaning                                                              |
| ---------------------- | -------- | -------------------------------------------------------------------- |
| `schema_version`       | yes      | Must be `1`. A future format bumps it rather than changing this one. |
| `title`, `description` | no       | Shown above the first image.                                         |
| `configs[].id`         | yes      | Referenced by `renditions`. Must be unique.                          |
| `configs[].label`      | yes      | Button text. Keep it short — it sits in the top bar.                 |
| `configs[].note`       | no       | Tooltip on the button.                                               |
| `images[].id`          | yes      | Used as the label when none is given.                                |
| `images[].label`       | no       | Heading for the section.                                             |
| `images[].note`        | no       | A line beside the heading — the natural home for measured numbers.   |
| `images[].renditions`  | yes      | Object keyed by **config id**.                                       |

A rendition is either a **string** (the image path — the common case) or an
object:

| Field             | Required | Meaning                                                                                                                                                                                                                                                                                                                       |
| ----------------- | -------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `src`             | yes      | Path to the full image, relative to `review.json`.                                                                                                                                                                                                                                                                            |
| `preview`         | no       | Thumbnail path. Defaults to `src`.                                                                                                                                                                                                                                                                                            |
| `width`, `height` | no       | Natural size in pixels. Lets the page reserve space before the image loads — it never scales the picture, and `fullsize` always shows the file at 1:1. State them correctly or leave them out: wrong numbers mis-size the reserved box and the mini-map, and the app logs a console warning when they disagree with the file. |

## Two rules worth knowing

**Order matters.** `configs` order sets both the button order and the keyboard
mapping: `1`–`9` select the first nine, `0` selects the tenth. Past ten there is
no key, but the buttons still work.

**An unknown config id is an error; a missing rendition is not.** A rendition
keyed by a config that was never declared is a typo the author wants to hear
about, so the whole set is refused with a message naming it — the same reason
every recipe struct in nc uses `deny_unknown_fields`. A config that simply has no
rendition for one image is ordinary (it failed to render, or the frame was added
later), so that slot renders as a visible gap and the rest of the set still
loads. A comparison silently missing half of itself is the worst outcome of the
three.
