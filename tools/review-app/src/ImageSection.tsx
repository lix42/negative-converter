import * as stylex from "@stylexjs/stylex";
import { For, Show, createEffect, createSignal, on, onCleanup, onMount } from "solid-js";
import { cls } from "./cls";
import type { Rendition, ReviewConfig, ReviewImage, ZoomMode } from "./review";

const styles = stylex.create({
  section: { paddingBlock: 20, paddingInline: 16 },
  head: { display: "flex", gap: 12, alignItems: "baseline", marginBottom: 8 },
  label: { fontWeight: 600 },
  note: { color: "var(--accent)", fontVariantNumeric: "tabular-nums" },

  strip: { display: "flex", gap: 8, alignItems: "center", marginBottom: 10 },
  preview: {
    padding: 0,
    borderRadius: 6,
    borderWidth: 2,
    borderStyle: "solid",
    borderColor: "transparent",
    backgroundColor: "transparent",
    cursor: "pointer",
    lineHeight: 0,
  },
  previewActive: { borderColor: "var(--accent)" },
  previewImage: {
    width: 104,
    height: 70,
    objectFit: "cover",
    borderRadius: 4,
    display: "block",
  },
  previewMissing: {
    width: 104,
    height: 70,
    borderRadius: 4,
    backgroundColor: "var(--missing)",
    color: "var(--fg-dim)",
    fontSize: 11,
    display: "grid",
    alignItems: "center",
    justifyItems: "center",
    lineHeight: 1.4,
  },

  // The mini-map: where the fullsize viewport currently sits inside the image.
  mapOuter: {
    marginInlineStart: "auto",
    width: 104,
    height: 70,
    borderRadius: 4,
    borderWidth: 1,
    borderStyle: "solid",
    borderColor: "var(--edge)",
    backgroundColor: "var(--panel)",
    position: "relative",
    overflow: "hidden",
  },
  mapWindow: {
    position: "absolute",
    borderWidth: 1,
    borderStyle: "solid",
    borderColor: "var(--accent)",
    backgroundColor: "var(--accent-wash)",
  },

  frame: { position: "relative" },
  viewport: {
    borderWidth: 1,
    borderStyle: "solid",
    borderColor: "var(--edge)",
    borderRadius: 8,
    backgroundColor: "var(--panel)",
    overflow: "auto",
    maxHeight: "82vh",
    minHeight: 120,
  },
  // Every rendition occupies the *same* grid cell, so switching config cannot
  // move the picture by a pixel — the whole point of comparing this way.
  // Inactive ones stay laid out (hidden, not removed) so nothing reflows.
  // `gridArea` is a shorthand StyleX drops; the start longhands are what stack.
  stage: { display: "grid", alignItems: "start", justifyItems: "start" },
  rendition: { gridRowStart: "1", gridColumnStart: "1", display: "block" },
  hidden: { visibility: "hidden" },
  fit: { maxWidth: "100%", maxHeight: "82vh", height: "auto", width: "auto" },
  // `width`/`height` **auto**, not just the max-* releases. The `width=`/`height=`
  // attributes are presentational, so with nothing overriding them they *become*
  // the rendered size — a stale or copied dimension in `review.json` would then
  // silently scale the picture in the one mode whose purpose is 1:1 inspection,
  // and two renditions declaring different dimensions would render at different
  // sizes in the same grid cell, moving the picture on toggle. `fullsize` means
  // natural size; say so.
  fullsize: { maxWidth: "none", maxHeight: "none", width: "auto", height: "auto" },

  pan: {
    position: "absolute",
    display: "grid",
    alignItems: "center",
    justifyItems: "center",
    width: 34,
    height: 34,
    borderRadius: 8,
    borderWidth: 1,
    borderStyle: "solid",
    borderColor: "var(--edge)",
    backgroundColor: "var(--scrim)",
    color: "var(--fg)",
    fontSize: 15,
    cursor: "pointer",
    padding: 0,
  },
  panUp: { top: 8, insetInlineStart: "50%" },
  panDown: { bottom: 8, insetInlineStart: "50%" },
  panLeft: { insetInlineStart: 8, top: "50%" },
  panRight: { insetInlineEnd: 8, top: "50%" },
  // An overlay, not a replacement. Swapping the scroller out for a message
  // unmounts it, and the scroll position goes with it: park somewhere in
  // fullsize, toggle through a config that has no rendition, and you come back
  // to the top-left. Keeping the stage mounted (every rendition hidden) holds
  // both the box and the position.
  missing: {
    position: "absolute",
    top: 12,
    insetInlineStart: "50%",
    paddingBlock: 8,
    paddingInline: 14,
    borderRadius: 8,
    borderWidth: 1,
    borderStyle: "solid",
    borderColor: "var(--edge)",
    backgroundColor: "var(--scrim)",
    color: "var(--fg-dim)",
  },
});

