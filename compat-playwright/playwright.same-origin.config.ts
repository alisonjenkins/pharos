import { defineConfig, devices } from "@playwright/test";

// Same-origin jellyfin-web, as production serves it (a proxy in front of
// pharos): tools/jf-proxy.cjs serves the bundle and forwards the REST API and
// /socket to pharos. Only same-origin does the browser keep the JellyfinAuth
// cookie that <img> and <track> fetches authenticate with (V191), so the
// item-file auth spec runs here. pharos is started by
// `just compat-playwright-full`.

const JELLYFIN_WEB_PORT = parseInt(process.env.JELLYFIN_WEB_PORT ?? "8910", 10);
if (!process.env.JELLYFIN_WEB_DIR) {
  throw new Error("JELLYFIN_WEB_DIR not set — enter the nix devShell (`nix develop`).");
}

export default defineConfig({
  testDir: "./tests",
  testMatch: /item-file-auth\.spec\.ts/,
  workers: 1,
  reporter: process.env.CI ? "github" : "list",
  timeout: 60_000,
  use: {
    baseURL: `http://127.0.0.1:${JELLYFIN_WEB_PORT}`,
    trace: "retain-on-failure",
    screenshot: "only-on-failure",
    extraHTTPHeaders: { "X-Pharos-Compat-Suite": "playwright" },
  },
  projects: [
    { name: "chromium", use: { ...devices["Desktop Chrome"], channel: "chromium" } },
  ],
  webServer: {
    command: `node tools/jf-proxy.cjs`,
    port: JELLYFIN_WEB_PORT,
    reuseExistingServer: false,
    timeout: 30_000,
  },
});
