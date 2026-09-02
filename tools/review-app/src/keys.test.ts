import { describe, expect, it } from "vite-plus/test";
import { actionForKey, keyForConfigIndex } from "./keys";

const NONE = {};

describe("actionForKey", () => {
  it("maps the number row left to right, with 0 as the tenth", () => {
    expect(actionForKey("1", NONE, 10)).toEqual({ kind: "config", index: 0 });
    expect(actionForKey("9", NONE, 10)).toEqual({ kind: "config", index: 8 });
    expect(actionForKey("0", NONE, 10)).toEqual({ kind: "config", index: 9 });
  });

  it("ignores a number with no config behind it", () => {
    expect(actionForKey("4", NONE, 2)).toBeNull();
    expect(actionForKey("0", NONE, 2)).toBeNull();
  });

  it("leaves modified keypresses to the browser and window manager", () => {
    // ⌘1 switches browser tabs; claiming it would break the user's own shortcut.
    expect(actionForKey("1", { meta: true }, 4)).toBeNull();
    expect(actionForKey("1", { ctrl: true }, 4)).toBeNull();
    expect(actionForKey("1", { alt: true }, 4)).toBeNull();
  });

  it("toggles zoom on f, in either case", () => {
    expect(actionForKey("f", NONE, 3)).toEqual({ kind: "zoom" });
    expect(actionForKey("F", NONE, 3)).toEqual({ kind: "zoom" });
  });

  it("ignores everything else", () => {
    for (const key of ["a", "Enter", "ArrowLeft", " ", "Shift"]) {
      expect(actionForKey(key, NONE, 4)).toBeNull();
    }
  });
});

describe("keyForConfigIndex", () => {
  it("labels the buttons with the key that reaches them", () => {
    expect(keyForConfigIndex(0)).toBe("1");
    expect(keyForConfigIndex(8)).toBe("9");
    expect(keyForConfigIndex(9)).toBe("0");
  });

  it("has no key past the number row", () => {
    expect(keyForConfigIndex(10)).toBeUndefined();
  });
});
