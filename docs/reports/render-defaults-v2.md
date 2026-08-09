# Render defaults v1 → v2

Measured baseline for the `pipeline_version` 2 default render (2026-08-08). Three
defaults moved together; this records what they do to real scans, so the change is
reviewable as evidence rather than as an argument.

| default | v1 | v2 |
|---|---|---|
| `NOMINAL_DMAX` (the `Fixed` anchor) | 2.0 | **1.3** |
| default density curve | exponential | **sigmoid** |
| `ExponentialParams::gamma` | 1.0 | **2.0** |

Film base is stated explicitly in every run below: it has no default (it lost one
*within* `pipeline_version` 1, deliberately without a bump — see the PR "Require the
film base to be stated"), and a roll's base is a calibration that both versions must
share or the comparison measures the base instead of the curve.

> **The first version of this table was wrong and is worth knowing about.** It was
> produced by a zsh helper function that interpolated an unquoted `$extra`
> parameter. zsh does not word-split unquoted parameters, so `--d-max 2.0` reached
> `nc` as a single argument and the flag was silently dropped — the "v1" column was
> measured with a v2 anchor, and reported clipping figures up to 72%. The numbers
> below come from [`scripts/render-defaults-v2/measure.py`](../../scripts/render-defaults-v2/measure.py),
> which passes an explicit argument list to `subprocess` and cannot reproduce that
> class of mistake. Re-run it before quoting anything here.

## The headline: v1 clipped up to 4.9% of a frame, v2 clips none

Measured with each roll's frozen `--film-base`, everything else default. "clipped"
is `loss.clipped_high / loss.total_samples` from the JSON report — samples the u16
encode had to pull back to white.

| frame | v1 clipped | v1 mean (G) | v2 clipped | v2 mean (G) |
|---|---|---|---|---|
| Ektar 20260713-nikon-963 | 0.00% | 0.1037 | **0.00%** | 0.1002 |
| Ektar 20260713-nikon-971 | **3.38%** | 0.2785 | **0.00%** | 0.4697 |
| Portra160 20260720-nikon-1058 | **4.86%** | 0.5314 | **0.00%** | 0.9794 |
| Portra160 20260720-nikon-1065 | **1.98%** | 0.2108 | **0.00%** | 0.2907 |

Three of four frames lost highlight data under v1, between 2% and 5% of their
samples. v2 loses none on any of the four, and that part is structural rather than
lucky: with `shoulder > 0` the sigmoid approaches display white `1.0` from strictly
below and never reaches it for any finite density (`algo::sigmoid`, pinned by its
own tests).

**This is a real improvement and a modest one.** A few percent of clipped highlights
is a defect worth removing; it is not a rescue. Anyone reading the 0.00% column as
"no highlight information is lost" should read the caveat below first.

### The 0.00% column has an asterisk

`io::encode` counts a clipped sample as `v > 1.0`, **strictly**. The sigmoid's
approach to white is asymptotic in ℝ but not in `f32`: at the shipped defaults with
the nominal 1.3 anchor, the rendered value is exactly `1.0f32` from just above
`D′ ≈ 3.1` onward. Arbitrarily different densities above that point therefore encode
to the identical 65535 with `clipped_high = 0` and `--strict` green.

The affected region is narrow — it takes near-opaque negative, i.e. deep highlights —
but the honest description of part of that 0.00% is that loss moved from a
**counted** category to an **uncounted** one, which is the opposite of what the clip
counter exists for. It does not change the direction of the result; it does mean the
clip counter is not a sufficient measure of highlight preservation under this curve.

### Read the means as a change-detector

A frame's mean conflates scene content with rendering, so it is not a quality score.
Portra160 1058 is a genuinely high-key frame, which is why it sits near white under
both versions. The clipping column is the one carrying the argument.

## Why each default moved

**`NOMINAL_DMAX` 2.0 → 1.3.** Every roll measured in this repo lands between 0.90
and 1.74, median ≈1.34 (Harman Phoenix 0.8976, Gold 200 1.2758, Ektar 1.2933,
Portra 160 1.3352 and 1.3816, Portra 400 1.4435 and 1.7383). `2.0` sat above *every*
measured roll. Since the exponential curve renders `10^(γ·(D′ − Dmax))`, an anchor
0.7 too high darkens the whole frame by that many decades: on Ektar 963 it rendered
**5.09× darker in linear terms** than the roll's own measured anchor (encoded means
0.104 vs 0.259). A default no real roll reaches is not conservative, it is wrong.

`1.3` is that median rounded to one decimal — a **nominal**, not a calibration.
Phoenix's 0.8976 is counted in, as the worst case showing where the floor of the
population is, not excluded to flatter the number; whether a calibrated constant
should exclude such stocks is `film-base/dmax-anchor-reliability`'s call. That task
is open precisely because the leader-measured anchor's *level* is uncontrolled (two
rolls of one stock 0.295 apart while their bases agree to 0.0005), and it still owns
the number. Measure per roll with `estimate --d-max-region` whenever accuracy
matters.

