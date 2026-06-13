# Negative Converter — Progress Log

How each task is actually being carried out — what was done and how, key
decisions, what works, what doesn't, and notes for dependent tasks. TASKS.md holds
the authoritative status (the checkboxes); this file is the narrative beside it.

One `##` section per task, named by the kebab task name. Read this before starting
a task; update your own section as you work. Append entries — don't rewrite them.

## project-foundation
**Status:** not started
**Updated:** —

- Goal: Cargo project, dependency declarations, module skeleton, and shared core
  types (`LinearImage`, `FilmBase`, `OutDepth`, `NcError`, param structs).

## silverfast-decode
**Status:** not started
**Updated:** —

- Goal: read SilverFast HDR (48-bit RGB) and HDRi (64-bit RGB+IR) TIFFs into a
  linear `f32` `LinearImage`, preserving the IR plane.

## tiff-encode
**Status:** not started
**Updated:** —

- Goal: write u16/f32 TIFF with embedded ICC, BigTIFF auto-promote, IR export, and
  sidecar JSON.

## color-management
**Status:** not started
**Updated:** —

- Goal: working→output ICC transforms with depth-aware default profile (sRGB for
  u16, wide-gamut for f32); provide the ICC blob to embed.

## film-base-estimation
**Status:** not started
**Updated:** —

- Goal: estimate `Dmin` `FilmBase` from border/region with full CLI override.

## algo-interface
**Status:** not started
**Updated:** —

- Goal: `Converter` trait + algorithm selection so converters are pluggable.

## cli-framework
**Status:** not started
**Updated:** —

- Goal: clap subcommands, recipe load/merge (flags override), JSON report,
  `params` subcommand, exit-code mapping.

## algo-simple
**Status:** not started
**Updated:** —

- Goal: channel-inversion baseline converter (debug / B&W) with white balance and
  black/white points.

## algo-density
**Status:** not started
**Updated:** —

- Goal: density-domain converter (Cineon/negadoctor style) with separate density
  and print-render sub-stages; the default algorithm.

## pipeline-orchestration
**Status:** not started
**Updated:** —

- Goal: wire `convert`/`inspect`/`estimate` end to end, producing a positive TIFF
  and JSON reports from a real scan.
