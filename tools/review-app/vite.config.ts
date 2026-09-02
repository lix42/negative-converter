import { defineConfig } from "vite-plus";
import solid from "vite-plugin-solid";
import stylex from "@stylexjs/unplugin";

export default defineConfig(({ mode }) => ({
  // Relative asset URLs: the built page is served from whatever directory it is
  // copied into, next to the review sets it renders.
  base: "./",
  plugins: [
    // StyleX's unplugin holds a handle that stops Vitest from exiting — the run
    // passes, then the process hangs ~10s before dying (measured: 10.9s with it,
    // 0.9s without). The tests here cover pure modules that import no styles, so
    // the compiler has nothing to do under test. If component tests ever need
    // compiled styles, put it back and budget for the hang.
    ...(mode === "test" ? [] : [stylex.vite({ useCSSLayers: true })]),
    // StyleX must come *before* the framework plugin, or Fast Refresh breaks.
    solid(),
  ],
  // Lint and format settings live here rather than in separate oxlint/oxfmt
  // files — Vite+ reads them from the one config, and `vp check` runs all three
  // (format, lint, type-check) as a single gate.
  lint: {
    ignorePatterns: ["dist/**"],
    options: { typeAware: true, typeCheck: true },
  },
  test: {
    environment: "node",
    include: ["src/**/*.test.ts"],
  },
}));
