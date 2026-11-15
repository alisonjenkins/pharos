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
kobo/build.sh --setup                # build, then install over USB
```

The server is baked in at build time (`PHAROS_KOBO_SERVER`; the Nix package
takes a `server` argument).

## Putting it on the Kobo

This installs Cobalt with Pharos built in, from this repository. Use these
steps rather than Cobalt's own installer, whose releases do not carry Pharos.

### Before you start

- **Kobo firmware 4.x.** Cobalt does not support firmware 5.x. Cobalt lists the
  Libra Colour as tested.
- **A charged Kobo.** The Kobo's own installer does nothing on a low battery,
  and does not say so.
- **A USB cable that carries data.** If the Kobo only charges and never offers
  to connect, try another cable.
- **Bluetooth headphones or a speaker.** The Libra Colour has no speaker or
  headphone socket.
- **What setup changes.** Cobalt and its apps go in the Kobo's book storage
  (`.adds/`). Setup also leaves an update archive that the Kobo's own
  firmware installs as root at the next start, and that writes outside book
  storage:
  - NickelMenu, the menu add-on that starts Cobalt
    (`/usr/local/Kobo/imageformats/libnm.so`).
  - The Kobo's SSH server turned on, with this computer's key as the only
    way in, so later installs can go over Wi-Fi. `--setup --no-ssh` leaves
    SSH off.

  It never touches the kernel, the bootloader or the partitions.

### 1. Build and install over USB

On a Linux machine with Nix, from the repository root:

```sh
nix shell nixpkgs#zig nixpkgs#rustup nixpkgs#gcc --command kobo/build.sh --setup
```

`build.sh` builds Cobalt with the patch and the app, runs the app's tests,
then runs `kobo setup --source`, which installs that build and no official
release. The first build downloads Cobalt's Rust toolchain.

Setup waits for the reader:

1. Plug the Kobo in.
2. **Tap Connect on the Kobo's screen.** Until you do, it only charges.
3. Wait for the **KOBOeReader** drive to appear.

Setup shows the model, the firmware and what it will change, then asks before
writing. To see the changes without writing anything, run
`kobo/build.sh --setup --dry-run` instead. Anything after `--setup` goes to
`kobo setup`.

### 2. Let the Kobo restart

Follow what setup prints. Usually that is:

1. Once setup has ejected the Kobo, unplug the USB cable.
2. The Kobo restarts by itself: it shows its Updating screen, installs the
   archive and reboots. Do not restart it again yourself.
3. Join it to Wi-Fi if it is not already.

Restart it by hand (hold the power button until it turns off, then turn it
on) only if setup asks you to.

**Then leave it on the home screen for a minute.** The menu add-on turns
itself off if the reader restarts again straight away.

### 3. Open Pharos

1. Open the menu at the **bottom right** of the Kobo home screen and choose
   **Cobalt**.
2. Tap **Pharos** in the launcher.
3. The app shows a six-digit code. In the Pharos web UI, open **Quick
   Connect** and enter it. The Kobo then lists your albums.

The Kobo stays signed in. It appears on the Pharos devices page as
"Pharos Kobo", and removing it there signs the Kobo out.

To leave Cobalt, choose **Kobo reader** in the launcher, or restart the Kobo.

### 4. Connect headphones

1. Turn Bluetooth on once from the Kobo home screen after each restart.
   Cobalt can only use the radio once the Kobo's own software has started it.
2. In Cobalt, open **Settings → Bluetooth**, scan, and connect to the
   headphones by name.
3. Play a track in Pharos.

### Updating

After the first install, send a newer build over Wi-Fi with no restart:

```sh
nix build .#kobo
kobo/.cobalt/target/debug/kobo deploy --device KOBO-IP --package result/KoboRoot.tgz
```

`KOBO-IP` is the Kobo's address on your Wi-Fi. Or run the USB install again. Cobalt's own update check refuses official
releases on this build, because they would remove Pharos.

### Removing it

With the Kobo plugged in and connected:

```sh
nix shell nixpkgs#zig nixpkgs#rustup nixpkgs#gcc --command kobo/build.sh --setup --undo
```

This removes Cobalt and its menu entry, and turns SSH off again. It moves the
apps' data to `.adds/cobalt.recovery.N` on the Kobo first.

### If something goes wrong

- **No Cobalt entry after the restart.** The battery was probably too low.
  Charge the Kobo and restart it again.
- **No Cobalt entry after a Kobo firmware update.** Firmware updates remove
  the menu add-on. Run `kobo/build.sh --setup --menu` in the same
  `nix shell` as the install.
- **The screen looks wrong or stays blank.** Hold the power button to
  restart. You are back in the Kobo's own reader with nothing to undo.

Cobalt's full guide: [docs/INSTALL.md](https://github.com/BandarLabs/Cobalt/blob/5ac30a46efce23d7fa0d1cb12a499c9b57b816b0/docs/INSTALL.md).

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
