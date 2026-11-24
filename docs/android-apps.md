# Pharos phone and TV apps

The phone app and the Android TV app are published as GitHub Releases on
this repo, tagged `apps-v<date>`. Each release holds two files:

| File | For |
|---|---|
| `pharos-phone-<version>.apk` | Android phones (64-bit, Android 8 or newer) |
| `pharos-tv-<version>.apk` | Android TV and Google TV boxes, 32-bit and 64-bit |

Both apps open on the Pharos server they were built for. Nobody types a
server address. Each person signs in with their own Pharos account.

- [Installing](#installing)
- [Publishing (server owner)](#publishing-server-owner)

## Installing

You need a Pharos account first. Ask the server owner for a user name and
password. There is no sign-up page.

### Phone: Obtainium

Obtainium installs apps from GitHub and tells you when an update is out.

1. Install Obtainium from
   [its GitHub releases](https://github.com/ImranR98/Obtainium/releases)
   (the `app-arm64-v8a-release.apk` file) or from F-Droid.
2. Open Obtainium and tap **Add app**.
3. In **App source URL**, enter `https://github.com/alisonjenkins/pharos`.
4. Open the additional options and set:
   - **Filter release titles by regular expression**: `^Pharos apps`
   - **Filter APKs by regular expression**: `pharos-phone`
5. Tap **Add**, then **Install**.
6. Android asks to allow installs from Obtainium. Allow it, then go back
   and install again.
7. Open Pharos and sign in.

Updates appear in Obtainium's list. Tap **Update** to install one. Your
sign-in and downloads stay.

If Obtainium keeps offering the same version again, open the app's
settings in Obtainium and turn off **Version detection**.

### TV: Downloader

Obtainium works on a TV too, but it is awkward with a remote. The free
**Downloader** app from the Play Store is easier.

1. Install **Downloader** from the Play Store on the TV.
2. Allow it to install apps: Settings → Apps → Security & restrictions →
   Unknown sources → Downloader. The menu names differ a little between TVs.
3. Open Downloader and enter
   `https://github.com/alisonjenkins/pharos/releases`.
4. Open the newest **Pharos apps** release and pick the `pharos-tv-…apk`
   file. Downloader asks to install it.
5. Open Pharos from the apps row and sign in.

To update, repeat steps 3 and 4 with the newest release. It installs over
the old version and keeps your sign-in.

To use Obtainium on the TV instead, follow the phone steps with the APK
filter `pharos-tv`. Many TVs are 32-bit only: on those, install Obtainium's
`app-armeabi-v7a-release.apk` instead of the arm64 one. If the arm64 file
says "App not installed", use the 32-bit one.

### If an update will not install

Android installs an update only when it is signed with the same key as the
installed app. If you got an APK from somewhere other than these releases,
uninstall it once, then install from a release.

Uninstalling deletes the app's data. You will need to sign in again, and
anything you downloaded for offline use has to be downloaded again.

## Publishing (server owner)

The `android apps` workflow (`.github/workflows/android-apps.yml`) builds
both APKs on the self-hosted Nix runner, signs them, and publishes the
release. It runs:

- after each server release made by release-please, and
- by hand: Actions → **android apps** → **Run workflow**.

### One-time setup

The workflow signs with the same keystore you use for local builds
(`scripts/android-apk.sh`). Use that exact keystore. A new key breaks
updates on every device that has the app.

Set three repository secrets and one variable. From a checkout, with the
keystore and its password file at hand:

```sh
base64 -w0 path/to/pharos.keystore | gh secret set PHAROS_ANDROID_KEYSTORE_B64
gh secret set PHAROS_ANDROID_KEYSTORE_PASSWORD < path/to/password-file
gh secret set PHAROS_ANDROID_KEY_ALIAS --body 'your-key-alias'
```

The password file holds one password used for both the store and the key,
as the scripts expect.

Then record the key's certificate fingerprint. The workflow refuses to
publish an APK signed with any other key:

```sh
keytool -list -v -keystore path/to/pharos.keystore -alias your-key-alias \
  | sed -n 's/^.*SHA256: //p'
gh variable set PHAROS_ANDROID_CERT_SHA256 --body '<the fingerprint>'
```

Colons and letter case in the fingerprint do not matter.

### Versions

Each run takes one moment in UTC, the time the run started. A re-run keeps
it, so it updates the same release:

- `versionCode` is the minutes since 2026-01-01 UTC, the scripts' default,
  so a newer build installs as an upgrade over an older one.
- `versionName` and the tag are the same moment as `YYYY.MM.DD.HHMM`.

The code is the minute the run started, not the minute it finished. A local
build made while a CI run is still going can get a higher code. A device that
installs it may then refuse that CI release as a downgrade, and takes the next
one instead.

Local sideloads use the same default `versionCode`. Do not set
`PHAROS_ANDROID_VERSION_CODE` to a small number, or a device that has a
release build will refuse the sideload as a downgrade.
