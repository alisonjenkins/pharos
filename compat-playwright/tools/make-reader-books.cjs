#!/usr/bin/env node
// Writes the books the reader's Playwright spec opens (spec 023) into the
// directory given as the first argument:
//
//   Reader Fixture.epub  three chapters of long paragraphs, so a section
//                        spans many pages at any window size, and every word
//                        is a unique token ("zq1x4x17": chapter 1,
//                        paragraph 4, word 17) the spec can find on screen.
//   Reader Paged.pdf     five pages ("Page 1 of 5" in large type, and a row
//                        of squares counting the page), written by hand
//                        below: Helvetica, no library.
//   Reader Second.epub   the same book under another title, for the cases that
//                        need two books (last-N, spec 036).
//   Reader Sized A/B/C.epub  about 800 KiB each, for the space budget cases
//                        (spec 036): two fit in 2 MiB, three do not.
//   Reader Locked.azw3   bytes the Kindle converter cannot read, which the
//                        scanner indexes as an unreadable book.
//   Reader Links.epub    a contents page whose links reach deep into a long
//                        chapter, to an id and to an old <a name>, and a
//                        footnote in a non-linear notes file (V1286-V1288).
//                        Its words are "zl<paragraph>x<word>".
//
// Node only: the devShell has it, and the EPUB is a stored (uncompressed)
// zip, which needs no library.

"use strict";
const fs = require("fs");
const path = require("path");

const CRC_TABLE = (() => {
  const t = new Uint32Array(256);
  for (let n = 0; n < 256; n++) {
    let c = n;
    for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
    t[n] = c >>> 0;
  }
  return t;
})();

function crc32(buf) {
  let c = 0xffffffff;
  for (const b of buf) c = CRC_TABLE[(c ^ b) & 0xff] ^ (c >>> 8);
  return (c ^ 0xffffffff) >>> 0;
}

/** A zip of `entries` ([name, string]) with every entry stored. */
function zip(entries) {
  const locals = [];
  const centrals = [];
  let offset = 0;
  for (const [name, text] of entries) {
    const nameBuf = Buffer.from(name, "utf8");
    const data = Buffer.from(text, "utf8");
    const crc = crc32(data);
    const local = Buffer.alloc(30);
    local.writeUInt32LE(0x04034b50, 0);
    local.writeUInt16LE(20, 4); // version needed
    local.writeUInt16LE(0, 6); // flags
    local.writeUInt16LE(0, 8); // stored
    local.writeUInt16LE(0, 10); // time
    local.writeUInt16LE(0x21, 12); // date: 1980-01-01
    local.writeUInt32LE(crc, 14);
    local.writeUInt32LE(data.length, 18);
    local.writeUInt32LE(data.length, 22);
    local.writeUInt16LE(nameBuf.length, 26);
    local.writeUInt16LE(0, 28);
    const central = Buffer.alloc(46);
    central.writeUInt32LE(0x02014b50, 0);
    central.writeUInt16LE(20, 4); // version made by
    central.writeUInt16LE(20, 6); // version needed
    central.writeUInt16LE(0, 8);
    central.writeUInt16LE(0, 10);
    central.writeUInt16LE(0, 12);
    central.writeUInt16LE(0x21, 14);
    central.writeUInt32LE(crc, 16);
    central.writeUInt32LE(data.length, 20);
    central.writeUInt32LE(data.length, 24);
    central.writeUInt16LE(nameBuf.length, 28);
    central.writeUInt32LE(offset, 42);
    locals.push(local, nameBuf, data);
    centrals.push(central, nameBuf);
    offset += local.length + nameBuf.length + data.length;
  }
  const directory = Buffer.concat(centrals);
  const end = Buffer.alloc(22);
  end.writeUInt32LE(0x06054b50, 0);
  end.writeUInt16LE(entries.length, 8);
  end.writeUInt16LE(entries.length, 10);
  end.writeUInt32LE(directory.length, 12);
  end.writeUInt32LE(offset, 16);
  return Buffer.concat([...locals, directory, end]);
}

const CHAPTERS = 3;
const PARAGRAPHS = 40;
// Each paragraph of 70 tokens is about 0.7 kB; 380 of them in each of three
// chapters is about 800 KiB.
const SIZED_PARAGRAPHS = 380;
const WORDS = 70;