**Default curve exponential → sigmoid.** The exponential pins display white at
`Dmax` and has **no anchor placement**, so raising its contrast pivots the line
*around white* and drags everything below it down. `reports/sigmoid-reference-baseline.md`
measured that trade on user-confirmed shadow patches: contrast 2.0 takes the black
floor 72 → 12/255 but costs **2.75 EV of midtone placement**. The two knobs fight.
The sigmoid pins **mid-grey** instead, which removes the conflict, and its
contrast (≈2.07) and shoulder (0.6) are *derived* from manufacturer reference
densities rather than chosen.

`algo/sigmoid-parameter-calibration` remains open, so those values are
provisional. Shipping them as the default is a deliberate call: provisional and
derived beats a slope (1.0) that was never a rendering intent — it is the value at
which the S-curve degenerates bit-for-bit into the straight line, which suggests a
testability default rather than a photographic one.

**`ExponentialParams::gamma` 1.0 → 2.0.** For anyone who still selects the
exponential explicitly, 2.0 is the measured improvement above (floor 72 → 12/255).
It remains a *partial* fix, because that curve cannot place the floor and the
midtones at once — the residual 2.75 EV midtone offset stands until it gets a
mid-grey anchor ([`algo/exponential-mid-grey-anchor`](../tasks/algo/exponential-mid-grey-anchor.md)).

## Consequences worth knowing

- **The sigmoid never exceeds 1.0.** That is why nothing is counted as clipped, and
  it also means `film-master` and `hdr-linear-tiff` carry no samples above display
  white under the default curve. Their integration tests now select the exponential
  explicitly, because their subject is the *container* (unclamped float, no transfer
  applied) and the default curve can no longer exercise it.
- **Single-rendition HDR presets now warn when they carry an SDR-range signal —
  and this condition is older than v2.** Measured on the IR-free fixture:

  | default | brightest pixel | warns |
  |---|---|---|
  | v1 (exponential γ1.0, anchor 2.0) | **114 nits** | yes |
  | v2 (sigmoid, anchor 1.3) | **201 nits** | yes |
  | exponential at v2's γ2.0 / anchor 1.3 | above 203 | no |

  Both defaults sit below the 203-nit reference white while the report advertises
  `target_peak_nits: 1000`, so `hdr-pq`, `hdr-hlg`, `hdr-linear-tiff`,
  `hdr-pq-tiff` and `hdr-hlg-tiff` emit a `--strict`-promotable warning when the
  rendition never rises above reference white.

  **v2 did not cause this; it improved it** (114 → 201, still short). An earlier
  draft of this report claimed the sigmoid dropped content light "from 1000 to
  201" — that 1000 came from the exponential at v2's *new* gamma and anchor, a
  configuration that was never a default, so the comparison was against something
  nobody shipped. The warning's value is that it surfaces a pre-existing defect,
  not that it guards a new one. Fixing the underlying cause — content never
  reaching the display stage's shoulder — belongs to `output/presets`.
- **HDR headroom at defaults is unchanged by this PR, and still absent.** The
  `ultra-hdr-v1` gain map measures `GainMapMax` ≈ 1.0027× under *both* the old and
  new curves on a real frame — content sits below the shared display stage's
  shoulder knee either way. That is `output/presets` / the display-stage work, not
  this change, and it is why the warning above is scoped to single-rendition presets.
- **Auto white balance is a weaker corrector for a *wrong* base under the
  sigmoid.** A wrong base leaves a constant per-channel density offset;
  `10^(γ·(D′ − Dmax))` turns that into a constant per-channel *factor*, which a
  stage-4 gain cancels exactly. The sigmoid is nonlinear in the same domain, so no
  post-curve gain fully cancels it (measured on the synthetic cast fixture:
  channels land 0.0595 / 0.0616 / 0.0685 instead of equal). The estimator is
  unchanged, and the effect does not arise when the base is right.

## Reproducing

```sh
cargo build --release
python3 scripts/render-defaults-v2/measure.py
```

The script prints the table above. It needs the real scans at `../nc-assets` (the
machine-local symlink described in CLAUDE.md) and reads derived numbers only.

The frames and the frozen per-roll film bases it uses, so the runs can be repeated
by hand:

| roll | frames | `--film-base` |
|---|---|---|
| Ektar | `rolls/Ektar/20260713-nikon-963.tif`, `…-971.tif` | `0.51679254,0.2768597,0.18973067` |
| Portra 160 | `rolls/Portra160/20260720-nikon-1058.tif`, `…-1065.tif` | `0.5340505,0.26347753,0.15655756` |

and the two invocations, in full:

```sh
# v2 — the shipped default render, which by definition takes no conversion flags
nc convert <frame> -o out.tif --film-base <r,g,b>

# v1 — all three moved defaults restored together; restoring only two measures a
# render that never shipped
nc convert <frame> -o out.tif --film-base <r,g,b> \
  --density-curve exponential --density-gamma 1.0 --d-max 2.0
```

Both print `output_stats.mean` and `loss.clipped_high` in the JSON report. Check
`dmax` in the report to confirm the v1 run really resolved 2.0 — that is the field
that would have caught the dropped flag described at the top.
