import { Browser, expect } from "@playwright/test";

/** Fail loudly unless this browser can really decode h264 over MSE. The
 *  h264 specs are meaningless on the FOSS Playwright chromium, which cannot,
 *  and a skipped assertion there would read as a pass. */
export async function requireH264(browser: Browser): Promise<void> {
  const ctx = await browser.newContext();
  const page = await ctx.newPage();
  await page.goto("about:blank");
  const canH264 = await page.evaluate(
    () =>
      (window as unknown as { MediaSource?: { isTypeSupported(t: string): boolean } })
        .MediaSource?.isTypeSupported('video/mp4; codecs="avc1.640028"') === true,
  );
  await ctx.close();
  expect(
    canH264,
    "PHAROS_H264_BROWSER lacks h264 decode — swap flake.nix to google-chrome " +
      "(allowUnfree) so the demuxed-CMAF path is exercised",
  ).toBe(true);
}
