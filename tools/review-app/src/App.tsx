import * as stylex from "@stylexjs/stylex";
import { For, Show, createMemo, createResource, createSignal, onCleanup, onMount } from "solid-js";
import { cls } from "./cls";
import { ControlBar } from "./ControlBar";
import { ImageSection } from "./ImageSection";
import { actionForKey, isTextEntry } from "./keys";
import { hasDataParam, loadReview, reviewUrl, type Review, type ZoomMode } from "./review";

const styles = stylex.create({
  header: { paddingBlock: 18, paddingInline: 16 },
  title: { marginBlock: 0, fontSize: 18 },
  description: { marginBlockStart: 4, color: "var(--fg-dim)", maxWidth: "78ch" },
  panel: { padding: 24, maxWidth: "80ch" },
  heading: { marginBlockStart: 0, fontSize: 18 },
  error: { color: "var(--bad)", whiteSpace: "pre-wrap" },
  code: {
    fontFamily: "ui-monospace, SFMono-Regular, Menlo, monospace",
    fontSize: 13,
    backgroundColor: "var(--panel)",
    paddingBlock: 2,
    paddingInline: 6,
    borderRadius: 4,
  },
  block: {
    display: "block",
    fontFamily: "ui-monospace, SFMono-Regular, Menlo, monospace",
    fontSize: 13,
    backgroundColor: "var(--panel)",
    padding: 12,
    borderRadius: 6,
    marginBlock: 12,
    whiteSpace: "pre-wrap",
    wordBreak: "break-all",
  },
  dim: { color: "var(--fg-dim)" },
});

export function App() {
  const [review] = createResource<Review>(() => loadReview(window.location.href));
  const [activeIndex, setActiveIndex] = createSignal(0);
  const [zoom, setZoom] = createSignal<ZoomMode>("fit");

  const configs = createMemo(() => (review.state === "ready" ? review().configs : []));

  onMount(() => {
    const onKeyDown = (event: KeyboardEvent) => {
      if (isTextEntry(event.target)) return;
      const action = actionForKey(
        event.key,
        { alt: event.altKey, ctrl: event.ctrlKey, meta: event.metaKey },
        configs().length,
      );
      if (!action) return;
      event.preventDefault();
      if (action.kind === "config") setActiveIndex(action.index);
      else setZoom((current) => (current === "fit" ? "fullsize" : "fit"));
    };
    document.addEventListener("keydown", onKeyDown);
    onCleanup(() => document.removeEventListener("keydown", onKeyDown));
  });

  // `review()` re-throws when the resource errored, so every read is guarded by
  // `state` — reading it unguarded takes the whole page down to a blank screen,
  // which is precisely the failure a missing `?data=` used to produce.
  const ready = () => (review.state === "ready" ? review() : undefined);

  return (
    <main>
      <Show when={review.state === "errored"}>
        <section class={cls(styles.panel)}>
          <Show
            when={hasDataParam(window.location.href)}
            fallback={
              <>
                <h1 class={cls(styles.heading)}>Point this at a review set</h1>
                <p>
                  This page renders a <b>review set</b>: a{" "}
                  <span class={cls(styles.code)}>review.json</span> plus the images it names. Name
                  one with the <span class={cls(styles.code)}>?data=</span> parameter — paths inside
                  it resolve next to that file.
                </p>
                <span class={cls(styles.block)}>?data=examples/synthetic/review.json</span>
                <p>
                  <a href="?data=examples/synthetic/review.json">Open the bundled example</a> to see
                  what it looks like, or read <span class={cls(styles.code)}>SCHEMA.md</span> for
                  the format.
                </p>
                <p class={cls(styles.dim)}>
                  With no <span class={cls(styles.code)}>?data=</span>, the page looks for{" "}
                  <span class={cls(styles.code)}>./review.json</span> beside itself, and none
                  loaded:
                </p>
                <p class={cls(styles.dim, styles.error)}>{String(review.error)}</p>
              </>
            }
          >
            <h1 class={cls(styles.heading)}>That review set did not load</h1>
            <p class={cls(styles.error)}>{String(review.error)}</p>
            <span class={cls(styles.block)}>{reviewUrl(window.location.href)}</span>
            <p class={cls(styles.dim)}>
              Both the JSON and its images must be served over http(s) — a{" "}
              <span class={cls(styles.code)}>file://</span> page cannot fetch them.{" "}
              <span class={cls(styles.code)}>SCHEMA.md</span> documents the format.
            </p>
          </Show>
        </section>
      </Show>

      <Show when={ready()}>
        {(loaded) => (
          <>
            <ControlBar
              configs={loaded().configs}
              activeIndex={activeIndex()}
              onActivate={setActiveIndex}
              zoom={zoom()}
              onZoom={setZoom}
            />
            <Show when={loaded().title || loaded().description}>
              <header class={cls(styles.header)}>
                <Show when={loaded().title}>
                  {(title) => <h1 class={cls(styles.title)}>{title()}</h1>}
                </Show>
                <Show when={loaded().description}>
                  {(description) => <p class={cls(styles.description)}>{description()}</p>}
                </Show>
              </header>
            </Show>
            <For each={loaded().images}>
              {(image) => (
                <ImageSection
                  image={image}
                  configs={loaded().configs}
                  activeIndex={activeIndex()}
                  onActivate={setActiveIndex}
                  zoom={zoom()}
                />
              )}
            </For>
            <Show when={loaded().images.length === 0}>
              <p class={cls(styles.panel)}>This review set lists no images.</p>
            </Show>
          </>
        )}
      </Show>
    </main>
  );
}
