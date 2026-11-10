import { defineConfig, devices } from "@playwright/test";
import dioxus from "./playwright.dioxus.config";

// Watch-together specs of the Dioxus suite in Chromium AND Firefox: a group
// is two browsers in step, and the browsers differ in how their media
// elements seek, buffer and report time. The Dioxus suite in CI runs these
// specs in Chromium only; this config adds Firefox without running the
// whole suite twice.
//
// Run through `just compat-dioxus-full on 8096 playwright.together.config.ts`
// (seeds pharos, including the users opted in to diagnostics). Each spec
// sets its own per-browser launch options (`groupLaunchOptions` in
// `tests/lib/together.ts`): autoplay allowed, audio muted. Firefox gets a
// null sound output from `tests/lib/audio-sink.ts` (B1140): without one its
// AudioContexts never run.

export default defineConfig({
  ...dioxus,
  testMatch: /\/[^/]*together[^/]*\.spec\.ts$/,
  timeout: 180_000,
  globalSetup: "./tests/lib/audio-sink.ts",
  projects: [
    { name: "chromium", use: { ...devices["Desktop Chrome"], channel: "chromium" } },
    { name: "firefox", use: { ...devices["Desktop Firefox"] } },
  ],
});