function chapter(c, count) {
  const paragraphs = [];
  for (let p = 0; p < count; p++) {
    const words = [];
    for (let w = 0; w < WORDS; w++) words.push(`zq${c}x${p}x${w}`);
    paragraphs.push(`<p>${words.join(" ")}</p>`);
  }
  return (
    `<?xml version="1.0" encoding="UTF-8"?>\n` +
    `<html xmlns="http://www.w3.org/1999/xhtml"><head><title>Chapter ${c + 1}</title></head>` +
    `<body><h1>Chapter ${c + 1}</h1>${paragraphs.join("")}</body></html>`
  );
}

function epub(title = "Reader Fixture", count = PARAGRAPHS) {
  const items = [];
  const refs = [];
  const nav = [];
  for (let c = 0; c < CHAPTERS; c++) {
    items.push(`<item id="c${c}" href="c${c}.xhtml" media-type="application/xhtml+xml"/>`);
    refs.push(`<itemref idref="c${c}"/>`);
    nav.push(`<li><a href="c${c}.xhtml">Chapter ${c + 1}</a></li>`);
  }
  return zip([
    ["mimetype", "application/epub+zip"],
    [
      "META-INF/container.xml",
      `<?xml version="1.0"?><container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">` +
        `<rootfiles><rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/></rootfiles></container>`,
    ],
    [
      "OEBPS/content.opf",
      `<?xml version="1.0" encoding="UTF-8"?>` +
        `<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="uid">` +
        `<metadata xmlns:dc="http://purl.org/dc/elements/1.1/">` +
        `<dc:title>${title}</dc:title><dc:creator>Ada Lovelace</dc:creator>` +
        `<dc:language>en</dc:language><dc:identifier id="uid">urn:pharos:${title.toLowerCase().replace(/ /g, "-")}</dc:identifier></metadata>` +
        `<manifest><item id="nav" href="nav.xhtml" media-type="application/xhtml+xml" properties="nav"/>` +
        `${items.join("")}</manifest><spine>${refs.join("")}</spine></package>`,
    ],
    [
      "OEBPS/nav.xhtml",
      `<?xml version="1.0" encoding="UTF-8"?>` +
        `<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops"><head><title>Contents</title></head>` +
        `<body><nav epub:type="toc"><ol>${nav.join("")}</ol></nav></body></html>`,
    ],
    ...Array.from({ length: CHAPTERS }, (_, c) => [`OEBPS/c${c}.xhtml`, chapter(c, count)]),
  ]);
}

const LINKED_PARAGRAPHS = 60;

/** The book of links: contents, one long chapter, and notes outside the reading order. */
function linkedEpub() {
  const xhtml = (title, body) =>
    `<?xml version="1.0" encoding="UTF-8"?>\n` +
    `<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops"><head><title>${title}</title></head>` +
    `<body>${body}</body></html>`;
  const paragraphs = [];
  for (let p = 0; p < LINKED_PARAGRAPHS; p++) {
    const words = [];
    for (let w = 0; w < WORDS; w++) words.push(`zl${p}x${w}`);
    const id = p === 30 ? ' id="deep"' : "";
    const old = p === 50 ? '<a name="old"></a>' : "";
    const note = p === 0 ? ' <a id="noteref" href="../notes.xhtml#n1" epub:type="noteref">1</a>' : "";
    paragraphs.push(`<p${id}>${old}${words.join(" ")}${note}</p>`);
  }
  return zip([
    ["mimetype", "application/epub+zip"],
    [
      "META-INF/container.xml",
      `<?xml version="1.0"?><container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">` +
        `<rootfiles><rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/></rootfiles></container>`,
    ],
    [
      "OEBPS/content.opf",
      `<?xml version="1.0" encoding="UTF-8"?>` +
        `<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="uid">` +
        `<metadata xmlns:dc="http://purl.org/dc/elements/1.1/">` +
        `<dc:title>Reader Links</dc:title><dc:creator>Ada Lovelace</dc:creator>` +
        `<dc:language>en</dc:language><dc:identifier id="uid">urn:pharos:reader-links</dc:identifier></metadata>` +
        `<manifest><item id="nav" href="nav.xhtml" media-type="application/xhtml+xml" properties="nav"/>` +
        `<item id="contents" href="contents.xhtml" media-type="application/xhtml+xml"/>` +
        `<item id="chapter" href="text/chapter%20one.xhtml" media-type="application/xhtml+xml"/>` +
        `<item id="notes" href="notes.xhtml" media-type="application/xhtml+xml"/></manifest>` +
        `<spine><itemref idref="contents"/><itemref idref="chapter"/><itemref idref="notes" linear="no"/></spine></package>`,
    ],
    [
      "OEBPS/nav.xhtml",
      xhtml("Contents", `<nav epub:type="toc"><ol><li><a href="contents.xhtml">Contents</a></li>` +
        `<li><a href="text/chapter%20one.xhtml">Chapter One</a></li></ol></nav>`),
    ],
    [
      "OEBPS/contents.xhtml",
      xhtml("Contents", `<h1>Contents</h1>` +
        `<p><a id="to-deep" href="text/chapter%20one.xhtml#deep">The deep section</a></p>` +
        `<p><a id="to-old" href="text/chapter%20one.xhtml#old">The old anchor</a></p>` +
        `<p><a id="to-web" href="https://example.org/">A web page</a></p>`),
    ],
    ["OEBPS/text/chapter one.xhtml", xhtml("Chapter One", `<h1>Chapter One</h1>${paragraphs.join("")}`)],
    ["OEBPS/notes.xhtml", xhtml("Notes", `<h1>Notes</h1><p id="n1">zlnote the first note.</p>`)],
  ]);
}

