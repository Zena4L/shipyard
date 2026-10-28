import { defineConfig } from "vite";
import { svelte } from "@sveltejs/vite-plugin-svelte";

// Tauri's dev window loads this port: refused, never moved, if it's taken
// (a page served on another port by someone else is never loaded).
// `--mode fixture` builds the page with the mock's data (preview.html) on
// its own, for the test that loads it under the release CSP.
// Nothing is inlined as a `data:` URI: the release CSP's `font-src 'self'`
// refuses one (small font subsets were, and fell back to other faces).
export default defineConfig(({ mode }) => ({
  plugins: [svelte()],
  clearScreen: false,
  server: { port: 1420, strictPort: true, host: "127.0.0.1" },
  build: {
    target: "safari16",
    assetsInlineLimit: 0,
    emptyOutDir: true,
    ...(mode === "fixture" ? { outDir: "dist-fixture", rollupOptions: { input: "preview.html" } } : { outDir: "dist" }),
  },
  test: { environment: "jsdom", include: ["src/**/*.test.ts"] },
  resolve: process.env.VITEST ? { conditions: ["browser"] } : undefined,
}));
