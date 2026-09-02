import { createRequire } from "node:module";
import { describe, expect, it } from "vite-plus/test";

/**
 * The project depends on `vite` aliased to Vite+'s core
 * (`"vite": "npm:@voidzero-dev/vite-plus-core"`), so that `vite-plugin-solid`
 * and the StyleX unplugin — which both import `vite` — get the same build Vite+
 * runs. A second, real Vite in the tree would load plugins into a different
 * instance and fail in ways that look like plugin bugs.
 *
 * `package.json`'s `overrides` block states that intent, but **npm reads it and
 * pnpm does not** — and pnpm 11 dropped the `pnpm` field too, moving settings to
 * `pnpm-workspace.yaml`. This project runs on pnpm, so the guarantee is asserted
 * here rather than trusted to a field the package manager ignores.
 */
describe("toolchain", () => {
  const require = createRequire(import.meta.url);

  it("resolves vite to Vite+ core", () => {
    const pkg = require("vite/package.json") as { name: string };
    expect(pkg.name).toBe("@voidzero-dev/vite-plus-core");
  });

  it("gives the framework plugin that same Vite", () => {
    // Resolve from the plugin's own location: a plugin that dragged in its own
    // Vite would find that one first, which is exactly the failure being ruled out.
    const pluginUrl = import.meta.resolve("vite-plugin-solid");
    const fromPlugin = createRequire(pluginUrl)("vite/package.json") as { name: string };
    expect(fromPlugin.name).toBe("@voidzero-dev/vite-plus-core");
  });
});
