import { defineConfig, devices } from "@playwright/test";
import dioxus from "./playwright.dioxus.config";
import { firefoxEnv } from "./tests/lib/audio-sink";

// The whole Dioxus suite in Firefox, with audio muted and autoplay allowed.
// CI runs the suite in Chromium only; run this one through
// `just compat-dioxus-full on 8096 playwright.dioxus-firefox.config.ts`.
//
// Playwright's Firefox decodes AAC, H.264 and Opus in MP4 through the host's
// libavcodec, and opens a sound output through libpulse; NixOS puts neither
// on the loader path. `firefoxEnv()` adds both. Without libavcodec every HLS
// stream falls back (Rust engine, `cause=codec`) or fails (hls.js,
// `manifestIncompatibleCodecsError`). Without a sound output its
// AudioContexts never run, so the music specs fail: `tests/lib/audio-sink.ts`
// starts a null sink for the run (B1140).

export default defineConfig({
  ...dioxus,
  globalSetup: "./tests/lib/audio-sink.ts",
  projects: [
    {
      name: "firefox",
      use: {
        ...devices["Desktop Firefox"],
        launchOptions: {
          firefoxUserPrefs: {
            "media.autoplay.default": 0,
            "media.autoplay.blocking_policy": 0,
            "media.volume_scale": "0.0",
          },
          ...firefoxEnv(),
        },
      },
    },
  ],
});
