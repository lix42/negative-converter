# Pipeline Orchestration

## Goal

Wire all stages into the working CLI: implement the `convert`, `inspect`, and
`estimate` subcommands end to end, producing a positive TIFF from a real scan and
machine-readable JSON for the inspect/estimate paths. This is the integration
task that makes `nc` actually do its job.

## Design

`main.rs` / `pipeline/stages.rs` compose the pieces built by the other tasks:

**`convert`** (the full pipeline):
```text
1. decode(input)                              # silverfast-decode
2. estimate(film_base params)                 # film-base-estimation
3. build(algorithm, params).convert(img,base) # algo-interface + algo-simple/density
4. resolve_output_space + to_output(...)      # color-management
5. encode_tiff(output, ...) + write_sidecar   # tiff-encode
   (+ export_ir if requested)
```

**`inspect`**: `decode` + report format/channels/bit depth + suggested `Dmin`
(`estimate`) as JSON; no output image.

**`estimate`**: run only `film-base-estimation`; emit `FilmBase` + region as JSON.

- Resolve the config from `cli-framework` (recipe + flag overrides).
- Collect warnings (clipped highlights/shadows, IR present-but-ignored, BigTIFF
  auto-promotion) into the `Report`; honor `--strict` (warnings → failure).
- Map any `NcError` to the documented exit code; keep stdout = JSON report only.
- Write the effective recipe to the sidecar so a run is fully reproducible.

## Implementation Suggestion

- Keep `main` thin: parse → dispatch on `Command` → call a `run_convert`,
  `run_inspect`, `run_estimate` function each returning `Result<Report, NcError>`.
- Thread a single `Report` accumulator through the stages so warnings/estimates
  land in one place for emission.
- Add at least one end-to-end test using the user's sample HDR/HDRi files (or a
  committed small synthetic scan) asserting an output TIFF is produced and the
  report JSON has the expected fields.
- This is where the real sample files matter most — validate the full path on them.

## How to Verify

- `nc convert sample.tiff -o out.tiff --algorithm density` produces a valid
  positive TIFF + `out.tiff.json` sidecar; `--out-depth f32` produces a float TIFF.
- `nc convert ... --algorithm simple` works as the baseline path.
- `nc inspect sample.tiff --report json` emits format/channel/Dmin JSON, no image.
- `nc estimate sample.tiff` emits a `FilmBase` JSON.
- `--export-ir` on an HDRi input writes the IR file; absent on HDR input is handled.
- `--strict` turns a clipping warning into a non-zero exit; normal mode warns and
  succeeds. Exit codes match the documented table.

## Dependencies

- [SilverFast HDR/HDRi decode](silverfast-decode.md)
- [TIFF encode and output](tiff-encode.md)
- [Color management](color-management.md)
- [Film-base / Dmin estimation](film-base-estimation.md)
- [Simple inversion algorithm](algo-simple.md)
- [Density-domain algorithm](algo-density.md)
- [CLI framework](cli-framework.md)
