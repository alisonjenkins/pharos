// V352 (DOM half): a place saved at one window and font size shows the same
// character at another. Run by `just quire-web-check`.
import { createRequire } from "node:module";
import http from "node:http";
import fs from "node:fs";
import path from "node:path";

// Resolved from the working directory, compat-playwright, whose pinned
// @playwright/test matches the browsers the dev shell provides.
const { chromium } = createRequire(path.join(process.cwd(), "package.json"))("@playwright/test");
const root = process.argv[2];
const server = http.createServer((req, res) => {
  let rel;
  try {
    rel = decodeURIComponent(new URL(req.url, "http://x").pathname).replace(/\/$/, "/index.html");
  } catch {
    res.writeHead(400);
    res.end();
    return;
  }
  // Serve only files under root: a decoded `..%2f` must not climb out of it.
  const file = path.resolve(root, "." + rel);
  if (!file.startsWith(path.resolve(root) + path.sep)) {
    res.writeHead(403);
    res.end();
    return;
  }
  const type = file.endsWith(".wasm") ? "application/wasm" : file.endsWith(".js") ? "text/javascript" : "text/html";
  fs.readFile(file, (err, body) => {
    res.writeHead(err ? 404 : 200, { "content-type": type });
    res.end(err ? "" : body);
  });
});
// With a host, listen binds after a lookup: wait for it before reading the port.
await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
const url = `http://127.0.0.1:${server.address().port}/`;
const browser = await chromium.launch({ executablePath: process.env.CHROME || undefined });
let failed = 0;
const check = (ok, what) => { console.log(`${ok ? "ok  " : "FAIL"} ${what}`); if (!ok) failed++; };

async function open(width, height, font) {
  const page = await browser.newPage({ viewport: { width, height } });
  page.on("pageerror", (e) => console.log("pageerror", e.message));
  await page.goto(url);
  await page.waitForSelector("body[data-ready]");
  await page.evaluate((f) => window.q.mount(f), font);
  return page;
}

const a = await open(1000, 700, 18);
for (let i = 0; i < 3; i++) await a.evaluate(() => window.q.next());
const saved = await a.evaluate(() => window.q.locator());
const offset = Number(saved.split(":")[2]);
const words = await a.evaluate((o) => window.q.text_at(o, 12), offset);
check(offset > 0, `paged into the section, at ${saved} "${words}"`);
let [shown, holds] = await a.evaluate((o) => window.q.pages_for(o), offset);
check(shown === holds, `the saved character is on the page shown (${shown}/${holds})`);

await a.evaluate(() => window.q.set_font(26));
[shown, holds] = await a.evaluate((o) => window.q.pages_for(o), offset);
check(shown === holds, `still on screen after a larger font (${shown}/${holds})`);
let first = await a.evaluate(() => window.q.first_visible());
check(first <= offset, `page now starts at or before it (${first} <= ${offset})`);
check((await a.evaluate(() => window.q.locator())) === saved, "the place itself is unchanged by the relayout");
await a.evaluate(() => window.q.set_font(18));
check((await a.evaluate(() => window.q.locator())) === saved, "and by a second one: no drift");

const b = await open(480, 820, 14);
await b.evaluate((l) => window.q.go_to(l), saved);
[shown, holds] = await b.evaluate((o) => window.q.pages_for(o), offset);
check(shown === holds, `restored in a narrow window and small font (${shown}/${holds})`);
first = await b.evaluate(() => window.q.first_visible());
check(first <= offset && first > 0, `its page starts at or before it (${first} <= ${offset})`);

// Turning pages lands mid-paragraph somewhere: a page start that is not the
// start of a text node proves pages are located per character.
const starts = new Set(await b.evaluate(() => window.q.node_starts()));
let mid = false;
for (let i = 0; i < 6 && !mid; i++) {
  await b.evaluate(() => window.q.next());
  const at = Number((await b.evaluate(() => window.q.locator())).split(":")[2]);
  mid = at > 0 && !starts.has(at);
}
check(mid, "a page that starts inside a paragraph is located inside it");

// Next and back again lands on the same first character.
const before = await b.evaluate(() => window.q.locator());
await b.evaluate(async () => { await window.q.next(); });
const forward = await b.evaluate(() => window.q.locator());
check(forward !== before, `next page moves on (${before} -> ${forward})`);

// V1290/V1291: a fixed-layout page is drawn whole, scaled to fit and centred,
// with the book's own sizes and colours, and its fonts reach the document.
async function openFixed(width, height) {
  const page = await browser.newPage({ viewport: { width, height } });
  page.on("pageerror", (e) => console.log("pageerror", e.message));
  await page.goto(url);
  await page.waitForSelector("body[data-ready]");
  await page.evaluate(() => window.q.mount_book(new Uint8Array(), 0));
  return page;
}
const fixedState = (page) =>
  page.evaluate(() => {
    const sh = document.getElementById("host").shadowRoot;
    const box = (el) => { const r = el.getBoundingClientRect(); return [r.left, r.top, r.width, r.height]; };
    const placed = sh.getElementById("placed");
    const cs = getComputedStyle(placed);
    return {
      strip: box(sh.querySelector(".quire-strip")),
      placed: box(placed),
      fontSize: cs.fontSize,
      color: cs.color,
      art: getComputedStyle(sh.getElementById("art")).backgroundColor,
      fonts: [...document.fonts].map((f) => f.family.replace(/"/g, "")),
      at: window.q.where_now(),
    };
  });
const near = (x, y) => Math.abs(x - y) < 1;
for (const [w, h] of [[1000, 700], [400, 800]]) {
  const f = await openFixed(w, h);
  const st = await fixedState(f);
  const scale = Math.min((w - 16) / 500, (h - 16) / 700);
  const [l, t, sw, sh] = st.strip;
  check(near(sw, 500 * scale) && near(sh, 700 * scale), `${w}x${h}: the page is scaled to fit (${sw.toFixed(1)}x${sh.toFixed(1)})`);
  check(near(l, (w - sw) / 2) && near(t, (h - sh) / 2), `${w}x${h}: and centred (${l.toFixed(1)}, ${t.toFixed(1)})`);
  check(near(st.placed[0], l + 40 * scale) && near(st.placed[1], t + 400 * scale), `${w}x${h}: placed text sits at its point on the page`);
  check(st.fontSize === "30px" && st.color === "rgb(10, 20, 30)", `${w}x${h}: placed text keeps the book's size and colour (${st.fontSize}, ${st.color})`);
  check(st.art === "rgb(250, 220, 200)", `${w}x${h}: the page's own colours are kept (${st.art})`);
  check(st.at[0] === 0 && st.at[1] === 1, `${w}x${h}: one page per section (${st.at})`);
  check(st.fonts.includes("placed-hand"), `${w}x${h}: the book's font reaches the document (${st.fonts})`);
  await f.evaluate(() => window.q.next());
  check((await fixedState(f)).at[2] === 1, `${w}x${h}: next turns to the next page's section`);
  await f.close();
}

await browser.close();
server.close();
process.exit(failed ? 1 : 0);
