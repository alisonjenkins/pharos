import { defineConfig, devices } from "@playwright/test";
import dioxus from "./playwright.dioxus.config";

// The whole Dioxus suite in Firefox, with audio muted and autoplay allowed.
// CI runs the suite in Chromium only; run this one through
// `just compat-dioxus-full on 8096 playwright.dioxus-firefox.config.ts`.
//
// Playwright's Firefox decodes AAC, H.264 and Opus in MP4 through the host's
// libavcodec, which NixOS does not put on the loader path. Without it,
// Firefox starts answering "no" to MP4 audio as soon as its media process is
// up. Every HLS stream then falls back (Rust engine, `cause=codec`) or fails
// (hls.js, `manifestIncompatibleCodecsError`). The dev shell names a
// libavcodec Firefox can load in `PHAROS_FIREFOX_LIBAV`.

const libav = process.env.PHAROS_FIREFOX_LIBAV;

export default defineConfig({
  ...dioxus,
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
          ...(libav
            ? {
                env: {
                  ...process.env,
                  LD_LIBRARY_PATH: [libav, process.env.LD_LIBRARY_PATH].filter(Boolean).join(":"),
                },
              }
            : {}),
        },
      },
    },
  ],
});
