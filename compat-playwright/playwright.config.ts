import { defineConfig, devices } from "@playwright/test";

// Env contract:
//   PHAROS_URL        — base URL of a running pharos instance (default 127.0.0.1:8096)
//   JELLYFIN_WEB_DIR  — directory containing the prebuilt jellyfin-web
//                       bundle (set automatically by the nix devShell
//                       from `pkgs.jellyfin-web`).
//   JELLYFIN_WEB_PORT — local port for http-server (default 8910)
//
// pharos itself must be running separately. The Rust integration test
// in tests/client_compat.rs covers the test-server variant.

const PHAROS_URL = process.env.PHAROS_URL ?? "http://127.0.0.1:8096";
const JELLYFIN_WEB_PORT = parseInt(process.env.JELLYFIN_WEB_PORT ?? "8910", 10);
const JELLYFIN_WEB_DIR = process.env.JELLYFIN_WEB_DIR;
if (!JELLYFIN_WEB_DIR) {
  throw new Error(
    "JELLYFIN_WEB_DIR not set — enter the nix devShell (`nix develop`) which exports it from pkgs.jellyfin-web.",
  );
}

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
      use: {
        ...devices["Desktop Chrome"],
        // Use full chromium (not chrome-headless-shell). nixpkgs's
        // playwright-driver.browsers ships chromium-1217 but the
        // matching chrome-headless-shell binary is out of sync; the
        // full Chromium app is the one always present.
        channel: "chromium",
      },
    },
  ],

  webServer: {
    // Serve the nix-pinned jellyfin-web bundle, with `--proxy` forwarding
    // every non-static request (all Jellyfin REST paths — e.g.
    // /System/Info/Public, /Items/.../Images/...) to the running pharos
    // instance. The browser thus sees one same-origin server, exactly how
    // real Jellyfin hosts jellyfin-web. Without the proxy, jellyfin-web's
    // boot-time same-origin probe (`/System/Info/Public`) 404s against the
    // dumb static server.
    command: `npx http-server ${JELLYFIN_WEB_DIR} -p ${JELLYFIN_WEB_PORT} -s --cors --proxy ${PHAROS_URL}?`,
    port: JELLYFIN_WEB_PORT,
    reuseExistingServer: true,
    timeout: 30_000,
  },

  metadata: {
    pharos_url: PHAROS_URL,
  },
});
