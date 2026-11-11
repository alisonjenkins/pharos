import { readFileSync } from "node:fs";
import path from "node:path";

// B1332 / V1303: the TV app's remote script, as the TV runs it, for a
// browser page. The TV app (spec 025) drives the same Dioxus UI the web
// build draws, through the system WebView, and one long-lived script
// (`BRIDGE_JS` in crates/pharos-ui/src/native/android_page.rs) decides what
// the remote's arrows, OK and Back do before the page hears them. That
// file compiles only for Android, so no host or browser test ever ran the
// script, and fixes made in it (the seek bar as a D-pad stop, OK on a
// search match) came back unnoticed.
//
// This reads the script out of the Rust source and fills its placeholders
// the way `tv_page::bridge_script` does, by reading that function's
// `.replace` calls and the constants they name, so a new placeholder or a
// changed selector reaches the tests with no copy kept here. Anything it
// cannot resolve throws, naming what it could not find.

const REPO = path.resolve(__dirname, "../../..");
const NATIVE = path.join(REPO, "crates/pharos-ui/src/native");

function read(file: string): string {
  return readFileSync(path.join(NATIVE, file), "utf8");
}

function unescapeRust(s: string): string {
  return s.replace(/\\(["\\nt])/g, (_, c: string) =>
    c === "n" ? "\n" : c === "t" ? "\t" : c,
  );
}

/** The value of the Rust constant `name` in `src`: a raw or plain string, or a number. */
function rustConst(src: string, name: string): string {
  const raw = new RegExp(`(?:pub )?const ${name}: &str = r#"([\\s\\S]*?)"#;`).exec(src);
  if (raw) return raw[1];
  const plain = new RegExp(`(?:pub )?const ${name}: &str = "((?:[^"\\\\]|\\\\.)*)";`).exec(src);
  if (plain) return unescapeRust(plain[1]);
  const num = new RegExp(`(?:pub )?const ${name}: u\\d+ = ([\\d_]+);`).exec(src);
  if (num) return num[1].replace(/_/g, "");
  throw new Error(`tv-bridge: no constant ${name} in tv_page.rs`);
}

/** `BRIDGE_JS` from android_page.rs, filled in as `tv_page::bridge_script` fills it. */
export function bridgeScript(): string {
  const page = read("android_page.rs");
  const template = /const BRIDGE_JS: &str = r#"\n([\s\S]*?)\n"#;/.exec(page)?.[1];
  if (!template) throw new Error("tv-bridge: no BRIDGE_JS in android_page.rs");
  const tv = read("tv_page.rs");
  const body = /pub fn bridge_script\(template: &str\) -> String \{([\s\S]*?)\n\}/.exec(tv)?.[1];
  if (!body) throw new Error("tv-bridge: no bridge_script in tv_page.rs");
  let script = template;
  const fills = [...body.matchAll(/\.replace\("(__\w+__)",\s*&?(\w+)(?:\.to_string\(\))?\)/g)];
  if (fills.length === 0) throw new Error("tv-bridge: bridge_script fills nothing");
  for (const [, placeholder, name] of fills) {
    script = script.split(placeholder).join(rustConst(tv, name));
  }
  const left = /__[A-Z][A-Z0-9_]*__/.exec(script);
  if (left) throw new Error(`tv-bridge: ${left[0]} is left unfilled`);
  return script;
}

// What the TV's Rust side does with a key or a media event, the web build
// does in its own listeners; two lines of the script exist only because
// Rust stands in for them on the TV, and would silence the web build's:
//
// - an arrow's preventDefault, which keeps the WebView's own spatial
//   navigation from moving the focus a second time (Chromium has none),
//   and which the web build's key listener (`on_mode_key`) would read as
//   "taken" and ignore;
// - timeupdate's stopPropagation, which keeps Dioxus from rendering four
//   times a second where Rust hears a tick instead; the web build's player
//   hears its position from those events.
//
// Each is matched exactly once, so a reworded line fails here loudly
// rather than leaving a page that silently behaves unlike the TV's.
const WEB_SHIMS: [string, string][] = [
  [
    "if (!text && takes.has(e.key)) e.preventDefault();",
    "/* web: the page's own listeners take the arrows */",
  ],
  [
    "if (e.type === 'timeupdate') {\n    e.stopPropagation();",
    "if (e.type === 'timeupdate') {",
  ],
];

/**
 * An init script that installs the TV's remote script in the page before the
 * app starts, as the TV app does at launch. What it would send to Rust is
 * kept in `window.__tvBridgeSent` for a test to read.
 */
export function tvBridgeInitScript(): string {
  let script = bridgeScript();
  for (const [from, to] of WEB_SHIMS) {
    const n = script.split(from).length - 1;
    if (n !== 1) throw new Error(`tv-bridge: web shim matched ${n} times: ${from}`);
    script = script.replace(from, to);
  }
  return `(() => {
  window.__tvBridgeSent = [];
  const dioxus = { send: (v) => window.__tvBridgeSent.push(v) };
  const start = async () => {
${script}
  };
  const run = () => start().catch((e) => console.error("tv-bridge: " + e));
  // The script reads the document and history; the TV installs it once the
  // page exists, so wait for that here too.
  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", run, { once: true });
  } else {
    run();
  }
})();`;
}
