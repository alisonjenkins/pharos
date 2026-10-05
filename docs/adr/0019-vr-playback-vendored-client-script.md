# ADR-0019: VR playback via a vendored client script in the jellyfin-web bundle

- **Status:** Accepted
- **Date:** 2026-10-05T00:00:00Z
- **Deciders:** Alison

## Context

Stock jellyfin-web has no VR player, so VR180, VR360 and stereoscopic files
could not be watched through pharos in a headset browser. The only consumer is
the Meta Quest Browser, which supports WebXR.

The player has to run in the client. Server-side there is nothing to render, and
pharos serves the jellyfin-web bundle unmodified apart from build-time patches
(`jellyfinWebBundle` in `flake.nix`, which already carries the B175 epub.js
patch, ADR-0009).

An existing community extension does this, [jellyfinvr](https://github.com/yyewolf/jellyfinvr)
by yyewolf (GPL-3.0). Upstream installs it through the Jellyfin "JavaScript
Injector" server plugin. Pharos has no plugin host and does not want one
(ADR-0024).

## Decision

**We vendor the upstream script unmodified and wire it into the jellyfin-web
bundle at build time.** The player stays upstream's work, credited to its
author in `docs/jellyfin-vr.md` and the vendored directory's README.

- The script lives in `web-patches/jellyfin-vr/` with upstream's licence text.
  `jellyfinWebBundle` copies it to `vr/jellyfin-vr.js` and injects
  `<script src="vr/jellyfin-vr.js" defer>` into `index.html`.
- The one rewrite: upstream loads three.js from a CDN. The bundle serves a
  pinned three@0.160.1 at `vr/three.min.js` and points `THREE_URL` at it, so a
  headset needs no third-party host. Paths are relative like every other
  bundle file, so they resolve under `/web/` and under the root-served bundle
  the compat harness uses.
- Every rewrite asserts exactly one match, and the vendored files are asserted
  non-empty. A jellyfin-web bump that changes `index.html` fails the build
  rather than silently dropping the button. Each assertion was disarmed once
  and confirmed to fail with its own message.

## Consequences

- Quest users get **Watch in VR** with no server plugin and no pharos code to
  maintain beyond the wiring. Updating means re-downloading the upstream file
  and re-checking the assertions.
- The script runs same-origin with the signed-in user's access token. It was
  grepped for dynamic code execution, beacons, cookies and `postMessage` (none)
  and its only other host is the three.js CDN, now removed. It was not read in
  full. This is third-party code with full client access, accepted for a
  personal deployment.
- **No transcode fallback.** The script's compatibility mode requests
  `/Videos/{id}/stream.mp4?Static=false` expecting an H.264 transcode. Pharos
  transcodes only progressive `.webm`, so that URL serves the source file.
  Playback needs a source the headset can decode. No server-side signal for
  this mismatch has been identified.
- Format detection is the filename and frame size only. Pharos does not populate
  Jellyfin's `Video3DFormat`, so nothing server-side marks a file as VR.
- WebXR playback has not been run on a headset by pharos's checks. The build
  verifies the files are in the bundle and the CI crawl loads the page.
- Every jellyfin-web page now loads the extra script (about 113 KB before
  compression). It is `defer` and compressed by the angie front.
- GPL-3.0 is compatible with the repo's AGPL-3.0-or-later. Upstream's licence
  text ships with the vendored copy.

## Alternatives considered

- **Write a VR player.** A WebXR player with projection, stereo packing and
  controls is far more than the problem needs when one exists.
- **Install the JavaScript Injector plugin.** Needs a plugin host. Rejected by
  ADR-0024's stance.
- **Fork jellyfin-web.** Rebasing a fork on every release is the cost the
  build-time patch approach avoids.
- **Populate `Video3DFormat` server-side.** jellyfin/jellyfin#18085 proposes 180,
  360 and fisheye values. Worth revisiting once the clients read them; today the
  vendored script does not.

## References

- `docs/jellyfin-vr.md`, `web-patches/jellyfin-vr/README.md`
- `flake.nix` (`jellyfinWebBundle`); PR #338
- ADR-0001 (Jellyfin client contract), ADR-0009 (Nix flake), ADR-0024 (no plugin host)
- Upstream: <https://github.com/yyewolf/jellyfinvr>
