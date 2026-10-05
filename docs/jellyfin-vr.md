# VR video in jellyfin-web

The jellyfin-web bundle that pharos serves includes a VR player. It adds a
**Watch in VR** entry to the video player's settings (gear) menu and plays the
current video as VR180, VR360 or stereoscopic 3D through WebXR. It was built
for the Meta Quest Browser.

Decision record: [ADR-0019](adr/0019-vr-playback-vendored-client-script.md).

## Credit

The VR player is **not pharos code**. It is the
[**jellyfinvr**](https://github.com/yyewolf/jellyfinvr) extension by
[**yyewolf**](https://github.com/yyewolf), used under the GPL-3.0. All of the
player, the format detection and the WebXR work are theirs. It is vendored
unmodified in [`web-patches/jellyfin-vr/`](../web-patches/jellyfin-vr/) with
upstream's licence text, and pharos only wires it into the bundle. Report
player bugs and feature requests upstream.

It is an unofficial extension, unaffiliated with the Jellyfin project.

## Using it

1. Open pharos's jellyfin-web (`/web/`) in the headset's browser and sign in.
2. Start a video, open the gear menu in the player, choose **Watch in VR**.
3. Press **Enter VR** in the overlay.

The overlay says how the format was chosen (for example "detected 180 / SBS
from the file name"). The control panel inside the headset has mode controls
such as **SWAP EYES**, **RECENTER** and **EXIT VR**. A mode you set is
remembered per item in the browser's local storage.

## How the format is chosen

The player decides from the item's name and file path first, then from the
frame size. Matching is case-insensitive and splits the text on anything that
is not a letter or digit, so `My_Film-VR180_SBS.mp4` yields the tokens
`my`, `film`, `vr180`, `sbs`, `mp4`.

| Marker (as a whole token) | Meaning |
|---|---|
| `180`, `vr180`, `180vr`, `180x180` | 180° projection |
| `360`, `vr360`, `360vr`, `360x180` | 360° projection |
| `fisheye`, or a lens profile such as `mkx200`, `mkx220`, `rf52`, `vrca220` | fisheye projection |
| `flat`, `2d` | flat (not VR) |
| `sbs`, `hsbs`, `fsbs`, `3dsbs`, `lr` | side-by-side stereo |
| `ou`, `hou`, `tb`, `htb`, `tab` | over-under (top-bottom) stereo |
| `rl` | side-by-side with the eyes swapped |
| `mono`, `2d` | one picture for both eyes |
| any other token starting or ending with `3d` | stereo, assumed side-by-side |

The stereo markers also match with a `half`, `full` or `3d` prefix or a `3d`
suffix, so `halfsbs`, `fullou` and `sbs3d` work. `2d` appears in two rows
because the script reads it as both a flat projection and mono stereo.

With no usable marker it falls back to the aspect ratio. A very wide frame of
3000 px or more is taken as 360 or 180 side-by-side, a roughly square frame of
2000 px or more as mono 180, and a tall frame as over-under.

Naming files with a marker (`Title.VR180.SBS.mkv`) is the reliable route, since
the aspect ratio cannot tell half-width packings from an ordinary video.

## Limits

- **No transcode fallback.** The player's "compatibility" mode requests an
  H.264 transcode from `/Videos/{id}/stream.mp4?Static=false`. Pharos only
  transcodes progressive `.webm`, so that URL serves the source file. A video
  plays in VR only if the headset's browser can decode the original. Very high
  resolution HEVC or AV1 may not.
- **Not tested on a headset** by pharos's own checks. The build verifies that
  the script and its three.js dependency are in the bundle; the CI jellyfin-web
  crawl loads the page. WebXR playback itself is unverified here.
- **Pharos does not tell clients a file is VR.** Jellyfin's `Video3DFormat`
  field is not populated, so nothing server-side marks a file as VR. Detection
  is entirely the filename and the frame size.
- **Updating it** means re-downloading the upstream file and re-checking the
  two build assertions, described in
  [`web-patches/jellyfin-vr/README.md`](../web-patches/jellyfin-vr/README.md).

## Where it lives

| Piece | Location |
|---|---|
| Vendored script and licence | `web-patches/jellyfin-vr/` |
| Wiring (copy, `<script>` tag, three.js) | `jellyfinWebBundle` in `flake.nix` |
| three.js | pinned 0.160.1, served at `vr/three.min.js`, no CDN |
