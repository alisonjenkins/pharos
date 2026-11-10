import { spawn, type ChildProcess } from "node:child_process";
import { connect } from "node:net";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

/**
 * Firefox's loader path for a test run: the libavcodec the dev shell names
 * (`PHAROS_FIREFOX_LIBAV`), so it decodes AAC and H.264, and libpulse
 * (`PHAROS_FIREFOX_PULSE`), so it can open a sound output. NixOS puts
 * neither on the path. Without libpulse every `AudioContext` stays
 * "suspended" and its `resume()` never settles (B1140).
 */
export function firefoxLibraryPath(): string | undefined {
  const dirs = [
    process.env.PHAROS_FIREFOX_LIBAV,
    process.env.PHAROS_FIREFOX_PULSE,
    process.env.LD_LIBRARY_PATH,
  ].filter(Boolean);
  return dirs.length ? dirs.join(":") : undefined;
}

/** Firefox launch `env` with {@link firefoxLibraryPath} on it, or `{}`. */
export function firefoxEnv(): { env?: Record<string, string | undefined> } {
  const path = firefoxLibraryPath();
  return path ? { env: { ...process.env, LD_LIBRARY_PATH: path } } : {};
}

const SOCKET_WAIT_MS = 10_000;

/**
 * Playwright `globalSetup`: start a PulseAudio daemon whose only sink is a
 * null sink, and point the browsers at it through `PULSE_SERVER`. A CI pod
 * or a headless box has no sound server, and Firefox, unlike Chromium,
 * has no fake output: its `AudioContext` never runs, so every music test
 * that reads the visualiser tap fails (B1140, V1100). Chromium ignores it.
 *
 * Skips, saying why, when `PULSE_SERVER` is already set (a desktop with a
 * real server) or the dev shell named no daemon (`PHAROS_PULSEAUDIO`).
 * Returns the teardown.
 */
export default async function nullAudioSink(): Promise<() => Promise<void>> {
  const none = async () => {};
  if (process.env.PULSE_SERVER) {
    console.log(`null-audio-sink: skipped reason=pulse_server_set server=${process.env.PULSE_SERVER}`);
    return none;
  }
  const daemon = process.env.PHAROS_PULSEAUDIO;
  if (!daemon) {
    console.log("null-audio-sink: skipped reason=no_daemon (PHAROS_PULSEAUDIO unset; Firefox will have no sound output)");
    return none;
  }
  const dir = mkdtempSync(join(tmpdir(), "pharos-pulse-"));
  const socket = join(dir, "native");
  const child: ChildProcess = spawn(
    daemon,
    [
      "--daemonize=no",
      "-n",
      "--exit-idle-time=-1",
      "--use-pid-file=no",
      "--disallow-module-loading",
      `--load=module-null-sink sink_name=pharos_null`,
      `--load=module-native-protocol-unix auth-anonymous=1 socket=${socket}`,
    ],
    {
      // Its runtime, state and cookie files stay in the scratch directory.
      env: { ...process.env, XDG_RUNTIME_DIR: dir, HOME: dir, PULSE_RUNTIME_PATH: dir },
      stdio: ["ignore", "ignore", "pipe"],
    },
  );
  let stderr = "";
  let spawnError = "";
  child.stderr?.on("data", (d) => (stderr += String(d)));
  // A missing or unrunnable daemon is an `error` event, not an exit.
  child.on("error", (e) => (spawnError = e.message));
  const started = Date.now();
  // The socket file exists from bind(), before listen(): ready is when a
  // connection is accepted, not when the file appears.
  while (!(await accepts(socket))) {
    if (spawnError || child.exitCode !== null || Date.now() - started > SOCKET_WAIT_MS) {
      child.kill();
      rmSync(dir, { recursive: true, force: true });
      throw new Error(
        `null-audio-sink: failed daemon=${daemon} exit=${child.exitCode} spawn_error=${spawnError || "none"} waited_ms=${Date.now() - started} stderr=${stderr.trim()}`,
      );
    }
    await new Promise((r) => setTimeout(r, 50));
  }
  process.env.PULSE_SERVER = `unix:${socket}`;
  console.log(`null-audio-sink: started server=${process.env.PULSE_SERVER} pid=${child.pid} wait_ms=${Date.now() - started}`);
  return async () => {
    child.kill();
    rmSync(dir, { recursive: true, force: true });
  };
}

/** Whether the unix socket at `path` accepts a connection now. */
function accepts(path: string): Promise<boolean> {
  return new Promise((resolve) => {
    const probe = connect(path);
    probe.once("connect", () => {
      probe.destroy();
      resolve(true);
    });
    probe.once("error", () => resolve(false));
  });
}
