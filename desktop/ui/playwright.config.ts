import { defineConfig, devices } from "@playwright/test";

// The visual comparison of the app with the design's mock (step 14.2), in
// WebKit: the engine the app's window is on a Mac.
export default defineConfig({
  testDir: "tests",
  timeout: 60_000,
  reporter: "line",
  use: { ...devices["Desktop Safari"], deviceScaleFactor: 2 },
  projects: [{ name: "webkit", use: { browserName: "webkit" } }],
  webServer: {
    command: "npm run dev",
    url: "http://127.0.0.1:1420/preview.html",
    // `npm run tauri dev` runs this same server on this port: the tests use
    // it while the app is open (it serves this tree).
    reuseExistingServer: true,
    timeout: 60_000,
  },
});
