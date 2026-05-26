import { defineConfig, devices } from "@playwright/test";

// Two URLs the test needs:
//   PHAROS_URL        — base URL of a running pharos instance (default 127.0.0.1:8096)
//   JELLYFIN_WEB_URL  — base URL of a separately-served jellyfin-web bundle (default 127.0.0.1:8910)
//
// The Playwright `webServer` block boots http-server to serve a local
// jellyfin-web build copied into ./jellyfin-web/. Build via:
//   git clone https://github.com/jellyfin/jellyfin-web ../jellyfin-web
//   cd ../jellyfin-web && npm ci && npm run build:production
//   cp -r dist /path/to/pharos/compat-playwright/jellyfin-web
//
// pharos itself must be running separately — the test does NOT bring it
// up (we want the *real* pharos serve binary under test, not a custom
// fixture, and the Rust integration test in tests/client_compat.rs
// already covers the test-server variant).

const PHAROS_URL = process.env.PHAROS_URL ?? "http://127.0.0.1:8096";
const JELLYFIN_WEB_PORT = parseInt(process.env.JELLYFIN_WEB_PORT ?? "8910", 10);

export default defineConfig({
  testDir: "./tests",
  fullyParallel: false,
  forbidOnly: !!process.env.CI,
  retries: process.env.CI ? 2 : 0,
  workers: 1,
  reporter: process.env.CI ? "github" : "list",
  timeout: 60_000,

  use: {
    baseURL: `http://127.0.0.1:${JELLYFIN_WEB_PORT}`,
    trace: "retain-on-failure",
    screenshot: "only-on-failure",
    video: "retain-on-failure",
    extraHTTPHeaders: {
      "X-Pharos-Compat-Suite": "playwright",
    },
  },

  projects: [
    {
      name: "chromium",
      use: { ...devices["Desktop Chrome"] },
    },
  ],

  webServer: {
    // Serve the pre-built jellyfin-web bundle so the browser loads it
    // from a local origin. Keeps SOP simple; pharos serves the API
    // origin separately at PHAROS_URL.
    command: `npx http-server ./jellyfin-web -p ${JELLYFIN_WEB_PORT} -s --cors`,
    port: JELLYFIN_WEB_PORT,
    reuseExistingServer: true,
    timeout: 30_000,
  },

  metadata: {
    pharos_url: PHAROS_URL,
  },
});