/**
 * Declared dimensions are a promise about the file; check it once per load.
 *
 * They exist so the page can reserve space before the image arrives. Nothing
 * downstream scales by them — but a set whose numbers are wrong is a set whose
 * mini-map and reserved space are wrong too, and that is invisible otherwise.
 */
function warnOnDimensionMismatch(
  element: HTMLImageElement,
  rendition: Rendition | undefined,
  imageLabel: string,
  configLabel: string,
): void {
  if (!rendition?.width || !rendition.height) return;
  if (element.naturalWidth === rendition.width && element.naturalHeight === rendition.height) {
    return;
  }
  console.warn(
    `${imageLabel} — ${configLabel}: review.json declares ` +
      `${rendition.width}x${rendition.height} but the image is ` +
      `${element.naturalWidth}x${element.naturalHeight}. The declared size only ` +
      `reserves space; fix it so the reserved box and mini-map match the picture.`,
  );
}

interface Props {
  image: ReviewImage;
  configs: readonly ReviewConfig[];
  activeIndex: number;
  onActivate: (index: number) => void;
  zoom: ZoomMode;
}

/**
 * Overflow state — coarse, and deliberately *not* live scroll position.
 *
 * Scroll position drives the mini-map, which updates on every scroll event. Doing
 * that through a signal means every scroll frame re-renders, and a re-render that
 * changes layout feeds the next measurement: that cycle wedged the renderer so
 * hard Chrome could not inject a script into the page. So the mini-map is written
 * imperatively in the scroll handler, and only this — whether the viewport
 * scrolls at all — is reactive, changing just a few times per session.
 */
interface Overflow {
  readonly x: boolean;
  readonly y: boolean;
}

