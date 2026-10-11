// Regression test: a rejected or reset WebSocket upgrade must not crash jf-proxy.
// Run: node --test compat-playwright/tools/jf-proxy.test.cjs
const test = require("node:test");
const assert = require("node:assert");
const net = require("net");
const http = require("http");
const os = require("os");
const path = require("path");
const fs = require("fs");
const { spawn } = require("child_process");

function freePort() {
  return new Promise((resolve) => {
    const s = net.createServer().listen(0, "127.0.0.1", () => {
      const { port } = s.address();
      s.close(() => resolve(port));
    });
  });
}

async function startProxy(upstreamPort) {
  const port = await freePort();
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "jf-proxy-test-"));
  fs.writeFileSync(path.join(dir, "index.html"), "ok");
  const child = spawn(process.execPath, [path.join(__dirname, "jf-proxy.cjs")], {
    env: {
      ...process.env,
      JELLYFIN_WEB_DIR: dir,
      JELLYFIN_WEB_PORT: String(port),
      PHAROS_URL: `http://127.0.0.1:${upstreamPort}`,
    },
    stdio: ["ignore", "pipe", "pipe"],
  });
  let stderr = "";
  child.stderr.on("data", (d) => (stderr += d));
  await new Promise((resolve, reject) => {
    child.once("exit", () => reject(new Error("proxy exited early: " + stderr)));
    child.stdout.on("data", (d) => String(d).includes("listening") && resolve());
  });
  return { child, port, stderr: () => stderr };
}

const UPGRADE =
  "GET /socket HTTP/1.1\r\nHost: x\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n" +
  "Sec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n";

async function assertAlive(proxy) {
  await new Promise((r) => setTimeout(r, 200));
  assert.strictEqual(proxy.child.exitCode, null, "proxy died: " + proxy.stderr());
  const status = await new Promise((resolve, reject) =>
    http
      .get({ port: proxy.port, host: "127.0.0.1", path: "/" }, (res) => {
        res.resume();
        resolve(res.statusCode);
      })
      .on("error", reject),
  );
  assert.strictEqual(status, 200);
}

function rawUpstream(onConn) {
  const srv = net.createServer(onConn);
  return new Promise((resolve) => srv.listen(0, "127.0.0.1", () => resolve(srv)));
}

test("upstream 401 then reset on upgrade, client resets: proxy survives", async () => {
  const up = await rawUpstream((c) => {
    c.on("error", () => {});
    c.once("data", () => {
      c.write("HTTP/1.1 401 Unauthorized\r\nContent-Length: 100\r\n\r\npartial");
      setTimeout(() => c.resetAndDestroy(), 20);
    });
  });
  const proxy = await startProxy(up.address().port);
  try {
    const c = net.connect(proxy.port, "127.0.0.1");
    c.on("error", () => {});
    c.write(UPGRADE);
    await new Promise((r) => setTimeout(r, 100));
    c.resetAndDestroy();
    await assertAlive(proxy);
  } finally {
    proxy.child.kill();
    up.close();
  }
});

test("rejected upgrade relays the status to the client", async () => {
  const up = await rawUpstream((c) => {
    c.on("error", () => {});
    c.once("data", () => c.end("HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\n\r\n"));
  });
  const proxy = await startProxy(up.address().port);
  try {
    const c = net.connect(proxy.port, "127.0.0.1");
    c.write(UPGRADE);
    let buf = "";
    c.on("data", (d) => (buf += d));
    await new Promise((r) => c.on("close", r));
    assert.match(buf, /^HTTP\/1\.1 401 /);
    await assertAlive(proxy);
  } finally {
    proxy.child.kill();
    up.close();
  }
});

test("client closes before upstream replies: upstream connection is closed", async () => {
  let upstreamClosed;
  const closed = new Promise((r) => (upstreamClosed = r));
  const up = await rawUpstream((c) => {
    c.on("error", () => {});
    c.on("data", () => {});
    c.on("close", () => upstreamClosed());
  });
  const proxy = await startProxy(up.address().port);
  try {
    const c = net.connect(proxy.port, "127.0.0.1");
    c.on("error", () => {});
    c.write(UPGRADE);
    await new Promise((r) => setTimeout(r, 100));
    c.end();
    const timedOut = new Promise((r) => setTimeout(() => r("timeout"), 1000));
    const result = await Promise.race([closed.then(() => "closed"), timedOut]);
    assert.strictEqual(result, "closed", "upstream connection left open after client reset");
  } finally {
    proxy.child.kill();
    up.close();
  }
});

test("client resets mid-upgrade while upstream is slow: proxy survives", async () => {
  const up = await rawUpstream((c) => c.on("error", () => {}));
  const proxy = await startProxy(up.address().port);
  try {
    const c = net.connect(proxy.port, "127.0.0.1");
    c.on("error", () => {});
    c.write(UPGRADE);
    await new Promise((r) => setTimeout(r, 100));
    c.resetAndDestroy();
    await assertAlive(proxy);
  } finally {
    proxy.child.kill();
    up.close();
  }
});
