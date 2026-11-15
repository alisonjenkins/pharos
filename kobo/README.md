# Pharos for Kobo

A Pharos client for Kobo readers (tested target: Libra Colour), built as a
[Cobalt](https://github.com/BandarLabs/Cobalt) app. Decision: ADR-0136.
Spec: `specs/047-kobo-app/`.

What it does today: signs in with Quick Connect (a code to approve in the
Pharos web UI), shows a home screen with a section for each kind of library
you can see (Music, Audiobooks, Books), and shows albums and audiobooks as a
grid of covers. An album opens on its cover, artist, year and length above
its tracks. Playing a track sends it to Bluetooth headphones through Cobalt's
player, which shows the same cover, and tells Pharos where you are. Books is
a placeholder: reading ebooks on the Kobo is not built yet.

Covers are fetched for the page on screen and the next one, two at a time,
and only that many are kept on the reader. Colour covers go to a colour
panel (such as the Libra Colour) and grey ones to the rest. A cover that
cannot be fetched leaves the tile's icon in its place.

It also reads books. Choose **Books** at the top of a music or audiobooks shelf for your
Pharos books by name, with authors; a book that is on the reader says
**Downloaded**. Open one to download it (progress is shown) and read it;
the reader remembers where you stopped. A book's `⋮` menu removes its
download. Books come from Pharos's `/Items/{id}/Download`, with the same
sign-in as the music; the OPDS catalogue is not used because it needs an
e-reader password the app does not hold.

Readable: EPUB (also Kindle books, which Pharos serves as EPUB), plain text,
Markdown and HTML, up to 16 MB and 64 downloaded books. PDF and comic books
(CBZ, CBR) show "This reader can't open PDF books yet" and download nothing.
Reading progress is kept on the reader only; it is not reported to Pharos.

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
nix shell nixpkgs#zig nixpkgs#rustup nixpkgs#gcc nixpkgs#lld --command kobo/build.sh --setup
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

**If setup says `cobalt-launch.sh is not executable`:** the desktop mounted
the Kobo's FAT drive with `showexec`. That option marks only `.exe`, `.bat`
and `.com` files as runnable, so Cobalt's check of its launch script fails.
This comes from the computer's mount, not the Kobo. Mount the drive yourself
without `showexec`, then point setup at it:

```sh
udisksctl unmount -b /dev/sdX   # the KOBOeReader device, see lsblk
sudo mkdir -p /mnt/kobo
sudo mount -t vfat -o uid=$(id -u),gid=$(id -g),fmask=0022,dmask=0022,utf8,shortname=mixed,flush /dev/sdX /mnt/kobo
nix shell nixpkgs#zig nixpkgs#rustup nixpkgs#gcc nixpkgs#lld --command kobo/build.sh --setup --volume /mnt/kobo
sudo umount /mnt/kobo           # setup cannot eject a mount made with sudo
```

Unmounting by hand also skips setup's wait for Wi-Fi and SSH pairing. Once
the Kobo is back on Wi-Fi, run `kobo/.cobalt/target/debug/kobo pair`.

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
   Connect** and enter it. The Kobo then shows your libraries.

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
nix shell nixpkgs#zig nixpkgs#rustup nixpkgs#gcc nixpkgs#lld --command kobo/build.sh --setup --undo
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
- **`kobo pair` says the reader "did not answer as a Kobo".** Two causes have
  been seen on firmware 4.45:
  1. The computer does not know the Kobo's SSH host key yet, and pairing
     refuses unknown hosts. Log in once by hand
     (`ssh -i ~/.ssh/kobo_cobalt root@KOBO-IP`) and accept the key.
  2. The firmware's `/etc/ssh/initial_ssh_setup.sh` ignores the command SSH
     sends and always opens a shell, so every Cobalt command over Wi-Fi
     hangs. Back the script up, then add this line just before its
     `exec /bin/sh`:
     `[ -n "$SSH_ORIGINAL_COMMAND" ] && exec /bin/sh -c "$SSH_ORIGINAL_COMMAND"`.
     A firmware update puts the original back.
- **Root can still log in over SSH with a password.** Setup turns that off
  once the Kobo is back on Wi-Fi after its restart. If the Kobo was offline
  then, or you unmounted by hand, the step was skipped, and Cobalt has no
  command to rerun it on its own. Check with
  `ssh -o PubkeyAuthentication=no root@KOBO-IP`, which should be refused.

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
