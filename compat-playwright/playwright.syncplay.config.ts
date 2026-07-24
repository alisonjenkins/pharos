import { defineConfig, devices } from "@playwright/test";

// SyncPlay group-watch harness config. Two projects:
//   - matrix: the 7-scenario group matrix on the nix-pinned FOSS chromium
//             (VP9 — the codec that browser advertises).
//   - h264:   the demuxed-CMAF decode + audio-swap smoke on an h264-capable
//             chromium (PHAROS_H264_BROWSER, exported by the devShell).
// pharos + jellyfin-web are started by `just compat-syncplay`; the webServer
// below serves the jellyfin-web bundle with a REST proxy to pharos.

const PHAROS_URL = process.env.PHAROS_URL ?? "http://127.0.0.1:8096";
const JELLYFIN_WEB_PORT = parseInt(process.env.JELLYFIN_WEB_PORT ?? "8910", 10);
const JELLYFIN_WEB_DIR = process.env.JELLYFIN_WEB_DIR;
if (!JELLYFIN_WEB_DIR) {
  throw new Error("JELLYFIN_WEB_DIR not set — enter the nix devShell (`nix develop`).");
}
const H264_BROWSER = process.env.PHAROS_H264_BROWSER;

export default defineConfig({
  testDir: "./tests",
  fullyParallel: false,
  workers: 1,
  retries: process.env.CI ? 2 : 0,
  reporter: process.env.CI ? "github" : "list",
  timeout: 150_000,

  use: {
    baseURL: `http://127.0.0.1:${JELLYFIN_WEB_PORT}`,
    trace: "retain-on-failure",
    screenshot: "only-on-failure",
    video: "retain-on-failure",
    extraHTTPHeaders: { "X-Pharos-Compat-Suite": "playwright" },
  },

  projects: [
    {
      name: "matrix",
      testMatch: /syncplay-group-matrix\.spec\.ts/,
      use: { ...devices["Desktop Chrome"], channel: "chromium" },
    },
    {
      name: "h264",
      testMatch: /syncplay-h264-codec\.spec\.ts/,
      use: {
        ...devices["Desktop Chrome"],
        launchOptions: H264_BROWSER ? { executablePath: H264_BROWSER } : {},
      },
    },
  ],

  webServer: {
    command: `npx http-server ${JELLYFIN_WEB_DIR} -p ${JELLYFIN_WEB_PORT} -s --cors --proxy ${PHAROS_URL}?`,
    port: JELLYFIN_WEB_PORT,
    reuseExistingServer: true,
    timeout: 30_000,
  },
});
