/**
 * Keyboard mapping, kept free of the DOM so it can be tested directly.
 *
 * `1`-`9` select the first nine configs and `0` selects the tenth — the layout
 * of the number row, so the keys read left to right exactly like the buttons.
 * Beyond ten there is no key; the buttons still work.
 */

/** How many configs the number row can reach. */
export const KEYBOARD_REACHABLE_CONFIGS = 10;

export type KeyAction =
  | { readonly kind: "config"; readonly index: number }
  | { readonly kind: "zoom" };

/**
 * The action a keypress should perform, or `null` for keys we leave alone.
 *
 * Modified keypresses are always `null`: `⌘1` switches browser tabs and `⌃1` is
 * a window-manager binding on some setups, so claiming them would break the
 * user's own shortcuts.
 */
export function actionForKey(
  key: string,
  modifiers: { alt?: boolean; ctrl?: boolean; meta?: boolean },
  configCount: number,
): KeyAction | null {
  if (modifiers.alt || modifiers.ctrl || modifiers.meta) return null;

  if (key === "f" || key === "F") return { kind: "zoom" };

  if (key.length === 1 && key >= "0" && key <= "9") {
    // '1'..'9' are 0..8; '0' is the tenth slot rather than the first.
    const index = key === "0" ? 9 : key.charCodeAt(0) - "1".charCodeAt(0);
    return index < configCount ? { kind: "config", index } : null;
  }

  return null;
}

/** The key that selects a config, or `undefined` past the number row. */
export function keyForConfigIndex(index: number): string | undefined {
  if (index < 0 || index >= KEYBOARD_REACHABLE_CONFIGS) return undefined;
  return index === 9 ? "0" : String(index + 1);
}

/** Typing in a field must not be swallowed as a shortcut. */
export function isTextEntry(target: EventTarget | null): boolean {
  if (!(target instanceof HTMLElement)) return false;
  if (target.isContentEditable) return true;
  const tag = target.tagName;
  return tag === "INPUT" || tag === "TEXTAREA" || tag === "SELECT";
}
