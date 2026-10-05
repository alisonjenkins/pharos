# jellyfin-vr

Vendored, unmodified copy of the VR player extension for jellyfin-web.

- Upstream: <https://github.com/yyewolf/jellyfinvr> (GPL-3.0; unofficial, unaffiliated with Jellyfin)
- Licence: GPL-3.0, text in `LICENSE` (compatible with pharos's AGPL-3.0-or-later).
  The text is the canonical one from gnu.org; upstream's LICENSE states
  "Version 3, 29 June 2007".
- File: `Jellyfin supports plugins for VR.js` at upstream `main`, fetched 2026-10-05
- Adds a "Watch in VR" entry to the video player's settings menu
  (VR180, VR360, SBS 3D, OU/top-bottom 3D, WebXR; built for Meta Quest Browser).
- Format is auto-detected from filename / title markers in `/Items/{id}`.

Upstream installs it through the Jellyfin "JavaScript Injector" server plugin,
which pharos does not have. `flake.nix` (`jellyfinWebBundle`) instead copies it
into the jellyfin-web bundle at `vr/jellyfin-vr.js` and injects a `<script>`
tag into `index.html`.

Build-time rewrite (asserted, fails the build if it misses): upstream loads
three.js from `cdn.jsdelivr.net`; the bundle step points `THREE_URL` at a
pinned copy of three@0.160.1 at `vr/three.min.js` (relative, like every other
bundle file, so it resolves under `/web/` and under a root-served bundle), so
the headset needs no third-party CDN.

Known limit: the script's "compatibility" fallback requests
`/Videos/{id}/stream.mp4?Static=false` expecting an H.264 transcode. Pharos
only transcodes progressive `.webm`; that URL serves the source file. VR
playback therefore needs a source the headset browser can decode.

To update: re-download the file, diff, and re-check the two build assertions.
