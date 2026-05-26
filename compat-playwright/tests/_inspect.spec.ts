import { test } from "@playwright/test";
test("audio errors", async ({ page }) => {
  const errs: string[] = [];
  page.on("pageerror", e => errs.push(e.message.slice(0, 250)));
  page.on("console", m => {
    const t = m.text();
    if (m.type() === "error" && !t.includes("Failed to load resource")) {
      console.log("[" + m.type() + "]", t.slice(0, 250));
    }
  });
  await page.goto("/", { waitUntil: "networkidle" });
  await page.waitForTimeout(800);
  await page.getByText(/add server/i).click();
  await page.waitForTimeout(300);
  await page.locator("#txtServerHost").fill("http://127.0.0.1:18096");
  await page.getByRole("button", { name: /^connect$/i }).click();
  await page.waitForURL(/#\/login/);
  await page.locator("#txtManualName").fill("playwright");
  await page.locator("#txtManualPassword").fill("playwright-test-pw");
  await page.getByRole("button", { name: /^sign in$/i }).click();
  await page.waitForURL(/#\/home/);
  await page.goto("/#/details?id=4");
  await page.waitForTimeout(8000);
  console.log("PageErrors:");
  for (const e of errs) console.log("  ", e);
  console.log("URL:", page.url());
  const playBtn = await page.locator("button.btnPlay").count();
  console.log("btnPlay count:", playBtn);
});
