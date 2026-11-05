# ADR-0065: Setup and what's new run from a hand-written registry and a server-side seen id

- **Status:** Accepted
- **Date:** 2026-11-05T22:30:00Z
- **Deciders:** Alison (asked for the feature); the mechanism was chosen while building it

## Context

Alison asked for a dialog after sign-in that walks viewers through settings
added since they last looked, with live demos, and a short list of new
features and fixes, and a first-run walk through setup for new accounts.
It must not interrupt someone watching when an update deploys. Settings are mostly device-local (localStorage), but a viewer uses
several devices, and the native API already has a per-user key/value
`Prefs` store with an allow-list.

`CHANGELOG.md` is release-please output: commit subjects, mostly server
internals, last cut at 0.3.0, so it is not viewer-facing text.

## Decision

We will keep a hand-written, append-only registry, `ENTRIES` in
`crates/pharos-ui/src/native/whats_new.rs`. Each entry has an increasing
numeric id, a kind (setting with a demo, feature, fix), a title and a line
of text. The newest id a user has been shown is stored server-side under the
`Prefs` key `whats_new.seen` (digits only, at most nine), so one Done covers
every device. An account with no value is new and gets setup (a welcome
and the core settings with demos); migration 0067 gives every account that
existed when this shipped the value `0`, so they get what's new. The dialog splashes only right after a page load or
sign-in, and only when nothing is on the stage and the watch page is closed
(V495); otherwise a top-bar badge opens it on a click. A device can turn it off with localStorage
`pharos.whats_new` = `off`.

## Consequences

- A change that adds a setting or something worth telling viewers appends
  an entry in the same PR; nothing reminds an author but review.
- One number per user and no schema change: the existing `user_prefs` table
  holds it. One data migration (0067) seeds existing accounts.
- An admin who restores an old database dump after this shipped would see
  every restored account treated as new; acceptable, setup only asks for
  device-local choices.
- The seen id is per user, not per device: a viewer who presses Done on the
  TV does not see the same entries on the web.
- Entries ship with the UI bundle, so a page loaded before a deploy shows
  the new entries only after a reload, which is when the new UI arrives.
- The browser suite turns the dialog off through the device key, and its
  own cases turn it back on.

## Alternatives considered

- **Generate from `CHANGELOG.md` / commits:** wrong audience and wording,
  and no way to attach a demo.
- **Account creation date for "new":** the native `Me` has none, so it
  would need a schema change; the migration answers the same question once.
- **Seen id in localStorage:** every device and every cleared browser would
  show the dialog again.
- **Version string (semver) instead of an id:** the UI ships continuously
  from main without release versions; an id is simpler to compare.
- **Wait for playback to end, then open:** opens mid-session over whatever
  the viewer turned to next; Alison asked for a badge instead and a splash
  only on a fresh load.

## References

- `specs/027-ui-whats-new/spec.md`
- `crates/pharos-server/src/api/pharos/rpc/prefs.rs` (`WHATS_NEW_SEEN`)
- ADR-0062 (the `description.language` pref, same store)
