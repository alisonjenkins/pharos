import { defineConfig, devices } from "@playwright/test";

// Mixed-client suite: a pharos Dioxus UI browser and an unmodified
// jellyfin-web browser in one SyncPlay group on one pharos.
//
// The Dioxus person talks to pharos directly (`PHAROS_URL`, which serves
// `/ui/`); the jellyfin-web person goes through the same-origin front-end
// `tools/jf-proxy.cjs`, which serves the jellyfin-web bundle and proxies the
// REST API and `/socket` to the same pharos. `baseURL` is the proxy because
// `lib/virtual-person.ts` navigates with relative URLs; the Dioxus spec uses
// absolute `PHAROS_URL` ones.
//
// Run through `just compat-dioxus-full on 8096 playwright.mixed.config.ts`,
// which seeds pharos and the bundles.

const JELLYFIN_WEB_PORT = parseInt(process.env.JELLYFIN_WEB_PORT ?? "8910", 10);
if (!process.env.JELLYFIN_WEB_DIR) {
  throw new Error("JELLYFIN_WEB_DIR not set — enter the nix devShell (`nix develop`).");
}

export default defineConfig({
  testDir: "./tests/mixed",
  fullyParallel: false,
  workers: 1,
  retries: process.env.CI ? 2 : 0,
  reporter: process.env.CI ? "github" : "list",
  timeout: process.env.CI ? 300_000 : 150_000,

  use: {
    baseURL: `http://127.0.0.1:${JELLYFIN_WEB_PORT}`,
    trace: "retain-on-failure",
    screenshot: "only-on-failure",
    video: "retain-on-failure",
    launchOptions: { args: ["--autoplay-policy=no-user-gesture-required"] },
  },

  projects: [
    {
      name: "chromium",
      use: { ...devices["Desktop Chrome"], channel: "chromium" },
    },
  ],

  webServer: {
    command: `node tools/jf-proxy.cjs`,
    port: JELLYFIN_WEB_PORT,
    reuseExistingServer: true,
    timeout: 30_000,
  },
});
