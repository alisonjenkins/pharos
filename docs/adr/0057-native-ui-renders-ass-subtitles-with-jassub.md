# ADR-0057: The native web UI draws ASS subtitles through a vendored, pinned JASSUB build

- **Status:** Accepted
- **Date:** 2026-11-04T00:30:00Z
- **Deciders:** Alison Jenkins

## Context

The native watch page shows a text subtitle as a WebVTT `<track>`. The
server converts an embedded ASS/SSA track to WebVTT for it, and the
conversion keeps only the words: the positions, fonts, colours, borders,
signs and karaoke that anime releases rely on are dropped. The server already
serves the original script at `/videos/{id}/subtitles/{index}/stream.ass`
(the route jellyfin-web uses).

A browser cannot draw ASS itself. libass compiled to WebAssembly can, in a
Web Worker over a canvas laid on the video. The UI's Content-Security-Policy
allows script and workers from `'self'` and `'wasm-unsafe-eval'` (ADR-0033),
so the renderer must be served from the UI's own origin.

## Decision

We will vendor JASSUB **2.5.23** at `crates/pharos-ui/assets/vendor/jassub/`,
built by `scripts/vendor-jassub.sh`. `pharosUiBundle` installs the directory
under `assets/vendor/jassub/`.

JASSUB 2.x is published as ES modules with bare imports, so the script
bundles them with the flake's esbuild (0.27.2): `jassub.js`, a classic script
that defines `window.JASSUB`, and `jassub-worker.js`, the module worker. The
two wasm builds and `default.woff2` are copied unmodified. The script checks
every tarball against the integrity npm published for it:

| Package | Version | Integrity |
|---|---|---|
| jassub | 2.5.23 | `sha512-KvWvkZOQvLUZKDoJTfAZzbmxCN2Avs37d1VnZlscKvmfPIjcS+l2guou0WdGW6z/+Uxb8F7ha8Nrq5h5+k9cLw==` |
| abslink | 1.3.1 | `sha512-ukFoU6q/RBMjoC7fu0Zx2+eVH2QFMfV+aJ2cI3Ypl0Shm0ikGjZ0T2VN0xvLl0a8imhWqvfVrqu2n2Ml3eSMyQ==` |
| lfa-ponyfill | 1.1.1 | `sha512-Had4tsGEHtIj58MRtf6SlubNDltxinxMSdxIn1GdwrWbye8TD+pZ+D6qKETnJ9+vQOfG9q/Ds4WUPskhyAMp5Q==` |
| rvfc-polyfill | 1.0.8 | `sha512-uA+0wwTkZ4OT8v45pfDfH+7Yq8mY6MvNngiF5Sq6VBgjJsvsfgt7Q18cyZqZjfAhW9rhkgXPX0cW0R9uw7yElA==` |
| throughput | 1.0.2 | `sha512-jvK1ZXuhsggjb3qYQjMiU/AVYYiTeqT5thWvYR2yuy2LGM84P5MSSyAinwHahGsdBYKR9m9HncVR/3f3nFKkxg==` |

sha256 of the output:

- `jassub.js` `abf438078bb39b07190bc432c591c5061fb19e8eeb0154ded82bb9e0dcc673cc`
- `jassub-worker.js` `017cb6cfd8f8ecca6e75cfa721ff7e7a64a1bd40a582faf0ab0f95f02c17b05b`
- `jassub-worker.wasm` `2b7e65f65ab7c84736dbcc5f2bbe3af9536db142f65ca8673b54bca21f38fbf8`
- `jassub-worker-modern.wasm` `e6b436efa71e03274cdb993d38553f7870f37e798d7c83ba7856651c66bb087d`
- `default.woff2` `886929903707c5bb28b07cd2eed69921b3d5ef5c49c73a0dd1e187ebf6546a4c`

`LICENSE` is JASSUB's, `LICENSE.<package>` each bundled dependency's (from
package.json where the tarball has no file), and `COPYRIGHT` the notices of
the C libraries compiled into the wasm (libass, FreeType, HarfBuzz, fribidi
and others), taken from the 1.8.8 tarball because 2.x ships none.
rvfc-polyfill is GPL-3.0, which pharos's AGPL-3.0 accepts.

- The watch page adds the `<script>` only when the picked subtitle's codec is
  `ass` or `ssa` (`native::playback::styled_subtitle_url`). The WebVTT track
  stays on screen until JASSUB's `ready` resolves, and comes back if it
  rejects or takes longer than 45 s, so a renderer failure costs the styling,
  never the words.
- JASSUB is told every URL (worker, wasm, font), turns off the Local Font
  Access query (it prompts the viewer), and sits behind
  `platform::StyledSubtitles` in `native/web.rs`. Replacing it is one type
  and one directory.

To update: change the pins in the script, run it in the dev shell, and record
the new versions and hashes in a new ADR that supersedes this one.

## Consequences

- Embedded ASS tracks play with their typesetting in every browser with Web
  Workers and WebAssembly.
- The bundle grows by about 4.6 MB (two 2.1 MB wasm builds, of which a
  browser fetches one), loaded only when an ASS track is shown. The files are not
  content-hashed, so they revalidate on each load.
- Fonts embedded in the file (MKV attachments) are not passed to JASSUB yet:
  the native API does not list them. Every style draws in the bundled
  Liberation Sans until it does, so a sign's layout and colours are right but
  its typeface is not.
- Sidecar `.ass` files are labelled `webvtt` by the server's stream list, so
  they still play as plain WebVTT.
- The files are reviewed by hash, not diff (`.gitattributes` marks the vendor
  directory `-diff`); anyone can rebuild them with the script and compare.
  Nothing watches upstream for security fixes.

## Alternatives considered

- **JASSUB 1.8.8.** Ships a self-contained UMD build that needs no bundling,
  but its emscripten glue builds functions with `new Function`, which the
  CSP's `script-src` (no `'unsafe-eval'`) refuses, so its worker never starts.
- **A CSP exception for the worker** (`'unsafe-eval'` on the vendor path
  only). Keeps 1.8.8, but relaxes ADR-0033's policy on a credentialed
  same-origin worker.
- **SubtitlesOctopus.** JASSUB's predecessor, unmaintained.
- **Burn ASS into the video on the server.** Already possible through the
  burn path, but it forces a transcode for every styled subtitle and every
  subtitle switch renegotiates.
- **Convert ASS to WebVTT with positions.** WebVTT has no fonts, borders,
  rotation or karaoke; anime typesetting does not survive it.

## References

- `scripts/vendor-jassub.sh`, `crates/pharos-ui/src/native/playback.rs`,
  `crates/pharos-ui/src/native/web.rs`
- ADR-0033 (UI CSP), ADR-0035 (the same vendoring pattern for hls.js)
- https://github.com/ThaUnknown/jassub
