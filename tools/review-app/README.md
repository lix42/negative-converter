# Visual review app

Compare rendered configurations of the same frames **in place**: every rendition
sits in one grid cell, so switching config swaps the picture without moving it by
a pixel. That is the whole point — toggling in place shows differences that
side-by-side hides, especially in highlights.

Built with [Vite+](https://viteplus.dev) (`vp`), Solid, and StyleX.

## Run it

```sh
corepack enable pnpm   # once per machine
pnpm install
pnpm dev
```

Then open the committed example:

<http://localhost:5173/?data=examples/synthetic/review.json>

To review a real set, serve the directory holding it and point `?data=` at the
`review.json`; image paths inside resolve next to that file:

```sh
pnpm build                          # → dist/
cp -R dist /path/to/review-app      # beside your review sets
cd /path/to && npx serve .
# http://localhost:3000/review-app/?data=../my-set/review.json
```

A `file://` page cannot fetch the JSON or its images — serve the directory.

The format is documented in [SCHEMA.md](SCHEMA.md).

## Using it

| Control            | Does                                   |
| ------------------ | -------------------------------------- |
| `1`–`9`, `0`       | Select config 1–10                     |
| `f`                | Toggle fit / fullsize                  |
| Config buttons     | Same as the number keys                |
| Preview thumbnails | Select that config for **every** image |

`fit` scales each image into the column; `fullsize` shows it at natural size and
the viewport scrolls.

## Gates

```sh
pnpm check         # vp check — format, lint, type-check (~1s)
pnpm test          # vp test run
pnpm build         # vp build
pnpm verify        # all three, in order
pnpm fix           # vp check --fix
```

CI runs the same three. `pnpm fix` applies oxfmt formatting and lint autofixes.

Package management is **pnpm**, pinned by `packageManager` in `package.json`;
`corepack` provisions it, and Vite+ downloads a matching version by itself when
you run `vp install`. Run `vp` built-ins directly (`vp check`); use `vpr <name>`
if you ever need the npm _script_ of the same name.

## Known limits

- **Every rendition of every image is fetched eagerly.** The stacking that makes
  switching instant requires the inactive renditions to be laid out, and
  `loading="lazy"` on them would collapse a section to zero height whenever a set
  omits `width`/`height` — which the schema permits. So a large set (many frames x
  many configs of full-size JPEGs) downloads everything up front. Fixing it
  properly means making dimensions mandatory, which is the generator's job when
  that half is built. Downsize the images meanwhile; the previews reuse them.

## Notes for the next person

- **`vp migrate --full` writes to `CLAUDE.md`/`AGENTS.md`.** Its `--agent` step
  rewrites coding-agent instructions, which in this repo would clobber the
  project's own. Configure lint in `vite.config.ts` by hand instead.
- **The StyleX plugin is excluded under test, deliberately.** It holds a handle
  that keeps Vitest from exiting: 10.9s with it, 0.9s without, measured. The unit
  tests cover pure modules that import no styles, so the compiler has nothing to
  do there. Putting it back means budgeting for the hang.
- **`vp build` warns `Unknown at rule: @stylex` on every build.** That is
  lightningcss meeting StyleX's CSS entrypoint directive, which StyleX leaves in
  place after appending the compiled rules. Browsers ignore an unknown at-rule,
  so it is cosmetic. Switching to esbuild's CSS minifier silences it but requires
  adding `esbuild` as a dependency, since Vite 8 ships rolldown instead — not
  worth a dependency for a cosmetic warning.
- **Only one Vite may exist in the tree.** `vite` is aliased to Vite+'s core, so
  the plugins that import `vite` get the build Vite+ actually runs. The
  `overrides` block says so, but npm reads that field and **pnpm does not** (and
  pnpm 11 dropped the `pnpm` field too — settings moved to
  `pnpm-workspace.yaml`). Since this project runs on pnpm, the guarantee is a
  test instead: `src/toolchain.test.ts`.
- **`stylex.props()` returns React's `className`, and spreading it is not
  reactive.** Solid's JSX ignores `className`, so styles vanish silently; and
  `{...stylex.props(a, cond && b)}` is evaluated once, freezing a conditional
  style at first render. Everything goes through `cls()` in `src/cls.ts` and is
  applied as `class={cls(...)}`, which Solid tracks like any attribute.
- **StyleX silently drops shorthands it does not model.** `background` and
  `border` never reached the stylesheet — measured in the browser, where the
  selected button was white text on the browser's default button face. Use
  longhands (`backgroundColor`, `borderWidth`/`Style`/`Color`,
  `gridRowStart`/`gridColumnStart`). Nothing warns you.
- **A scroll handler must never write a signal here.** Scroll → signal → re-render
  → layout change → measure → scroll geometry is a cycle, and it wedged the
  renderer so hard that Chrome could not inject a script into the page. The
  mini-map is therefore painted imperatively (`paintMap`), and only coarse
  "does it overflow at all" state is reactive.
- **Correctness must not depend on `requestAnimationFrame`.** A hidden or
  backgrounded tab never fires it, and `scrollTo({behavior:'smooth'})` is driven
  by the same loop. Both bit here: the pan controls stayed permanently absent and
  pan clicks were silently dropped. Measure synchronously in the effect (Solid
  runs effects after the DOM updates) and fall back to an instant scroll when the
  document is hidden.
