# Implementation plan: Pharos on a Kobo (047)

**Spec**: [spec.md](spec.md). **ADR**: ADR-0136.

## Shape

| Place | Holds |
|---|---|
| `kobo/app/src/api.rs` | URLs, bodies and reply parsing for the pharos REST calls; no runtime calls, host-tested. |
| `kobo/app/src/main.rs` | The `KoboApp`: views (sign-in, albums, album, player, problem), the network task table, the player and the reports. |
| `kobo/app/src/tests.rs` | The app driven through `AppRunner`: sign-in, a rejected token, an expired code, play and reports, screens fit the Clara BW. |
| `kobo/app/cobalt-app.json` | Capabilities: `network`, `audio`, `bluetooth-audio`, `bluetooth-control`. |
| `kobo/cobalt.patch` | Cobalt: Pharos in `MANAGED_BUILTINS` and `INSTALLED_PACKAGES`, the workspace member and lock entry, `update::apply` refusing platform releases. |
| `kobo/default.nix` | Fetches Cobalt at the pinned commit, applies the patch, adds the app as `examples/pharos`, vendors the patched lock, builds `KoboRoot.tgz` with `kobo package`. |
| `kobo/build.sh` | The same steps with plain cargo, for a machine without the flake inputs. |

No pharos server change: the Jellyfin-compatible endpoints already cover Quick Connect, listings, the universal audio stream and playback reports.

## Signals

- Server, sign-in: `{namespace="pharos"} |= "quick connect"` with `app_name="Pharos Kobo"`.
- Server, playback: `{namespace="pharos"} |= "playback: session started" | json | client="Pharos Kobo"`.
- Reader: `kobo logs` lines `pharos: play track=…`, `pharos: report <started|progress|stopped> sent`, `pharos: <work> failed: <error>`, `pharos: signed in`.

## Gates

- Cobalt's toolchain (1.85.1): `cargo test -p kobo-pharos`, `cargo clippy -p kobo-pharos --all-targets -- -D warnings`, `cargo fmt --check`; `cargo test -p kobod -p kobo-cli` with the patch applied.
- The Started report test disarmed once and watched fail.
- Static ARM build verified by `kobo verify`; the package by `kobo inspect`.
