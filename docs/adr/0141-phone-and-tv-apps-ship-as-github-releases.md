# ADR-0141: Phone and TV apps ship as GitHub Releases for Obtainium

- **Status:** Superseded by ADR-0153
- **Date:** 2026-11-16T17:30:16Z
- **Deciders:** Alison

## Context

The phone app (`com.alisonjenkins.pharos.phone`) and the Android TV app
(`com.alisonjenkins.pharos.tv`) were built and signed only on one desktop,
by `scripts/android-phone-apk.sh` and `scripts/android-apk.sh`. People
outside the household need to install them and get updates without asking
for each new file.

Android installs an update only when it is signed with the same key and has
a higher `versionCode` than the installed copy. The TV builds had used small
hand-picked codes, while the scripts default to minutes since 2026-01-01 UTC.

## Decision

We will publish both APKs as assets of a GitHub Release on this repo,
tagged `apps-v<YYYY.MM.DD.HHMM>` and titled `Pharos apps <version>`, from the
`android apps` workflow. It runs by hand and after each release-please
release, on the self-hosted Nix runner.

- Both apps are signed with the one existing Pharos keystore, held in the
  repository secrets `PHAROS_ANDROID_KEYSTORE_B64`,
  `PHAROS_ANDROID_KEYSTORE_PASSWORD` and `PHAROS_ANDROID_KEY_ALIAS`. The
  workflow refuses to publish when the certificate differs from the
  repository variable `PHAROS_ANDROID_CERT_SHA256`.
- `versionCode` is minutes since 2026-01-01 UTC for every build, CI or
  local. `versionName` and the tag are the same minute as a date.
- App releases are not marked Latest, so the server release stays Latest.
- Friends install with Obtainium (phone) or Downloader (TV), as
  `docs/android-apps.md` explains.

## Consequences

- Friends get updates without the owner sending files.
- The tag and title format is now a contract: friends' Obtainium filters
  match `^Pharos apps` and the APK file names `pharos-phone` / `pharos-tv`.
  Renaming either breaks their updates.
- Losing or replacing the keystore forces every friend to uninstall and lose
  their sign-in. It must be backed up.
- The keystore now also lives in GitHub secrets and is written to the
  self-hosted runner's temp directory during a run.
- A local build with a small `PHAROS_ANDROID_VERSION_CODE` would be refused
  as a downgrade on a device with a release build.

## Alternatives considered

- **Build locally and upload by hand.** The keystore stays on one machine,
  but every release needs that machine and a person.
- **A download page served by Pharos.** Only signed-in users would see it,
  but Obtainium could not track it, so updates would stay manual.
- **Google Play.** Review and a developer account for a private app.

## References

- `.github/workflows/android-apps.yml`, `.github/workflows/release-please.yml`
- `docs/android-apps.md`
