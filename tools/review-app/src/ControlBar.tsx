import * as stylex from "@stylexjs/stylex";
import { For, Show } from "solid-js";
import { cls } from "./cls";
import { keyForConfigIndex } from "./keys";
import type { ReviewConfig, ZoomMode } from "./review";

// Longhands only. StyleX drops shorthands it does not model — `background` and
// `border` were silently absent from the output, leaving white text on the
// browser's default button face. Measured in the browser, not assumed.
const styles = stylex.create({
  bar: {
    position: "sticky",
    top: 0,
    zIndex: 10,
    display: "flex",
    flexWrap: "wrap",
    gap: 16,
    alignItems: "center",
    paddingBlock: 10,
    paddingInline: 16,
    backgroundColor: "var(--panel)",
    borderBottomWidth: 1,
    borderBottomStyle: "solid",
    borderBottomColor: "var(--edge)",
  },
  group: { display: "flex", gap: 6, alignItems: "center" },
  spacer: { marginInlineStart: "auto" },
  button: {
    display: "inline-flex",
    alignItems: "baseline",
    gap: 6,
    paddingBlock: 5,
    paddingInline: 12,
    borderRadius: 6,
    borderWidth: 1,
    borderStyle: "solid",
    borderColor: "var(--edge)",
    backgroundColor: "var(--button)",
    color: "var(--fg)",
    fontFamily: "inherit",
    fontSize: "inherit",
    cursor: "pointer",
  },
  active: {
    backgroundColor: "var(--accent)",
    borderColor: "var(--accent)",
    color: "var(--accent-ink)",
    fontWeight: 600,
  },
  // `<kbd>` defaults to the UA's monospace font, which differs per platform and
  // per element; state it so the key sits on the label's baseline predictably.
  // Colour is inherited so one rule reads correctly on both the plain and the
  // filled (active) button.
  key: {
    fontFamily: "ui-monospace, SFMono-Regular, Menlo, monospace",
    fontSize: 11,
    opacity: 0.7,
    fontVariantNumeric: "tabular-nums",
  },
  hint: {
    fontFamily: "ui-monospace, SFMono-Regular, Menlo, monospace",
    color: "var(--fg-dim)",
    fontSize: 12,
  },
});

interface Props {
  configs: readonly ReviewConfig[];
  activeIndex: number;
  onActivate: (index: number) => void;
  zoom: ZoomMode;
  onZoom: (zoom: ZoomMode) => void;
}

export function ControlBar(props: Props) {
  return (
    <div class={cls(styles.bar)}>
      <div class={cls(styles.group)}>
        <For each={props.configs}>
          {(config, index) => {
            const shortcut = () => keyForConfigIndex(index());
            return (
              <button
                type="button"
                class={cls(styles.button, index() === props.activeIndex && styles.active)}
                aria-pressed={index() === props.activeIndex}
                // The real ARIA spelling of what the `<kbd>` shows, so the shortcut
                // is announced as a shortcut rather than read as part of the name.
                aria-keyshortcuts={shortcut()}
                title={config.note ?? config.label}
                onClick={() => props.onActivate(index())}
              >
                <span>{config.label}</span>
                {/* Past the number row there is no key, and an empty <kbd> claims
                    a keystroke that does not exist — so render none. */}
                <Show when={shortcut()}>
                  {(key) => (
                    <kbd class={cls(styles.key)} aria-hidden="true">
                      {key()}
                    </kbd>
                  )}
                </Show>
              </button>
            );
          }}
        </For>
      </div>

      <div class={cls(styles.group, styles.spacer)}>
        <For each={["fullsize", "fit"] as const}>
          {(mode) => (
            <button
              type="button"
              class={cls(styles.button, props.zoom === mode && styles.active)}
              aria-pressed={props.zoom === mode}
              aria-keyshortcuts="f"
              title={`Show images at ${mode === "fit" ? "fit" : "natural"} size (f toggles)`}
              onClick={() => props.onZoom(mode)}
            >
              {mode}
            </button>
          )}
        </For>
        <kbd class={cls(styles.hint)} aria-hidden="true">
          f
        </kbd>
      </div>
    </div>
  );
}