const PDF_PAGES = 5;

/** A PDF of `PDF_PAGES` letter pages, each saying which it is and showing that many squares. */
function pdf() {
  const objects = [];
  const add = (body) => objects.push(body);
  add("<< /Type /Catalog /Pages 2 0 R >>");
  const kids = Array.from({ length: PDF_PAGES }, (_, i) => `${4 + 2 * i} 0 R`).join(" ");
  add(`<< /Type /Pages /Kids [${kids}] /Count ${PDF_PAGES} >>`);
  add("<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>");
  for (let i = 0; i < PDF_PAGES; i++) {
    add(
      `<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents ${5 + 2 * i} 0 R ` +
        `/Resources << /Font << /F1 3 0 R >> >> >>`,
    );
    let content = `BT /F1 64 Tf 72 600 Td (Page ${i + 1} of ${PDF_PAGES}) Tj ET\n`;
    for (let q = 0; q <= i; q++) content += `0.1 0.2 0.6 rg ${72 + q * 80} 120 60 60 re f\n`;
    add(`<< /Length ${Buffer.byteLength(content)} >>\nstream\n${content}endstream`);
  }
  let out = "%PDF-1.4\n";
  const offsets = [];
  objects.forEach((body, i) => {
    offsets.push(Buffer.byteLength(out));
    out += `${i + 1} 0 obj\n${body}\nendobj\n`;
  });
  const xref = Buffer.byteLength(out);
  out += `xref\n0 ${objects.length + 1}\n0000000000 65535 f \n`;
  for (const at of offsets) out += `${String(at).padStart(10, "0")} 00000 n \n`;
  out += `trailer\n<< /Size ${objects.length + 1} /Root 1 0 R >>\nstartxref\n${xref}\n%%EOF\n`;
  return Buffer.from(out, "latin1");
}

const out = process.argv[2];
if (!out) {
  console.error("usage: make-reader-books.cjs <directory>");
  process.exit(2);
}
fs.mkdirSync(out, { recursive: true });
fs.writeFileSync(path.join(out, "Reader Fixture.epub"), epub());
fs.writeFileSync(path.join(out, "Reader Second.epub"), epub("Reader Second"));
for (const x of ["A", "B", "C"]) {
  fs.writeFileSync(path.join(out, `Reader Sized ${x}.epub`), epub(`Reader Sized ${x}`, SIZED_PARAGRAPHS));
}
fs.writeFileSync(path.join(out, "Reader Paged.pdf"), pdf());
fs.writeFileSync(path.join(out, "Reader Links.epub"), linkedEpub());
fs.writeFileSync(path.join(out, "Reader Locked.azw3"), Buffer.from("not a mobi file: DRM stand-in\n"));
