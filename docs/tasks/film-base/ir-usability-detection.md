# Decide IR usability by measurement, not by declared film type

## Goal

Stop gating IR-assisted holder detection on `--film-type chromogenic`. Decide from
the **IR plane itself** whether it can separate holder from film on this frame,
and demote `--film-type` from a gate to a hint.

## Why the current gate is wrong

It keys on the film's *chemistry*; the property that matters is the *frame's
density*. Measured on the HP5 roll (silver-halide, 2026-08-11), IR median
transmission:

| Frame | border p05 | interior p05 | interior median | separable |
|---|---|---|---|---|
| 1364 unexposed | 0.0229 | 0.4567 | 0.4734 | **yes — 20:1** |
| 1330 half-leader | 0.0194 | 0.0202 | 0.4620 | partly |
| 1354 regular | 0.0186 | 0.0236 | 0.0818 | no |
| 1329 leader | 0.0154 | 0.0151 | 0.0163 | **no — uniformly opaque** |

Silver blocks IR in proportion to accumulated density, so an *unexposed* frame is
strongly IR-transparent against an opaque holder, while a fully-exposed leader is
opaque everywhere and indistinguishable from it.

That matters because the `Dmin` workflow measures from an **unexposed reference
frame** — exactly where today's rule says "silver, IR off" and the measurement
says 20:1. Conversely the `Dmax` workflow measures from a **leader**, where IR can
never work for silver. The declared chemistry predicts neither.

The discriminator is stark enough to compute directly: border-vs-interior
bimodality. 1364 shows 0.023 against 0.457; 1329 shows 0.0154 against 0.0151.

## Open questions

1. **The exact statistic and threshold.** Border-vs-interior percentiles is the
   obvious first form, but the useful property is really "two well-separated
   modes, with the opaque one hugging the frame edge". Two rolls will not set a
   threshold — say what evidence would.
2. **What to do with a partially-separable frame** like 1330 (half leader, half
   unexposed): use the separable edges only, or refuse?
3. **Should a declared `--film-type` ever override the measurement?** Probably
   only to force it *off*, never on — but state it.
4. **Does the same measured verdict serve IR dust removal later**, or is that a
   different usability question? Worth knowing before the predicate is named.

## How to Verify

- The four HP5 frames above resolve to the verdicts in that table, from the IR
  plane alone with no `--film-type`.
- A chromogenic colour scan resolves *usable* without being declared — the
  behaviour that removes the flag's gate role.
- 1329 resolves *not usable* by itself: the check must fail safe on the frame
  where the holder and the film genuinely cannot be told apart.
- A shape-only IR plane (no `NewSubfileType=4` marker) is still refused, as today.
- Existing RGB fixtures produce unchanged film-base results.

## Dependencies

- [IR-assisted film-holder detection](ir-holder-detection.md)

The colour path needs nothing else. Validating the B&W evidence above needs
[gray primary decode](../io/gray-primary-decode.md), since nc cannot currently
read those files at all — deliberately not a hard dependency, so the colour
improvement is not blocked behind B&W input support.
