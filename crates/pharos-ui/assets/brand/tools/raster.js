// Renders the raster icons from the brand SVGs with headless Chromium.
// usage: node raster.js <assets/brand dir>   (needs playwright, then Pillow
// for the .ico: see ../README.md). Run build_brand.py first: it writes the
// _tile_*.svg sources this reads and removes nothing.
const { chromium } = require('playwright');
const fs = require('fs');
const dir = process.argv[2];
const jobs = [
  ['mark.svg', '_favicon-16.png', 16], ['mark.svg', '_favicon-32.png', 32], ['mark.svg', '_favicon-48.png', 48],
  ['_tile_full.svg', 'apple-touch-icon.png', 180], ['_tile_any.svg', 'icon-192.png', 192],
  ['_tile_any.svg', 'icon-512.png', 512], ['_tile_mask.svg', 'icon-maskable-512.png', 512],
];
(async () => {
  const b = await chromium.launch();
  const p = await b.newPage({ deviceScaleFactor: 1 });
  for (const [src, out, n] of jobs) {
    await p.setViewportSize({ width: n, height: n });
    await p.setContent(`<style>html,body{margin:0;background:transparent}svg{display:block;width:${n}px;height:${n}px}</style>` + fs.readFileSync(`${dir}/${src}`, 'utf8'));
    await p.screenshot({ path: `${dir}/${out}`, omitBackground: true });
  }
  await b.close();
})();
