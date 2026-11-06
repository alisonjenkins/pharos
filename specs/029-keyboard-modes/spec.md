# Spec 029: keyboard modes

Status: Accepted (2026-11-06)

## Goal

Every action in the web UI can be reached without the mouse. Keys are
grouped by **mode**, the context the focus is in. Within a mode no two
actions share a key. An action found in more than one mode has the same
key in each, so a key learnt once works everywhere.

Two maps are bound side by side in every mode: the **standard** keys
(arrows, Home/End, Enter, Space, Escape) and **Vim** keys (h/j/k/l, gg/G).
Neither needs turning on.

## Rules

- A mode's keys act only when the focus is not in a text field (`input`,
  `textarea`, `select`, `contenteditable`). There, every key types.
- A key held with Ctrl, Alt or Cmd is never a mode key; those chords stay
  the browser's and the palette's (Ctrl+K). Shift is part of a key: `G`
  is Shift+g.
- A key a mode does not bind does nothing new; the browser keeps it
  (Tab, Shift+Tab, Page Up/Down, Space scrolling a page).
- Enter always activates the focused control. Movement keys never
  activate anything, so moving is always safe.
- Focus is always visible (`:focus-visible` ring) on every control a key
  can reach.
- A page that opens puts the focus on its main action (Play, Resume, or
  on a show page the next episode's Play), without scrolling the page,
  unless the focus is already on a control in the page body. So a page
  reached from the search box, the palette or a tile is one Enter from
  playing.

## Shared keys

| Action | Standard | Vim | Modes |
|---|---|---|---|
| Move focus up / down / left / right | Arrows | k / j / h / l | Browse, Detail, Queue, any list |
| First / last item | Home / End | gg / G | Browse, Detail, Queue, any list |
| Activate the focused control | Enter | Enter, `o` | all |
| Back, close a dialog | Escape | Escape | all |
| Open the palette | Ctrl+K / Cmd+K | Ctrl+K / Cmd+K | all |
| Show this mode's keys | `?` | `?` | all |
| Go to (prefix, see below) | | `g` … | all |
| Previous / next track or episode | Shift+P / Shift+N | Shift+P / Shift+N | all (B562) |

### Global while something plays (any mode)

Bound by the music work (not yet in `BINDINGS`). Bare arrows are taken
by movement outside the player, so the transport is reached with Shift
there. Inside the player the bare keys still work.

| Action | Key |
|---|---|
| Play / pause, focus not on a button or link | Space |
| Seek back / forward 10 s | Shift+Left / Shift+Right |
| Volume up / down | Shift+Up / Shift+Down |
| Open the full music player | Shift+F |
| Switch between the mini and floating player | Shift+M |

Shift+F, Shift+M and `g q` are not bound in the video player, which has
no mini bar or floating window, so its key help does not list them (B619).

## Key help (which-key)

Every binding carries a short description and the mode it belongs to, so
the UI can teach itself:

- **Prefixes.** A key can start a sequence instead of acting. Pressing a
  prefix and pausing (500 ms) opens a small panel at the foot of the page
  listing the keys that can follow it, with their descriptions, for the
  current mode only. Pressing one runs it; Escape, or any key not listed,
  closes the panel. A sequence typed without pausing runs without the
  panel. A prefix waits for its next key for 2 s, then lapses.
- **`?`** opens the same panel listing every key of the current mode,
  grouped (Move, Play, Go to, Mode keys), standard and Vim side by side.
- The panel names the mode it is in ("Show page", "Player"), so a
  learner sees why the keys differ between pages.

### `g`: go to

| Keys | Goes to |
|---|---|
| `gg` | first item (Vim) |
| `gh` | home |
| `gt` | watch together |
| `gq` | now playing, the music full player (music thread) |
| `gs` | settings |
| `/` | (not under `g`) the palette, as Ctrl+K |

More prefixes can be added (a leader such as `,` for settings groups);
each is one row in the binding table with a group label.

## Modes

### Browse: home, libraries, grids, rails, album track lists

Movement keys move between tiles by position on screen (spatial: Down
goes to the tile below). Enter or `o` opens the focused tile, or plays
the focused track.

| Action | Key |
|---|---|
| Add the focused item to the end of the queue (music) | `a` |
| Play the focused item next (music) | Shift+A |
| Start a Mix from the focused item (music) | `x` |

### Detail: a film, episode, series or season page

| Action | Key |
|---|---|
| Move between controls, tabs and episode rows | Arrows, h / j / k / l |
| Play or resume (the main action, focused on arrival) | Enter, or `p` from anywhere on the page |
| Play from the start | `r` |
| Watch together, from where you are | `t` |
| Watch together from the start | Shift+T |
| Mark watched / unwatched: the focused episode, else the page's item | `w` |
| Previous / next season | `[` / `]` |

On the show page, Left/Right move along the season tabs and Enter opens
the season; Up/Down move along the episode rows, keeping the column (open,
mark, play). The page's Play or Resume plays the next episode of the
season shown and names it ("Resume S2 · E3").

### Player: video and the music full player

| Action | Standard | Vim |
|---|---|---|
| Play / pause | Space | Space |
| Seek back / forward 10 s | Left / Right | h / l |
| Volume up / down | Up / Down, `+` / `-` | k / j |
| Mute | `m` | |
| Seek to 0 %, 10 % … 90 % | `0` … `9` | |
| Fullscreen (video) | `f` | |
| Next subtitle track, cycling through off (video) | `c` | |
| Next audio track, cycling (video) | `a` | |
| Shrink to the library (music) | Escape, Backspace | |

The player's keys work with the focus anywhere on the page: the key
layer sends one pressed outside the player on to it (B618). After a
prefix the next key is the prefix's, so `g h` goes home from the player
rather than seeking back (B617).

The Vim keys mirror the arrows in the player too: `k` and `j` set the
volume like Up and Down (Alison, 2026-11-06), so `k` no longer plays and
pauses; Space does.

### Queue: the music full player's queue

| Action | Key |
|---|---|
| Focus the queue | `q`, or Tab |
| Move along the queue | Arrows, j / k, Home / End, gg / G |
| Play the focused track | Enter |
| Remove the focused track | `d`, Delete |
| Move the focused track up / down | Shift+K / Shift+J |
| Shuffle on / off | `s` |
| Clear the queue (asks first) | Shift+C |
| Open Mix setup | `x` |
| Lyrics on / off | `y` (`l` is Vim right) |

## Code

- `crates/pharos-ui/src/native/keymap.rs` (pure, unit-tested):
  - `BINDINGS`: every binding as `Binding { keys, label, group, modes,
    action }`, `keys` a sequence such as `&["g", "h"]`; `Mode` names the
    modes, `PREFIXES` the keys that start a sequence. The dispatcher and
    the help both read this one table, so a key cannot be bound without a
    description. A test asserts that within a mode no two bindings share a
    sequence and none hides another, and that every sequence starts with a
    declared prefix (V510).
  - `resolve(mode, &keys) -> Resolved::{Run(action), Pending(prefix),
    Unbound}`; `transport(mode, key)` for a player's own key handler;
    `help(mode, after)` for the panel's rows; `pick(dir, from, rects)` for
    the spatial choice of the next control.
- `crates/pharos-ui/src/native/keys.rs`:
  - `KeyLayer`: mounted once by the app; runs the current mode's bindings
    from a page-wide `keydown` listener (`web.rs` `on_mode_key`, which
    skips text fields, Ctrl/Alt/Cmd chords, repeats and keys a nearer
    handler took) and draws `KeyHelp`, the which-key panel.
  - `use_key_mode(mode)`: puts the keys in `mode` while the calling page
    is shown; the watch route is always Player, any other page without a
    mode is Browse.
  - `use_arrival_focus(key, id)`: focuses element `id` without scrolling
    when `key` changes, unless the focus is on a control inside `main`.
- Markup hooks the layer reads:
  - `data-keys`: an area the movement keys move in (the app shell is one).
  - `data-keys-list`: where First and Last stay (a show's episode list).
  - `data-key-row`: a row whose own control a page key presses first.
  - `data-key-action="play|restart|together|together-start|watched"`:
    the control a page key presses.
  - `data-key-tabs`: the strip `[` and `]` step along; the current tab
    carries `aria-current`.
- A music page adds its modes' rows to `BINDINGS`, calls
  `use_key_mode(Mode::Music)` or `Mode::Queue`, and marks its lists with
  the hooks above; `listen.rs` reads its transport keys with
  `keymap::transport(Mode::Music, key)`.