export function ImageSection(props: Props) {
  const [viewport, setViewport] = createSignal<HTMLDivElement>();
  const [overflow, setOverflow] = createSignal<Overflow>(
    { x: false, y: false },
    { equals: (a, b) => a.x === b.x && a.y === b.y },
  );
  let mapWindow: HTMLDivElement | undefined;

  /** Position the mini-map's window box. Direct DOM write, no signals. */
  const paintMap = () => {
    const element = viewport();
    if (!element || !mapWindow) return;
    const pct = (part: number, whole: number) =>
      `${Math.min(100, (part / Math.max(whole, 1)) * 100)}%`;
    mapWindow.style.left = pct(element.scrollLeft, element.scrollWidth);
    mapWindow.style.top = pct(element.scrollTop, element.scrollHeight);
    mapWindow.style.width = pct(element.clientWidth, element.scrollWidth);
    mapWindow.style.height = pct(element.clientHeight, element.scrollHeight);
  };

  // Painted straight from the scroll handler. The browser already coalesces
  // scroll events to about one per frame, and this only reads scroll offsets and
  // writes four inline styles on a 104x70 box — cheap enough not to need
  // throttling, and unlike `requestAnimationFrame` it still runs in a hidden or
  // backgrounded tab.
  /** Recheck whether the viewport scrolls. Cheap, and rare. */
  const measureOverflow = () => {
    const element = viewport();
    if (!element) return;
    setOverflow({
      x: element.scrollWidth - element.clientWidth > 1,
      y: element.scrollHeight - element.clientHeight > 1,
    });
    paintMap();
  };

  onMount(() => {
    measureOverflow();
    const onResize = () => measureOverflow();
    window.addEventListener("resize", onResize);
    onCleanup(() => window.removeEventListener("resize", onResize));
  });

  // Switching fit/fullsize resizes the stage, so re-check overflow. Measured
  // **synchronously** — Solid runs effects after the DOM is updated, so layout is
  // already current — with a follow-up frame for anything that settles late.
  // Correctness must not depend on `requestAnimationFrame`: a hidden or
  // backgrounded tab never fires it, which left the pan controls permanently
  // absent there.
  createEffect(
    on(
      () => props.zoom,
      () => {
        measureOverflow();
        requestAnimationFrame(measureOverflow);
      },
    ),
  );

  const canPan = () => props.zoom === "fullsize" && (overflow().x || overflow().y);

  /** Step by most of a screen, keeping a sliver of overlap for continuity. */
  const pan = (dx: number, dy: number) => {
    const element = viewport();
    if (!element) return;
    element.scrollBy({
      left: dx * element.clientWidth * 0.8,
      top: dy * element.clientHeight * 0.8,
      // Smooth scrolling is driven by the same frame loop as `requestAnimationFrame`
      // and simply does not run in a hidden tab — the scroll would be silently
      // dropped. Fall back to an instant jump there so the action always happens.
      behavior: document.visibilityState === "visible" ? "smooth" : "auto",
    });
  };

  const activeId = () => props.configs[props.activeIndex]?.id;
  const activeRendition = () => {
    const id = activeId();
    return id === undefined ? undefined : props.image.renditions.get(id);
  };

  /**
   * Reserve the declared box in `fullsize`, before the image has loaded.
   *
   * A **floor**, not a size: the image still renders at its natural dimensions, so
   * a stale declared value cannot scale the picture (which is why `fullsize` sets
   * `width`/`height: auto` in the first place). Without this the stage is 0x0 until
   * the first decode and the whole section jumps when it lands. Not applied in
   * `fit`, where a 2400px floor would force horizontal overflow on a column that
   * is meant to shrink the image to fit.
   */
  const reservation = () => {
    const rendition = activeRendition();
    if (props.zoom !== "fullsize" || !rendition?.width || !rendition.height) {
      return undefined;
    }
    return {
      "min-width": `${rendition.width}px`,
      "min-height": `${rendition.height}px`,
    };
  };
  const hasActive = () => {
    const id = activeId();
    return id !== undefined && props.image.renditions.has(id);
  };

  return (
    <section class={cls(styles.section)}>
      <div class={cls(styles.head)}>
        <span class={cls(styles.label)}>{props.image.label}</span>
        <Show when={props.image.note}>
          {(note) => <span class={cls(styles.note)}>{note()}</span>}
        </Show>
      </div>

      <div class={cls(styles.strip)}>
        <For each={props.configs}>
          {(config, index) => {
            const rendition = () => props.image.renditions.get(config.id);
            return (
              <button
                type="button"
                class={cls(styles.preview, index() === props.activeIndex && styles.previewActive)}
                aria-pressed={index() === props.activeIndex}
                title={`${config.label}${rendition() ? "" : " — no rendition for this image"}`}
                onClick={() => props.onActivate(index())}
              >
                <Show
                  when={rendition()}
                  fallback={
                    <span class={cls(styles.previewMissing)}>
                      {config.label}
                      <br />
                      missing
                    </span>
                  }
                >
                  {(present) => (
                    <img
                      class={cls(styles.previewImage)}
                      src={present().preview}
                      alt={`${props.image.label} — ${config.label}`}
                      loading="lazy"
                    />
                  )}
                </Show>
              </button>
            );
          }}
        </For>

        <Show when={canPan()}>
          <div
            class={cls(styles.mapOuter)}
            title="Where the viewport sits inside the full-size image"
          >
            <div
              class={cls(styles.mapWindow)}
              ref={(element) => {
                // Paint on attach: the map mounts *because* overflow appeared, so
                // the measurement that revealed it ran before this element existed.
                mapWindow = element;
                paintMap();
              }}
            />
          </div>
        </Show>
      </div>

      <div class={cls(styles.frame)}>
        <div class={cls(styles.viewport)} ref={setViewport} onScroll={paintMap}>
          <div class={cls(styles.stage)} style={reservation()}>
            <For each={props.configs}>
              {(config) => (
                <Show when={props.image.renditions.get(config.id)}>
                  {(rendition) => (
                    <img
                      class={cls(
                        styles.rendition,
                        props.zoom === "fit" ? styles.fit : styles.fullsize,
                        config.id !== activeId() && styles.hidden,
                      )}
                      src={rendition().src}
                      width={rendition().width}
                      height={rendition().height}
                      alt={`${props.image.label} — ${config.label}`}
                      onLoad={(event) => {
                        warnOnDimensionMismatch(
                          event.currentTarget,
                          rendition(),
                          props.image.label,
                          config.label,
                        );
                        measureOverflow();
                      }}
                    />
                  )}
                </Show>
              )}
            </For>
          </div>
        </div>

        <Show when={!hasActive()}>
          <div class={cls(styles.missing)}>
            No rendition of {props.image.label} for config{" "}
            {props.configs[props.activeIndex]?.label ?? "?"}.
          </div>
        </Show>

        <Show when={canPan()}>
          <Show when={overflow().y}>
            <button
              type="button"
              class={cls(styles.pan, styles.panUp)}
              aria-label="Pan up"
              onClick={() => pan(0, -1)}
            >
              ↑
            </button>
            <button
              type="button"
              class={cls(styles.pan, styles.panDown)}
              aria-label="Pan down"
              onClick={() => pan(0, 1)}
            >
              ↓
            </button>
          </Show>
          <Show when={overflow().x}>
            <button
              type="button"
              class={cls(styles.pan, styles.panLeft)}
              aria-label="Pan left"
              onClick={() => pan(-1, 0)}
            >
              ←
            </button>
            <button
              type="button"
              class={cls(styles.pan, styles.panRight)}
              aria-label="Pan right"
              onClick={() => pan(1, 0)}
            >
              →
            </button>
          </Show>
        </Show>
      </div>
    </section>
  );
}
