# ADR-0151: The Pharos engine is the default HLS player

- **Status:** Accepted
- **Date:** 2026-11-22T17:30:00Z
- **Deciders:** Alison Jenkins

## Context

The goal is one HLS engine, written in Rust, on every client, with hls.js
removed (spec 031, ADR-0071). Until now hls.js played every MSE stream unless
a device's Playback setting or `?engine=rust` asked for the Rust engine, which
the setting called experimental.

The Rust engine now plays every stream shape the server makes, including
streams in several parts (V1504), and on desktop Chromium and Firefox it
matched or beat hls.js in the bench (`hls-bench.spec.ts`) on start time, seek
time and stalls. It also runs in the Android apps' page (ADR-0152), but that
build has not yet played on a TV or a phone. Safari, TV and phone performance
are not measured.

Deleting hls.js in the same step would leave no way back if a browser or
stream nobody tested plays worse.

## Decision

We will make the Rust engine, named "Pharos" in the Playback setting, the
default for MSE playback on the web. hls.js stays vendored for one release as
"hls.js (older)": a device can pick it in the setting, `?engine=hlsjs` picks
it for one page load, and the engine still hands a stream it cannot play to
it.

The default is named per platform (`platform::default_mse_engine`). The
Android apps keep hls.js as their default until the Rust engine has played on
a TV and a phone; switching them is a one-line change
(`tv_page::DEFAULT_MSE_ENGINE`).

hls.js, its engines and the hand-back path are deleted in a later change,
once the Android default has switched and nothing has needed the way back.

## Consequences

- A device with no stored choice changes engine on its next page load. A
  stored choice is kept, so a device that picked hls.js stays on it.
- `pharos: playback engine=<rust|hlsjs> reason=default` and the
  `playbackEngine` client event show which engine each device plays, so the
  share still on hls.js is a query, not a guess (V630).
- Two engines are maintained for one more release.
- The watch page no longer fetches `hls.min.js` unless hls.js is asked for or
  a stream is handed back to it.

## Alternatives considered

- **Delete hls.js now.** Rejected: no way back for an untested browser, and
  Android has not played the engine on a device.
- **Switch every platform at once.** Rejected for the same Android reason.
- **Keep hls.js the default and widen opt-in testing.** Rejected: nobody opts
  in, so it measures nothing.
