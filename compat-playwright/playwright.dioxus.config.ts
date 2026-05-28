import { defineConfig, devices } from "@playwright/test";

// T51 phase 3 — Playwright config for the pharos Dioxus UI suite.
//
// This is a thin sibling of `playwright.config.ts` that drops the
// `JELLYFIN_WEB_DIR` requirement + the local http-server fixture: the
// Dioxus suite hits pharos's own `/ui/*` route, which serves the WASM
// bundle direct.
//
// Env contract:
//   PHAROS_URL  — base URL of a running pharos instance whose
//                 `[server].ui_dir` points at a `dx build --release`
//                 output. Defaults to 127.0.0.1:8096.
//
// Build the bundle before running:
//   nix develop --command dx build --package pharos-ui --release
// Then point `[server].ui_dir` at `target/dx/pharos-ui/release/web/public`.

const PHAROS_URL = process.env.PHAROS_URL ?? "http://127.0.0.1:8096";

export default defineConfig({
  testDir: "./tests/dioxus",
  fullyParallel: false,
  forbidOnly: !!process.env.CI,
  retries: process.env.CI ? 2 : 0,
  workers: 1,
  reporter: process.env.CI ? "github" : "list",
  timeout: 60_000,

  use: {
    baseURL: PHAROS_URL,
    trace: "retain-on-failure",
    screenshot: "only-on-failure",
    video: "retain-on-failure",
    extraHTTPHeaders: {
      "X-Pharos-Compat-Suite": "playwright-dioxus",
    },
  },

  projects: [
    {
      name: "chromium",
      use: {
        ...devices["Desktop Chrome"],
        channel: "chromium",
      },
    },
  ],

  metadata: {
    pharos_url: PHAROS_URL,
    suite: "dioxus-ui",
  },
});
