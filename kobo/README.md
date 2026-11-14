# Pharos for Kobo

A Pharos client for Kobo readers (tested target: Libra Colour), built as a
[Cobalt](https://github.com/BandarLabs/Cobalt) app. Decision: ADR-0136.
Spec: `specs/047-kobo-app/`.

What it does today: signs in with Quick Connect (a code to approve in the
Pharos web UI), lists music albums and their tracks, and plays a track to
Bluetooth headphones through Cobalt's player, telling Pharos where you are.

## Layout

| Path | What |
| --- | --- |
| `app/` | The app (crate `kobo-pharos`, AGPL-3.0-only). It builds only inside Cobalt's workspace, as `examples/pharos`; the Pharos workspace does not list it. |
| `cobalt.env` | The pinned Cobalt commit. |
| `cobalt.patch` | Adds the app to Cobalt's built-in apps and device package, and refuses official platform updates (they would remove the app). |
| `default.nix` | `nix build .#kobo` → `result/KoboRoot.tgz`. |
| `build.sh` | The same build without Nix, for working on the app. |

## Building

```sh
nix build .#kobo                     # the device package
kobo/build.sh                        # without Nix; see the script's header
```

The server is baked in at build time (`PHAROS_KOBO_SERVER`; the Nix package
takes a `server` argument).

## Running in the simulator

After `kobo/build.sh`, from `kobo/.cobalt/examples/pharos`:

```sh
PHAROS_KOBO_SERVER=https://your-pharos ../../target/debug/kobo dev
```

The simulator has no audio; sound needs the reader.

## Bumping Cobalt

1. Change `COBALT_REV` in `cobalt.env` and `rev` in `default.nix`.
2. Run `kobo/build.sh`; if the patch fails, fix it by hand in
   `kobo/.cobalt` and save it with
   `git -C kobo/.cobalt diff -- . ':!examples/pharos' > kobo/cobalt.patch`.
   The `Cargo.lock` hunk conflicts most often.
3. Put the new source hash in `default.nix` (Nix prints it).
