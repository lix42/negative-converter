/**
 * StyleX → Solid adapter.
 *
 * Two traps this exists to close, both of which fail *silently*:
 *
 * 1. `stylex.props()` returns React's spelling (`className`), which Solid's JSX
 *    does not treat as the class attribute — the element renders unstyled.
 * 2. Spreading the result (`<div {...stylex.props(a, cond && b)} />`) is
 *    evaluated **once**, so a conditional style freezes at first render and the
 *    active state never moves. Returning a bare string instead forces call sites
 *    to write `class={cls(...)}`, which Solid tracks like any other attribute
 *    expression.
 *
 * Dynamic values (StyleX style functions) are not used here; when they are
 * needed, pass a plain object to `style={}` rather than reaching for the spread.
 */
import * as stylex from "@stylexjs/stylex";
import type { StyleXStyles } from "@stylexjs/stylex";

export function cls(...styles: StyleXStyles[]): string | undefined {
  return stylex.props(...styles).className;
}
