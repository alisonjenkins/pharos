# 027 — First-run setup and what's new after sign-in

- **Status:** slice 1 implemented on main (audited 2026-11-09: 734e2836, 2ccd18fc, 6d4865e9, 11b6fc28; PR #843 batch AF/AG, ADR-0065); later slices (more setup pages, changelog from release notes) not started. Owned elsewhere (What's-New wizard).
- **Date:** 2026-11-05
- **Asked by:** Alison: a wizard on login when settings were added since the
  viewer last logged in, demoing each option live so they can pick, plus "a
  mini change log too advertising new features and bug fixes", and it
  "should not be intrusive if a new UI update is deployed and they happen to
  be watching things at the time". Then: "I don't want them to wait to be
  browsing, but instead have like an icon showing there is something new
  they can click to go check it. Unless they have just loaded the UI in
  which case yes splash like experience", and "If they are a new user too
  they should get the splash to walk them through initial setup".
- **Ids:** ADR-0065, V495, V496, V497, B581.

## Goal

A new viewer sets Pharos up in a minute with each choice shown live, and an
existing viewer learns about new settings and changes without hunting for
them. Neither has playback interrupted to do so.

## Behaviour

- **Registry.** `native/whats_new.rs` `ENTRIES`: append-only entries, each
  with an increasing `id`, a kind (a setting with a demo, a feature, or a
  fix), a title and one line of text. The newest id is `current()`.
- **Seen, per user, on the server.** The native `Prefs` key
  `whats_new.seen` holds the newest id the viewer has been shown (a decimal
  number; anything else is refused). Absent means a new account: migration
  0067 gave every account that existed when this shipped `0`.
- **Setup (no value).** A welcome page, then the `SETUP` settings (theme,
  subtitle size) and every setting in `ENTRIES`, each once, then Start
  watching. No changelog.
- **What's new (a value).** Everything after the seen id: one page per new
  setting, oldest first, then a changelog page with up to eight feature and
  fix lines, newest first. Accounts at `0` get every setting entry and the
  newest eight lines.
- **When.** Read once per sign-in (a page load signs in too). Right after
  that, the dialog opens by itself as a splash, unless something is on the
  stage (a video or song, in any view) or the watch page is open (V495).
  Otherwise, and after Remind me later, a sparkle button in the top bar
  says how many things are new and opens the dialog when clicked. It never
  opens by itself later in the page's life.
- **Settings.** Each setting page shows a live demo and the setting's own
  picker; a pick applies and saves at once through the same path as the
  settings page. The theme restyles the page itself as it is picked; the
  subtitle demo draws a cue over a stand-in picture with the player's scale
  steps (V497); the library card transition demo carries its own
  `data-library-transition` (V496) and replays per pick; the controls
  effect demo is a stand-in player whose chrome hides and comes back on a
  loop under the chosen effect (V500).
- **Closing.** Done (Start watching in setup) records `current()` as seen. Remind me later and Escape
  record nothing: the badge stays, and the dialog splashes again on the
  next load or sign-in. A failed save is logged and the dialog stays closed for the page.
- **Off on a device.** localStorage `pharos.whats_new` = `off` turns it off
  on that device (kiosks, and the browser suite, which signs in fresh
  accounts constantly).
- **Signals.** Console lines `pharos: whats_new splash=<verdict> flow=setup|whats_new seen= current=`
  once per sign-in, `pharos: whats_new opened from=badge`, and `pharos: whats_new closed how=later|done`; the
  server WARNs `pref refused: not an entry number` on a bad write.

## Adding an entry

Append to `ENTRIES` with the next id. A setting also adds a `Setting`
variant (title and text), its demo in `SettingPage`, and its `Pick`; it then
shows in setup too. The test
`entry_ids_are_unique_and_increasing` guards the order.

## Later slices

- More setup pages: description language and maximum quality (both already
  settings; left out of slice 1 to keep setup short).
- Changelog lines from release notes rather than by hand, if release-please
  notes become user-facing.
