# ADR-0153: Phone and TV apps ship as GitHub Releases for anyone with an account

- **Status:** Accepted
- **Date:** 2026-11-24T08:40:00Z
- **Deciders:** Alison
- **Supersedes:** ADR-0141

## Context

ADR-0141 chose GitHub Releases plus Obtainium for the phone app
(`com.alisonjenkins.pharos.phone`) and the Android TV app
(`com.alisonjenkins.pharos.tv`). It described the people installing them
as the owner's friends. Pharos is open source, and the releases are public,
so that framing is wrong. Anyone may download the APKs. Signing in still
needs an account on the Pharos server the apps were built for.

The mechanics in ADR-0141 are unchanged and proven: the first release,
`apps-v2026.11.23.0714`, published from the `android apps` workflow.

Android installs an update only when it is signed with the same key and has
a higher `versionCode` than the installed copy.

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
  local. That is the scripts' default; `PHAROS_ANDROID_VERSION_CODE`
  overrides it. The workflow sets it for both APKs from its shared
  `version` job, so the two apps and the release carry the same minute.
  Do not set it by hand for a local build, because a code below the
  installed release's makes Android refuse the update as a downgrade.
  `versionName` and the tag are the same minute as a date.
- App releases are not marked Latest, so the server release stays Latest.
- The audience is anyone who installs the apps. They install with Obtainium
  (phone) or Downloader (TV), as `docs/android-apps.md` explains, and sign in
  with an account the server owner gives them. There is no sign-up page.

## Consequences

- People who install the apps get updates without the owner sending files.
- The tag and title format is a contract: Obtainium filters match
  `^Pharos apps` and the APK file names `pharos-phone` / `pharos-tv`.
  Renaming either breaks updates for everyone who installed them.
- Losing or replacing the keystore forces everyone who installed the apps to
  uninstall and lose their sign-in. It must be backed up.
- The keystore also lives in GitHub secrets and is written to the
  self-hosted runner's temp directory during a run.
- A local build with a small `PHAROS_ANDROID_VERSION_CODE` would be refused
  as a downgrade on a device with a release build.
- The published APKs open on the owner's server. This decision does not
  publish builds for other Pharos servers; someone running their own server
  builds the apps from source.

## Alternatives considered

- **Keep ADR-0141 as written.** It records the right mechanism but names the
  wrong audience, so it misleads readers of a public repo.
- **Build locally and upload by hand.** The keystore stays on one machine,
  but every release needs that machine and a person.
- **A download page served by Pharos.** Only signed-in users would see it,
  but Obtainium could not track it, so updates would stay manual.
- **Google Play.** Review and a developer account for each app.

## References

- ADR-0141 (superseded)
- `.github/workflows/android-apps.yml`, `.github/workflows/release-please.yml`
- `docs/android-apps.md`
