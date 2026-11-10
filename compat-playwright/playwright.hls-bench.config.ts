import { defineConfig, devices } from "@playwright/test";
import dioxus from "./playwright.dioxus.config";

// The HLS bench (spec 031 T031-02) in Chromium and Firefox, one engine run
// after another on the same server:
//   PHAROS_HLS_BENCH=1 just compat-dioxus-full on 8096 playwright.hls-bench.config.ts
// The spec sets its own launch options: autoplay allowed, audio muted, and
// Firefox's libavcodec (`groupLaunchOptions` in tests/lib/together.ts).

export default defineConfig({
  ...dioxus,
  testMatch: /\/hls-bench\.spec\.ts$/,
  retries: 0,
  projects: [
    {
      name: "chromium",
      use: { ...devices["Desktop Chrome"], channel: "chromium" },
    },
    { name: "firefox", use: { ...devices["Desktop Firefox"] } },
  ],
});
