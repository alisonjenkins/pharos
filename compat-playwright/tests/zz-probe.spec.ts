import { test } from "@playwright/test";
const BASE = process.env.AUDIT_BASE!;
test("probe", async ({ page }) => {
  test.setTimeout(120000);
  await page.goto(`${BASE}/web/index.html`, { waitUntil: "domcontentloaded" });
  await page.waitForTimeout(5000);
  const globals = await page.evaluate(() =>
    Object.keys(window).filter((k) => /api|connection|client/i.test(k)),
  );
  console.log("globals:", JSON.stringify(globals));
  const ok = await page.evaluate(
    ({ token, user }) => {
      const w = window as unknown as Record<string, any>;
      const ac = w.ApiClient;
      if (!ac) return "no ApiClient";
      try {
        ac.setAuthenticationInfo(token, user);
        return "set ok";
      } catch (e) {
        return `throw: ${e}`;
      }
    },
    { token: process.env.AUDIT_TOKEN, user: process.env.AUDIT_USER },
  );
  console.log("setAuth:", ok);
  await page.goto(`${BASE}/web/index.html#/home.html`, {
    waitUntil: "domcontentloaded",
  });
  await page.waitForTimeout(8000);
  console.log("URL:", page.url());
  console.log("cards:", await page.locator(".card").count());
  console.log(
    "body:",
    (await page.locator("body").innerText()).slice(0, 200).replace(/\n+/g, " | "),
  );
});
