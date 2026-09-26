# Visual review app

Compare rendered configurations of the same frames **in place**: every rendition
sits in one grid cell, so switching config swaps the picture without moving it by
a pixel. That is the whole point — toggling in place shows differences that
side-by-side hides, especially in highlights.

Each frame gets **one screen** — picture, preview strip and charts together — and
scrolling settles a frame at a time. Only the frames near the viewport render
their pictures at all, so a set of thirty-odd frames costs the same to open as a
set of three.

Built with [TanStack Start](https://tanstack.com/start) on
[Vite+](https://viteplus.dev) (`vp`), Solid, and Panda CSS. It is a **local** tool:
the server exists to read your review set off disk and watch it, and there is no
deployment story — `pnpm dev` is how you run it.

## Run it

```sh
corepack enable pnpm   # once per machine
pnpm install
pnpm dev ~/sets/display-tone/review.json
```

Then open <http://localhost:5173> — but **check the port it printed**. Another
`pnpm dev` already holding 5173 does not fail; Vite binds 5174 instead and the
old server goes on serving _its_ set at the address you were about to open. The
page states its own source path under the title, which is the thing to read when
a set looks wrong. The server reads the set from disk, so the
path may be anywhere — it does not have to sit beside the app, and there is no
URL to construct. A directory works too, meaning the `review.json` inside it:

```sh
pnpm dev ~/sets/display-tone
```

`pnpm dev <path>` is sugar over the real contract, the `REVIEW_SET` environment
variable, which the server reads once at startup:

```sh
REVIEW_SET=~/sets/display-tone/review.json pnpm dev
```

With neither, a bare `pnpm dev` renders the committed example under
`public/examples/synthetic/`, so the app runs out of the box.

Images are served from `/img/<id>`, addressed through a map built while the set
was parsed — only files the set actually named are reachable, and the id carries
the file's mtime so a re-render is a new URL rather than a cache problem.

The format is documented in [SCHEMA.md](SCHEMA.md). The usual way to _produce_ a
set is `nctool review generate`, which renders a matrix and writes each
rendition's measurement beside it — from the repo root, and with the spelling
that works (`nctool` is not installed, and measuring wants the venv):

```sh
PYTHONPATH=scripts/analysis .venv/bin/python -m nctool review generate \
  <matrix.json>
```

## Using it

| Control            | Does                                   |
| ------------------ | -------------------------------------- |
| `1`–`9`, `0`       | Select config 1–10                     |
| `f`                | Toggle fit / fullsize                  |
| `j` / `k`          | Step to the next / previous frame      |
| `h` / `l`          | Step to the previous / next config     |
| `a`                | Note on the current frame              |
| `n`                | Every note and patch, with Copy all    |
| `c`                | Clear notes and patches (asks first)   |
| `p`                | Patch mode — draw and label regions    |
| `i`                | Colour mode — read the pixel under it  |
| `m`                | Show / hide the charts row             |
| Config buttons     | Same as the number keys                |
| Preview thumbnails | Select that config for **every** image |

The **current frame** — the one whose title is underlined — is the lowest frame
whose picture is on screen. Lowest, because scrolling down reveals the next
picture at the bottom of the screen and that is the frame you are moving to, so
it takes the mark as soon as it appears.

`j` and `k` step it, aligning the frame they land on. **An unaligned frame is
aligned first**: after a free scroll the first press settles what you are already
looking at rather than skipping past it, in either direction.

`h` and `l` step the selected config instead, and **wrap** where `j`/`k` clamp.
That difference follows the thing being stepped: the configs are a handful of
renderings of one frame, cycled repeatedly to see what moves, so running off one
end and back on is the gesture — while frames are a long list you work down, and
wrapping from the last to the first would lose your place.

`fit` **contains** the whole frame in the space left over — the entire picture is
visible whatever its aspect, letterboxed against the panel, and that pane never
scrolls. `fullsize` shows it at natural size and the pane scrolls, with the pan
controls and mini-map. Note `fit` is bounded by height as well as width now, so
raising `sizes.chartsBand` costs picture size in both directions.

A frame is exactly one screen tall, and scrolling **snaps** to a frame — by
proximity, so a flick settles on one but a small drag leaves you where you put
it, and scrolling _inside_ a `fullsize` picture is not fought. The page header is
a snap stop of its own, so the title and the set's path stay reachable at the top.

Within that screen the picture takes every pixel the head and the charts leave.
When there is not enough for both, the **charts** are what gives way: they are a
summary of numbers held elsewhere, while the picture is the thing you came for.
`m` says the same thing by hand: it puts the charts away for every frame at once
and hands their band to the picture, which is how you get the most picture a
screen can hold without leaving the app.

The page follows the set while it is open. Re-run `nc` over the same directory
and the renditions that changed swap in place — the selected config and the
scroll position stay put, so you can keep toggling while renders land. Editing
`review.json` works the same way: a config added to it simply appears. A
re-measurement lands the same way, because the records are watched too.

## Notes

Reviewing a roll usually means writing down what you saw. `a` opens a note on the
current frame, `n` opens every note at once — one textarea per frame, editable
there too — and **Copy all** puts the lot on the clipboard as one block, in set
order, listing only the frames actually noted:

```text
# Review notes — Ektar roll

2 of 14 frames noted

## F0005 — synthetic frame 5

Highlights clip on this one.
```

A frame that has a note says so beside its title, so you can see what you have
already covered while scrolling. `c` clears every note and **asks first** — notes
live only in the page, so there is nothing to restore them from. (`c` on a review
with no notes does nothing rather than asking.)

**Notes are frontend-only**: no server, no storage, gone on reload. They are meant
to be collected in one go and pasted somewhere that keeps them, which is what
Copy all is for.

They are dropped automatically when the set's **path or frame list** changes,
because a note then describes something that is no longer there. Deliberately
_not_ on every refresh: re-running `nc` over the same frames and watching them
update in place is the workflow this app exists for, and wiping the notes each
time would make them useless.

The same change scrolls the page back to the top, for the same reason: the scroll
position means a frame, and once the frame list has moved the offset you were
parked at lands on some other frame — or, on a shorter set, past the end, where
the browser leaves you at the bottom with no way to tell what you are looking
at.

## Patches

`p` turns the picture into a drawing surface: drag a rectangle, give it a label —
"cloud", "white shirt", "the shadow under the bridge" — and it is marked. A
second `p` puts it away. Hovering a patch shows its delete button; there is no
edit, because a patch is a rectangle and a few words, and correcting one is
quicker to redraw than to edit.

The label sits **inside** the rectangle's top-left corner, unconditionally.
Drawn above it, the scrolling viewport clips it away as soon as the patch
reaches the top — which in `fullsize` is every patch in turn, as you scroll. It
costs a corner, which is free: patches are drawn only in patch mode, never while
a picture is being judged.

**A patch belongs to the frame, not to the config.** Every rendition of a frame
is the same subject rendered differently, so one rectangle sits on the same cloud
in all of them — which is what makes a patch useful for comparing variants, and
is the same in-place principle the stage is built on. The coordinates are
normalised to the image, so a patch survives a config switch, `fit`/`fullsize`
and a window resize without moving off its subject. Verified: the same patch
reports an identical position to four decimal places in both zoom modes.

They are listed in the `n` dialog and go out with **Copy all**, as percentages of
the frame:

```text
## F0005 — synthetic frame 5

Highlights clip on this one.

Patches:
- "white shirt" — x 31.2% y 12.4% w 18.0% h 9.1%
```

Percentages rather than pixels because the renditions of one frame need not share
a pixel size — a percentage is the one spelling true of all of them. The labels
are the point: a reader who cannot see your screen learns _where to look_, which
is the thing a note alone cannot say.

Patches live only in the page, exactly like notes, and are cleared by the same
rule — a different set, or one whose frame list changed. `c` clears both.

## Colour

`i` reads the pixel under the pointer: the HEX, a phrase for it, the RGB triple,
L\* and chroma. Clicking copies the HEX. **It re-reads whenever the picture
moves under the pointer, not only when the pointer moves** — turning the mode on,
scrolling the page, stepping a frame with `j`/`k`, panning inside a `fullsize`
picture, and switching config. That last one is the whole comparison: the pixel
does not move at all, but what is under it is a different rendering, and a
reading taken only on `pointermove` would go on describing the previous config.
Move the picture off the pointer entirely and the readout clears rather than
going stale. It works in both zoom modes, and in
`fit` it reports the colour the screen is showing — one screen pixel there covers
several image pixels, so the sampler downscales exactly that span to one, the
same reduction the browser performed to draw it. **`devicePixelRatio` is part of
that span**: a raster image is rasterised at device resolution, so on a 2x
display the screen shows twice as many samples across as there are CSS pixels,
and dividing by CSS pixels alone would average a box twice as wide as the one
drawn — a 4x error in area, visible wherever the spot has detail. In `fullsize`
on an ordinary display the span is one pixel and the reading is the file's own.

**The values are sRGB as displayed.** Renditions carry ICC profiles, and the
sampler reads through a plain sRGB canvas, so the browser has already converted:
the HEX matches the screen rather than the bytes nc wrote, and a Display P3
rendition's out-of-sRGB colours read clipped. That is deliberate — the readout
answers "what colour am I looking at".

The phrase is coarse on purpose, with one band that carries the weight. The
bands are whole code values off an 8-bit sampler, so they are stated as the
integer spans they are rather than as "up to": a channel span of **0–3** is
named grey with **no** hue, because a direction read off grain would be worse
than none; **4–22** is named on the grey scale _and_ told which way it leans —
"near-white, slightly yellow", "dark grey, slightly blue" — because a faint cast
is exactly what an eye cannot name, and it is what this was asked for; **23 and
up** is a colour in its own right.

That band is measured in **channel span, not HSL saturation**, and the difference
is not cosmetic: `s` divides by how much room a colour of that lightness has to
be saturated, so the same lean reads as 4% on a light grey and 10% on a dark one.
Measured on this app's own example, one cutoff in `s` could not catch both
without also naming single-code-value noise on a near-white.

The lightness _word_ comes from L\*, the same number printed beside it — not from
HSL's `l`. `#A7A7A7` is `l` 0.65 and L\* 68.5, which sit on opposite sides of
"light grey", so a chip could otherwise say "grey" on one line and 68.5 on the
next.

## Measurements

When a rendition names a metric record (`metrics` in SCHEMA.md), the three charts
appear under the picture and swap with the config exactly as the picture does: a
tone histogram, the same histogram split into red, green and blue, and colour
cast across the tone bands. They sit in **one row of three equal columns** and share one `viewBox`, so
they render at identical size and never wrap — the three are read together, and
comparing a shape against one below it is not the same gesture as comparing it
against one beside it. They describe the **measured region**, which a set usually insets so the
film holder stays out of the statistics — the panel says what share of the frame
that is.

A rendition with no record shows its picture and says it has no measurement; a
record that will not parse says why, in place of the charts. Neither refuses the
set.

**The charts lag the picture by design.** Switching config swaps the picture at
once and the panel follows about 200ms later, once the new picture has loaded and
the selection has been still — so toggling two configs to compare them draws no
charts at all until you stop. The panel is fed the config it is describing
throughout, label included, so it never labels one config's numbers with
another's; during the lag it simply still names the previous one.

**Nothing checks that a record describes the pixels beside it.** Re-run `nc` by
hand over a set and the picture updates while the charts go on describing the
previous render, silently. `nctool review generate` avoids it by re-measuring
whenever the image's checksum changes, which is why it is the way to rebuild a
set.

`/charts` renders the same panel from the committed synthetic fixture, with the
degenerate cases a real record rarely carries at once — a sparse band, a band
with no pixels, a channel running past the top of the range. It needs no review
set, no assets and no venv.

## Thumbnails

The preview strip shows the same file as the stage, in a 104x70 box. On a set of
real scans that file is the scan — 5184x3600, ~7 MB — so a frame coming into
view used to ask the browser to decode **six 18.7 Mpx photographs** to paint six
thumbnails. Measured on a 43-frame, 6-config set, one `j` press cost **832-888
ms**, of which ~1 ms was script: the rest was a single RasterTask full of
`Decode Image`, i.e. presentation delay. Stepping _back_ onto frames mounted
seconds earlier cost 712 ms, because an 18.7 Mpx image is ~75 MB decoded and the
browser's decode cache evicts it long before you return.

So the server shrinks them: `/img/<id>?w=` resizes with libvips (sharp), caches
the result under the OS temp dir keyed by the source's path, mtime and size — plus
the width and the pipeline version below — and the page's `preview` URLs carry the
`?w=`. **Only the strip** — the stage always gets the file itself, because that is
the picture under review. The same press
now costs **24-80 ms** warm, 208 ms the first time a frame is seen (70 ms to
generate a thumbnail, once per rendition, against ~6 KB served instead of 7 MB).

**The thumbnail keeps the source's ICC profile.** That is load-bearing, not an
implementation detail: an untagged JPEG is read as sRGB, so a Display P3
rendition served without its profile shows visibly more saturated in the strip
than on the stage next to it — in a tool whose whole purpose is judging colour by
eye. The chain also applies EXIF `Orientation` and flattens alpha onto white, for
the same reason: the strip must show the picture the stage shows.

**So bump `PIPELINE_VERSION` when you change that chain.** It is in the cache key
_and_ in every preview URL (`&t=`), and it has to be in both: an entry on disk is
keyed by the source, and the response is `immutable` for a year, so with the
source unchanged neither the server nor an already-loaded browser would otherwise
notice that the chain now produces different bytes. This is not theoretical — the
profile-less first version was still being served from a browser cache after the
fix landed.

**Nothing prunes the cache, deliberately.** An entry is keyed by the source's
mtime and size, so re-rendering a frame orphans its old thumbnail rather than
replacing it — a full re-render of a 43x6 set leaves ~1.5 MB behind. It sits in
the OS temp dir because that is the directory whose contents the system reclaims
for you: macOS runs `dirhelper` at load and daily at 03:35, with
`CLEAN_FILES_OLDER_THAN_DAYS = 3` — which age it reads was not verified here, so
do not assume an entry you keep opening is kept. The directory name carries the
uid, because `tmpdir()` is `/tmp` on Linux and a directory shared with another
user is one this server can never write to.
If you ever do add a sweep, do **not** prune entries that are not in the current
set: two servers on two sets run side by side routinely, and each would wipe the
other's cache. Age plus a size cap is the safe form.

Two things worth knowing if you touch this:

- **Neither browser-side fix works, and both were measured.** `decoding="async"`
  leaves the decode on the raster path (872 ms), and pre-decoding the neighbours
  the overscan has already fetched does nothing (872 ms) — the cache cannot hold
  images this size. Shrinking the bytes is the only lever.
- **The residual is the stage picture, not the strip.** With previews hidden
  entirely the same presses still cost 16-176 ms, so pre-generating every
  thumbnail at startup would buy ~25 ms on a first visit and was declined.
  Serving a display-sized stage image in `fit` would flatten it — at the price of
  reviewing a server-resampled picture, which is a different decision.

`sharp` is the app's one native dependency. Its binaries are prebuilt per
platform (`optionalDependencies`), so it needs no compiler and no `allowBuilds`
entry — unlike `esbuild`, it has no install script. `--frozen-lockfile` resolves
the linux-x64 entry the lockfile carries, which is exactly why the chain's tests
**do** decode, resize and re-encode in CI rather than skipping: a change to the
sharp chain can turn CI red, which is the point. The skip path exists for a
platform with no prebuilt binary, and there nothing is fatal either — the import
is inside the call, so the server starts and serves originals.

**The strip is not downscaled in four cases**, all of which serve the file
itself: a format off the thumbnailable list (an SVG, which the browser draws at
any size for nothing, or a GIF); an extension nothing recognises, which is never
mangled into a JPEG; a file whose header says to leave it alone — **animated**,
because a strip showing frame one beside a moving picture is worse than a large
one, or **already inside the 208px box**, where the chain would re-encode it
lossily (sometimes _larger_) for no change in size; and a generation the libvips
build declined.

Only the last is a statement about an _attempt_ rather than about the file, and
that distinction lives in the type (`ThumbnailResult`) rather than in a boolean,
because it decides how long the fallback may be cached: a decline gets a short
lifetime, since fd exhaustion or a read-only temp dir can clear, while the others
are cached as long as the file itself. Both are remembered per process, so each
costs one probe — and a decline one warning — not one per view.

**Alpha is flattened onto white**, which is a compromise rather than a right
answer: JPEG has nowhere to put it, the page composites the stage over its own
panel colour, and that colour is the viewer's theme, which the server cannot
know. A transparent rendition therefore reads white in the strip and
theme-coloured on the stage. `nc` writes no such rendition; if one ever matters,
the fix is an alpha-capable thumbnail format, not a guess at the background.

## Gates

```sh
pnpm check         # panda codegen, then vp check — format, lint, type-check (~3s)
pnpm test          # vp test run
pnpm build         # panda codegen, then vp build
pnpm verify        # all three, in order
pnpm fix           # panda codegen, then vp check --fix
```

CI runs the same three, on Linux only. `pnpm fix` applies oxfmt formatting and
lint autofixes.

`vp build` is a **compile check**, not a deployment step: nothing is served from
`.output/`, and there is no `pnpm start`. `src/routeTree.gen.ts` is generated by
TanStack's route generator on every dev and build run, and is committed — `vp
check` runs before anything generates it, so a fresh clone would not type-check
without it. Add a route file and the next `pnpm build` regenerates it.

Package management is **pnpm**, pinned by `packageManager` in `package.json`;
`corepack` provisions it, and Vite+ downloads a matching version by itself when
you run `vp install`. Run `vp` built-ins directly (`vp check`); use `vpr <name>`
if you ever need the npm _script_ of the same name.

## Known limits

- **A rendition outside the set directory is only watched if its directory
  exists when the watch starts.** Renditions inside the set are covered by a
  recursive watch, so a file appearing later starts working; an _outside_
  directory that does not exist yet cannot be watched, and the alternative —
  watching the nearest existing ancestor — could mean recursively watching a
  home directory. Such a rendition shows its gap until the page is reloaded.
  Declined deliberately: the page shows a visible gap, not a wrong picture.

- **A dev-server restart reloads the page**, losing the selected config and
  scroll position. Ordinary edits to a set never do this — they update in place.

- **A frame more than one screen away is unmounted, and its pan position goes
  with it.** Scroll two frames past one you had panned into in `fullsize`, come
  back, and it is at the top-left again. The neighbours stay mounted, so this
  costs nothing while you are toggling between adjacent frames; retaining the
  offset would mean restoring it after the images decode, since the stage cannot
  scroll to an offset it is not yet tall enough to hold.

- **The charts are drawn well below their natural size, and the band is what
  decides that.** `sizes.chartsBand` is a stated 280px — applied only when charts
  are actually drawn, since a set rendered with `--no-metrics` would otherwise
  hold the band open for one line of "not measured" and take ~220px of picture
  from every frame — of which a fixed 121px
  goes to the panel head and each card's title, gaps, padding and caption — so
  the chart gets 159px against the 300px its `viewBox` describes, and the three
  share that `viewBox`, so a shorter slot letterboxes them rather than distorting
  them. Measured: 0.53 scale at any viewport from 1570px down to about 845px, the
  same on every screen by design. Below that the band can no longer coexist with
  `sizes.pictureFloor` and starts shrinking — 195px at a 728px viewport, 0.36
  scale, where the 10px tick labels stop being readable; below 820px the captions
  drop out **inside a frame** so the drawing keeps what they were using. `/charts`
  keeps them at any height — the rule is scoped to the band, not the viewport.

  Raising `chartsBand` is the one-line way to trade picture height for chart
  legibility. Making them fill their slot instead would mean measuring each one
  and setting its `viewBox` from the measurement — cheap, but it puts a
  `ResizeObserver` write in the path of the layout it measures, which is the
  cycle this app has been bitten by before. `/charts` is uncapped and always
  draws them at full size.

- **The server could measure `width`/`height` itself**, which would retire the
  manual fields. It does not yet; the schema is unchanged. Note this is no longer
  what gates lazy loading — that limit closed instead by making a frame's shell
  one screen tall whether or not its contents are mounted, so no image dimension
  is needed to reserve the right space.

## Notes for the next person

- **`shellComponent` renders on the server only.** A `<link rel="stylesheet">`
  written into it names an asset the client build never emits, and a
  browser-side `import()` from it is tree-shaken out of the client bundle
  entirely — measured: zero occurrences, no hydration markers, and a page that
  404s its own stylesheet. Stylesheets go through the root route's `head.links`;
  anything that must run in the browser lives inside the routed tree, which is
  why `LiveReload` renders from the index route.
- **Styles need no plugin wiring.** Panda runs as a PostCSS plugin
  (`postcss.config.cjs`), and Vite pipes every stylesheet through PostCSS in dev
  and in a build alike — so `src/index.css` is the whole stylesheet in both
  modes, linked once from `head.links`, with no dev-only virtual half to keep in
  step. Verified in dev: `curl -H 'Accept: text/css' localhost:5173/src/index.css`
  returns the generated rules, and editing a `css()` call updates it in place.
- **A route file's `server.handlers` is stripped from the client bundle**, which
  is what lets `src/routes/img.$id.ts` import `node:fs` at all. `createServerFn`
  does the same for its handler body. Worth re-checking after a dependency bump:
  `grep -o "node:fs" dist/client/assets/*.js` must find nothing.
- **Start's serializable check refuses a `ReadonlyMap`.** It tests
  `T extends Map<any, any>`, which the readonly form fails, so anything a server
  function returns has to avoid it — `renditions` is a plain record for this
  reason as much as for modelling.
- **A rendition's mtime is frozen into the asset map at parse time, and that
  mtime _is_ its `/img/` URL.** So the held set must be dropped whenever any
  rendition changes, not only when `review.json` does — otherwise a re-rendered
  frame keeps its old URL, the browser serves it from an `immutable` cache
  entry, and the page shows the previous render with nothing reporting a
  problem. Review found this shipped; every manual check had missed it because
  they all used `pnpm dev <directory>`, and the cache key compared the _stated_
  path against the loaded set's _resolved_ one, so the directory form never hit
  the cache at all and accidentally looked correct. When testing live refresh,
  use the **file** form — it is the one the docs tell people to use, and the one
  with a cache to get wrong.
- **Live refresh must not lean on `EventSource` reconnecting.** The stream from
  `/events` is the fast path — a re-render changes a file's mtime, which changes
  its `/img/` URL, which is what makes the browser fetch the new picture. But
  when the dev server is replaced, the stream does not reliably come back:
  measured, neither its built-in retry nor a hand-rolled replacement recovered,
  the `changed` event stopped arriving, and `router.invalidate()` then issued no
  request at all. The page just went on showing the previous render with no
  error anywhere — indistinguishable from a re-render that changed nothing,
  which is the one wrong answer this tool must never give. Recovery therefore
  rests on a plain `fetch` poll of `/alive`: a different boot id means a
  different server, and the page reloads. Do not "simplify" that away.
- **`vp migrate --full` writes to `CLAUDE.md`/`AGENTS.md`.** Its `--agent` step
  rewrites coding-agent instructions, which in this repo would clobber the
  project's own. Configure lint in `vite.config.ts` by hand instead.
- **`styled-system/` is generated and gitignored**, so every script that needs it
  runs `panda codegen` first (0.6s). `prepare` alone is not enough: pnpm skips
  lifecycle scripts when the lockfile is already satisfied, so deleting the
  directory and re-installing leaves it missing and the build fails on an import
  that no source change explains. `src/routeTree.gen.ts` is committed instead —
  Panda's output is 1.6 MB across 70 files, which is not.
- **pnpm 11 must be told about esbuild's install script.** Panda bundles
  `panda.config.ts` with esbuild; left undecided, pnpm writes an `allowBuilds`
  placeholder into `pnpm-workspace.yaml` and **exits 1** — including on the
  `pnpm install` that `vp check` runs for itself, so the gate fails before it
  starts. `allowBuilds: {esbuild: true}` is the decision.
- **`presets` is `['@pandacss/preset-base']` only, and that is load-bearing for
  `strictTokens`.** Panda's default is two presets doing unrelated jobs:
  `preset-base` is the machinery (357 utilities, 107 conditions including
  `_osLight`, the patterns) and carries **no tokens**; `preset-panda` is _only_
  token ladders — 246 colours and rem-valued spacing/size/font scales, 422 tokens
  in all. With `strictTokens` on, leaving `preset-panda` in would put 422
  valid-but-meaningless entries behind every autocomplete and emit them all as
  `:root` custom properties, which is the opposite of what the flag is for.
  Dropping it also took the stylesheet from 16.6 kB to 6.9 kB (5.2 → 2.1 kB
  gzipped) and the token block to **52** custom properties — the app's actual
  vocabulary, and exactly what the theme declares.
- **Only one Vite may exist in the tree.** `vite` is aliased to Vite+'s core, so
  the plugins that import `vite` get the build Vite+ actually runs. The
  `overrides` block says so, but npm reads that field and **pnpm does not** (and
  pnpm 11 dropped the `pnpm` field too — settings moved to
  `pnpm-workspace.yaml`). Since this project runs on pnpm, the guarantee is a
  test instead: `src/toolchain.test.ts`.
- **`strictTokens` and `strictPropertyValues` are both on, so the theme in
  `panda.config.ts` is exhaustive.** A measurement that is not a token there is a
  type error at the call site; adding one is a deliberate edit to that file. Note
  the coverage is Panda's, not ours — a property is checked only if its utility
  declares a token category, which is why `borderWidth`, `zIndex`, `opacity` and the
  SVG geometry properties (`strokeWidth`, `strokeDasharray`, `fillOpacity`)
  still take raw values. `strictPropertyValues` was free: it found nothing,
  every enum-valued property already naming a real CSS keyword.
- **What `strictTokens` is protecting you from, measured before it was on:** a
  bare number is a _token lookup_, not pixels. `gap: 16` compiled to
  `var(--spacing-16)` — `4rem`, four times too big — because the old preset had a
  spacing token named `16`, while `padding: 18` compiled to `18px` because it had
  no token named `18`. Same syntax, opposite meanings, decided by the preset and
  silent either way. That class of bug is now a compile error.
- **Token naming follows one rule.** A value that denotes a _specific thing_ gets
  a role name — `sizes.thumbWidth`, `sizes.frameHeight`, `fontSizes.key` — and
  that is what retired the literals this app used to repeat across files (104x70
  in three places). `spacing` gets no such names because it has no such
  structure: the same 8px is a gap here, an inset there and a padding elsewhere,
  so a semantic name would be fiction. It is named by its measurement
  (`spacing["8px"]`), which keeps call sites reading like CSS while `strictTokens`
  still gates the set.
- **A token name must not mean two things across categories.** `panel` was both a
  colour and a size, so `backgroundColor: "panel"` was a surface and
  `maxWidth: "panel"` a column width — precisely the confusion naming tokens is
  meant to remove. It is `panelMeasure` now. The one name deliberately shared is
  `body` (a font, a font size and a line height): there, all three do mean "the
  body's".
- **No `[escape hatch]` values are used, and it is worth keeping that true.** The
  two that tempted were `width: auto` / `maxWidth: none` on `fullsize`, and
  `fontFamily`/`fontSize: inherit` on the buttons. The first pair became
  `sizes.natural` / `sizes.unconstrained` — "render at the size the file is" is a
  real decision this app makes, so it earns a name. The second became the `body`
  font and size tokens outright: stating what a control matches is better than
  inheriting it, and it is the same value today.
- **Styles are `css.raw()` objects merged at the call site.** `class={css(a, cond
&& b)}` — the call sits in a JSX attribute expression, so Solid tracks it, and
  `css()` accepts `false`/`undefined` for the inactive branch. Because the merge
  happens on the _objects_ and not on class strings, an override replaces the
  base's declaration outright rather than competing with it in the cascade.
- **The charts must never be redrawn synchronously with the picture, and two
  separate things guarantee it.** Measured on 14 frames x 6 configs: a config
  switch cost **~360ms** of synchronous work — one long task — of which the charts
  were 99% (the same set with its measurements stripped switched in 2ms).

  The first cause was that no derivation in either chart component was a
  `createMemo`. A plain `() => ...` recomputes on every read, and
  `histogramOutline`'s callbacks read the scales once per bin, so `y()` re-entered
  `ceiling()` -> `peak()` -> `drawn()` — a full scan of every series' bins — for
  each of ~220 points. Memoizing took the switch to ~10ms. The second is that even
  10ms does not belong in the keypress: `ImageSection` holds a _separate_ charted
  config id that advances only after the picture has loaded and the selection has
  been still for `CHART_SETTLE_MS`, which took it to **~2.8ms** and means rapid
  toggling draws no charts at all.

  **Memoizing changed the evaluation order, and that is the trap.** A `createMemo`
  body runs eagerly at creation where a plain arrow ran on first read, so a memo
  calling a `const` declared below it hits the temporal dead zone. `peak` calls
  `visibleBins`; memoizing in place turned that into a `ReferenceError` that
  blanked the whole page — with `check`, `test` and `build` all green, because it
  only fails when the component runs. The bin-geometry block is now declared above
  its readers, and it has to stay there.

- **A hydrated `<img>` fires no `load` event, and a client-created one has no
  `src` when its ref runs.** Both halves matter to the charts' wait-for-the-
  picture gate, and each one alone gets it wrong. An image that came down in the
  server-rendered HTML can finish before Solid attaches the listener, so it must
  be recognised in the ref or its charts never appear — deleting the ref check
  blanked every chart on first load. But Solid assigns `src` in an effect that
  runs _after_ the ref (verified in the compiled output: `addEventListener` ->
  ref -> `setAttribute("src")`), and per the HTML spec `complete` is `true` when
  there is no `src` — so testing `complete` alone marks every config loaded at
  mount and retires the gate silently. The check is
  `element.complete && element.currentSrc !== ""`; it needs both.

- **A percentage height on a grid item resolves against its grid _area_, and an
  implicit row is content-sized.** So `max-height: 100%` / `height: 100%` on a
  rendition resolves against an indefinite value and is dropped — silently, with
  every gate green. This shipped: `fit` had been `maxHeight: stageCap` (82vh, an
  absolute length that always resolved) and became `maxHeight: full` during the
  one-screen work, at which point the picture was width-limited only, overflowed
  its pane and put a second scrollbar inside the page's own. The fix is
  `stageFit`'s explicit `100%` tracks, which make the cell's height definite;
  `object-fit: contain` then does the scaling. If you touch those tracks, check
  the pane for a scrollbar in `fit` — that is the visible symptom, and nothing
  else reports it.

- **Keep the border longhands.** Not a Panda limitation — it models shorthands
  fine — but a consequence of the merge above: `previewActive` and `active`
  override only `borderColor`, and a base that spelled the whole border as one
  `border` shorthand would leave the override as a second declaration whose winner
  is decided by emission order. The `gridRowStart`/`gridColumnStart` longhands in
  `ImageSection` are _not_ that: nothing overrides them and `gridArea: "1 / 1"`
  would work now. They are inherited spelling from StyleX, which dropped
  `gridArea` silently.
- **Colours live in `panda.config.ts`, not in the stylesheet.** Every one is a
  semantic token whose `base` value is the dark palette and whose `_osLight`
  override is the light one — the same shape the hand-written custom properties
  had (dark by default, light under `prefers-color-scheme: light`), except that
  `color: "fg.dim"` is now type-checked and a typo fails `vp check`.
- **The pointer modes are exclusive by construction, not by checking.** `p` and
  `i` each take over the pointer on the picture, so a pair that could both be on
  is a state with no sensible behaviour. They are one `PointerMode` value rather
  than two booleans, which makes the illegal pair unrepresentable — the same
  reasoning the recipe structs in `nc` use for mutually exclusive knobs.

- **The patch overlay is anchored to the stage, at the picture's _painted_ box.**
  Both halves matter. Inside the stage, it scrolls with the picture in `fullsize`
  and needs no scroll handler at all — which is how it avoids the
  scroll-writes-a-signal cycle this app has been bitten by. At the painted box
  rather than the `<img>` element box, because `fit` letterboxes: on a portrait
  frame in a landscape pane most of the element is empty, and a patch measured
  against it would sit nowhere near the subject. `geometry.ts` owns that
  arithmetic and is tested; the components only place what it returns.

  The **colour readout is the exception** and is rendered in the pane instead. In
  `fullsize` the overlay is far larger than the window, so a chip placed there
  could sit off screen; the pane is the picture's fixed frame, which is the box
  the chip has to stay inside. Its size is _stated_ in tokens rather than
  measured, because the flip arithmetic needs it before layout — measuring would
  put a read of the element in the path of the write that positions it.

- **The colour readout re-reads from a remembered pointer position, because the
  browser will not tell you where the pointer is.** `pointer.ts` keeps the last
  seen position in a **plain variable, not a signal** — nothing should re-render
  because the pointer moved a pixel — and the overlay reads it when one of the
  triggers fires. The scroll trigger is one `capture: true` listener on `window`,
  which catches the page scrolling _and_ the pane scrolling inside a `fullsize`
  picture, since scroll events do not bubble. It is called straight from the
  handler rather than through `requestAnimationFrame`, on the same reasoning as
  `paintMap`: the browser already coalesces scroll to about one event per frame,
  rAF does not run in a backgrounded tab, and this cannot feed itself because the
  readout is absolutely positioned and changes no layout. Measured at **0.18 ms
  per scroll event** with three frames mounted, so it needs no throttling.

  **Which overlay owns a point is decided by `elementFromPoint`, not by a rect
  test.** In `fullsize` the overlay is far larger than the pane that clips it, so
  a rect test would let a frame claim a pointer that is over its neighbour — and
  several frames are mounted at once, so more than one would claim it.

- **`Show` does not recreate its children when the condition changes but stays
  truthy, so `onMount` is not a substitute for a dependency.** The overlay is
  shown for _either_ pointer mode, so going patch → colour keeps `when` truthy,
  the component is never rebuilt, and the `onMount` that takes the first colour
  reading never runs again — while the frame has meanwhile cleared the previous
  one. The readout stayed blank until the pointer was jogged. `off → colour`
  worked throughout, which is exactly why it survived testing: the trap is only
  on the mode-to-mode edge. The mode is now a dependency of the resample effect.

- **A control hidden until hover needs `opacity` _and_ `pointer-events`, and
  neither alone is enough.** A patch's delete button appears on hover, and the
  two obvious spellings each ship a bug. Bare `opacity: 0` leaves it taking
  clicks, so every patch carried an invisible delete control at its corner —
  confirmed with a real pointer. `visibility: hidden` fixes that and takes the
  button out of the tab order, which for the _only_ way to delete a patch, on an
  object with no edit operation, means a keyboard-only user can never correct a
  mis-drawn one. It is `opacity: 0` plus `pointer-events: none`, revealed by
  `:hover` and `:focus-within`: an `opacity: 0` element is still focusable.

- **Nothing in a `.tsx` file is tested, and that is a structural fact, not an
  omission.** `vite.config.ts` collects only `.test.ts` under `src/` and runs it
  with `environment: "node"` — `.tsx` is not matched, and there is no DOM to mount
  a component in. So every number a chart draws lives in a pure `.ts`
  (`src/charts/scale.ts`, `ramp.ts`, `metrics.ts`) with a sibling test, and the
  components map props to markup. `src/keys.ts` states the same rule; the mini-map's
  scale math in `ImageSection.tsx` is the counter-example, and is untested. Two chart
  bugs — a reference line whose condition could never be true, and a required prop
  nothing read — passed a green type-check and 90 tests because they were in a
  component.
- **In SVG, Panda gates `fill` and `stroke` but not the geometry.** Through `css()`
  those two are token-typed (`fill: Tokens["colors"]`), so chart chrome goes through
  `css()` and tracks the light/dark semantic tokens. SVG _presentation attributes_
  (`fill="…"`, `stroke="…"`) bypass Panda entirely — no `strictTokens`, and no theme
  switching — which is right for a computed data colour like a CIELAB ramp and wrong
  for anything the theme should own. `strokeWidth`, `strokeDasharray` and
  `fillOpacity` declare no token category and take raw values either way.
- **A scroll handler must never write a signal here.** Scroll → signal → re-render
  → layout change → measure → scroll geometry is a cycle, and it wedged the
  renderer so hard that Chrome could not inject a script into the page. The
  mini-map is therefore painted imperatively (`paintMap`), and only coarse
  "does it overflow at all" state is reactive.

- **The current-frame mark and the `j`/`k` keys read the same rule two different
  ways, deliberately.** `currentFrame` is the rule; the mark is driven by an
  `IntersectionObserver` on each picture pane, because it has to follow a free
  scroll, while the keys re-measure straight off the DOM at keypress time. An
  observer callback is asynchronous, so two quick presses of `j` would both act
  on the same stale answer and the second would scroll _backwards_. A keypress is
  not a scroll handler, so measuring inside one is free.

  **That measurement is only trustworthy because `align` scrolls instantly**, and
  a smooth scroll broke it in exactly the way the observer would have. Measured:
  aligned on frame 4, `k` twice in quick succession landed back on frame 4.
  Mid-animation both the frame being left and the one being entered have a
  picture on screen, so `currentFrame` answers with the _lower_ — the one you are
  leaving — and the second press aligned that. Landing immediately means every
  press measures a settled page. Do not restore `behavior: "smooth"` here without
  also tracking the in-flight target.

- **The mounting observer is the exception to that rule, and one invariant is
  what makes it safe.** `App` writes a signal from an `IntersectionObserver`
  callback, which is the same shape. It is safe only because a frame's shell is
  `frameHeight` tall **whether or not its contents are mounted** — so a render
  caused by that callback changes no geometry and cannot move a sibling across
  the viewport edge into the next callback. Break that invariant (give the shell
  `height: auto`, or let the charts push it past one screen) and the observer
  starts feeding itself. It is also what makes the scrollbar right on the first
  paint instead of growing as pictures arrive, and it retired the old limit where
  switching config resized every section above the viewport.

- **One number decides where a frame snaps, and JS reads it from the token.**
  `sizes.barHeight` is used three ways: `frameHeight` subtracts it,
  `scroll-padding-top` is it, and `App`'s `SNAP_LINE` reads it through Panda's
  `token()`. Measuring the bar instead would be a second source of truth for the
  same offset — and the failure is quiet: let the two diverge and the browser's
  own snapping pulls the page a few pixels after `align` lands, so `isAligned`
  is never true and `j` re-aligns the same frame forever instead of stepping.
  Reading the token also retires a `?? 0` fallback that would have aligned every
  frame _under_ the bar had the ref ever been unset.

- **The control bar must stay a single non-wrapping row.** `sizes.frameHeight` is
  `calc(100dvh - {sizes.barHeight})`, a stated constant — so a bar that wrapped on
  a set with many configs would make every frame the wrong height, with nothing
  reporting it. The config group scrolls sideways instead, and only that group:
  letting the whole bar scroll pushes `fit`/`fullsize` off the right edge.

- **`globalCss` is outside `strictTokens`, and an unresolved token name is
  emitted verbatim.** `scrollPaddingTop: "barHeight"` compiled to
  `scroll-padding-top: barHeight` — invalid CSS, silently dropped by the browser —
  because that property reads the `spacing` scale while `barHeight` is a `sizes`
  token. All four gates stayed green while every snapped frame sat with its label
  under the control bar. Inside `css()` this is a compile error; in `globalCss` it
  is not, so spell cross-category values as references: `"{sizes.barHeight}"`.
  Worth checking the emitted rule when adding one — `curl -H 'Accept: text/css'
localhost:5173/src/index.css`.

- **`flexBasis: zero` on the picture is load-bearing.** At the default `auto` a
  flex item's base size is its own content height, so the picture did not grow
  into the space left over — it started at its content height, overflowed the
  frame, and squeezed the charts to nothing. From a zero basis it takes exactly
  what the head and the charts leave, and `minHeight: pictureFloor` is then what
  it is _guaranteed_ rather than what it starts from. That floor is also what
  makes the charts the side that yields — **not** a `flex-shrink` difference, as
  an earlier draft of this note claimed: both items shrink at `1`, and the
  asymmetry is that the picture floors at `pictureFloor` while the band's
  `minHeight` is zero, so a frame too short for both takes it out of the charts.
- **Correctness must not depend on `requestAnimationFrame`.** A hidden or
  backgrounded tab never fires it, and `scrollTo({behavior:'smooth'})` is driven
  by the same loop. Both bit here: the pan controls stayed permanently absent and
  pan clicks were silently dropped. Measure synchronously in the effect (Solid
  runs effects after the DOM updates) and fall back to an instant scroll when the
  document is hidden.
