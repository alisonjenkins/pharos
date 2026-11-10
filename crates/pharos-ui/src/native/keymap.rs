//! Keyboard modes (spec 029): every key binding with its description, and
//! the pure choices the key layer makes from them. The dispatcher and the
//! which-key help both read [`BINDINGS`], so a key cannot be bound without
//! saying what it does.

/// The context the focus is in. Within a mode no two bindings share keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Mode {
    /// Home, libraries, grids and rails.
    Browse,
    /// A film, episode, series or season page.
    Detail,
    /// The video player.
    Player,
    /// The music full player.
    Music,
    /// The music full player's queue.
    Queue,
    /// The book reader, whose page-turn keys are its own.
    Reader,
}

impl Mode {
    /// The mode's name as the help panel shows it.
    pub fn label(self) -> &'static str {
        match self {
            Mode::Browse => "Browsing",
            Mode::Detail => "Title page",
            Mode::Player => "Player",
            Mode::Music => "Music player",
            Mode::Queue => "Queue",
            Mode::Reader => "Reading",
        }
    }
}

/// Where a binding applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Modes {
    All,
    Only(&'static [Mode]),
}

impl Modes {
    pub fn has(self, mode: Mode) -> bool {
        match self {
            Modes::All => true,
            Modes::Only(modes) => modes.contains(&mode),
        }
    }
}

/// A direction the focus moves in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    Up,
    Down,
    Left,
    Right,
    First,
    Last,
}

/// A place `g` goes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Place {
    Home,
    Together,
    Settings,
}

/// What the player does for a key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    TogglePlay,
    SeekBack,
    SeekForward,
    VolumeUp,
    VolumeDown,
    Mute,
    /// Seek to this many tenths of the way in.
    SeekTenths(u8),
    Fullscreen,
    NextSubtitle,
    NextAudio,
    /// Skip the intro, recap or credits whose button is showing.
    SkipSegment,
}

/// What a binding does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Listed for the help, but the browser or a control already does it
    /// (Enter, Escape): the dispatcher leaves the key alone.
    Native,
    Move(Dir),
    /// Activate the focused control, as Enter does.
    Activate,
    Help,
    Palette,
    Go(Place),
    /// Press the control marked `data-key-action="<name>"`: the focused
    /// row's when it has one, else the page's.
    Press(&'static str),
    /// Open the season tab before (`-1`) or after (`1`) the one shown.
    Season(i8),
    /// Focus the first control in the `data-key-area="<name>"` area,
    /// pressing `data-key-action="<name>"` first to show it when it is
    /// hidden (the music player's queue).
    Focus(&'static str),
    Transport(Transport),
}

/// The heading a binding is listed under in the help.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Group {
    Move,
    Play,
    Page,
    GoTo,
    General,
}

impl Group {
    pub fn label(self) -> &'static str {
        match self {
            Group::Move => "Move",
            Group::Play => "Play",
            Group::Page => "This page",
            Group::GoTo => "Go to",
            Group::General => "General",
        }
    }
}

/// One key sequence, what it does, and where.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Binding {
    /// `KeyboardEvent.key` values in order: `["g", "h"]`, `["ArrowUp"]`.
    pub keys: &'static [&'static str],
    pub label: &'static str,
    pub group: Group,
    pub modes: Modes,
    pub action: Action,
}

/// A key that starts a sequence, and its name in the help.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Prefix {
    pub key: &'static str,
    pub label: &'static str,
}

pub const PREFIXES: &[Prefix] = &[Prefix {
    key: "g",
    label: "Go to",
}];

const MOVING: Modes = Modes::Only(&[Mode::Browse, Mode::Detail, Mode::Queue]);
const BROWSE: Modes = Modes::Only(&[Mode::Browse]);
/// Where a song, an album, a film or an episode can be queued (V853).
const QUEUEING: Modes = Modes::Only(&[Mode::Browse, Mode::Detail]);
const MUSIC: Modes = Modes::Only(&[Mode::Music]);
const QUEUE: Modes = Modes::Only(&[Mode::Queue]);
const LISTENING: Modes = Modes::Only(&[Mode::Music, Mode::Queue]);
/// Escape is each full player's way back to the library.
const NOT_PLAYING: Modes = Modes::Only(&[Mode::Browse, Mode::Detail, Mode::Queue]);
const DETAIL: Modes = Modes::Only(&[Mode::Detail]);
const PLAYERS: Modes = Modes::Only(&[Mode::Player, Mode::Music]);
const VIDEO: Modes = Modes::Only(&[Mode::Player]);
/// The music player's size keys mean nothing on the video page, which has
/// no mini bar or floating window.
const NOT_VIDEO: Modes = Modes::Only(&[Mode::Browse, Mode::Detail, Mode::Music, Mode::Queue]);
/// Where the music player is the mini bar or floating window, if anything.
const SMALL_PLAYER: Modes = Modes::Only(&[Mode::Browse, Mode::Detail]);

const fn bind(
    keys: &'static [&'static str],
    label: &'static str,
    group: Group,
    modes: Modes,
    action: Action,
) -> Binding {
    Binding {
        keys,
        label,
        group,
        modes,
        action,
    }
}

use Action as A;
use Group as G;

/// Every binding. Standard and Vim keys for one action are separate rows
/// with the same label; the help joins them.
pub const BINDINGS: &[Binding] = &[
    bind(&["ArrowUp"], "Up", G::Move, MOVING, A::Move(Dir::Up)),
    bind(&["k"], "Up", G::Move, MOVING, A::Move(Dir::Up)),
    bind(&["ArrowDown"], "Down", G::Move, MOVING, A::Move(Dir::Down)),
    bind(&["j"], "Down", G::Move, MOVING, A::Move(Dir::Down)),
    bind(&["ArrowLeft"], "Left", G::Move, MOVING, A::Move(Dir::Left)),
    bind(&["h"], "Left", G::Move, MOVING, A::Move(Dir::Left)),
    bind(
        &["ArrowRight"],
        "Right",
        G::Move,
        MOVING,
        A::Move(Dir::Right),
    ),
    bind(&["l"], "Right", G::Move, MOVING, A::Move(Dir::Right)),
    bind(&["Home"], "First", G::Move, MOVING, A::Move(Dir::First)),
    bind(&["g", "g"], "First", G::Move, MOVING, A::Move(Dir::First)),
    bind(&["End"], "Last", G::Move, MOVING, A::Move(Dir::Last)),
    bind(&["G"], "Last", G::Move, MOVING, A::Move(Dir::Last)),
    bind(
        &["Enter"],
        "Open or play what is focused",
        G::Move,
        MOVING,
        A::Native,
    ),
    bind(
        &["o"],
        "Open or play what is focused",
        G::Move,
        MOVING,
        A::Activate,
    ),
    bind(&["p"], "Play or resume", G::Page, DETAIL, A::Press("play")),
    bind(
        &["r"],
        "Play from the start",
        G::Page,
        DETAIL,
        A::Press("restart"),
    ),
    bind(
        &["t"],
        "Watch together",
        G::Page,
        DETAIL,
        A::Press("together"),
    ),
    bind(
        &["T"],
        "Watch together from the start",
        G::Page,
        DETAIL,
        A::Press("together-start"),
    ),
    bind(
        &["w"],
        "Mark watched or unwatched",
        G::Page,
        DETAIL,
        A::Press("watched"),
    ),
    bind(
        &["f"],
        "Add to or remove from favourites",
        G::Page,
        DETAIL,
        A::Press("favorite"),
    ),
    bind(&["["], "Previous season", G::Page, DETAIL, A::Season(-1)),
    bind(&["]"], "Next season", G::Page, DETAIL, A::Season(1)),
    bind(
        &[" "],
        "Play or pause",
        G::Play,
        PLAYERS,
        A::Transport(Transport::TogglePlay),
    ),
    bind(
        &["ArrowLeft"],
        "Back 10 s",
        G::Play,
        PLAYERS,
        A::Transport(Transport::SeekBack),
    ),
    bind(
        &["h"],
        "Back 10 s",
        G::Play,
        PLAYERS,
        A::Transport(Transport::SeekBack),
    ),
    bind(
        &["ArrowRight"],
        "Forward 10 s",
        G::Play,
        PLAYERS,
        A::Transport(Transport::SeekForward),
    ),
    bind(
        &["l"],
        "Forward 10 s",
        G::Play,
        PLAYERS,
        A::Transport(Transport::SeekForward),
    ),
    bind(
        &["ArrowUp"],
        "Volume up",
        G::Play,
        PLAYERS,
        A::Transport(Transport::VolumeUp),
    ),
    bind(
        &["k"],
        "Volume up",
        G::Play,
        PLAYERS,
        A::Transport(Transport::VolumeUp),
    ),
    bind(
        &["j"],
        "Volume down",
        G::Play,
        PLAYERS,
        A::Transport(Transport::VolumeDown),
    ),
    bind(
        &["+"],
        "Volume up",
        G::Play,
        PLAYERS,
        A::Transport(Transport::VolumeUp),
    ),
    bind(
        &["="],
        "Volume up",
        G::Play,
        PLAYERS,
        A::Transport(Transport::VolumeUp),
    ),
    bind(
        &["ArrowDown"],
        "Volume down",
        G::Play,
        PLAYERS,
        A::Transport(Transport::VolumeDown),
    ),
    bind(
        &["-"],
        "Volume down",
        G::Play,
        PLAYERS,
        A::Transport(Transport::VolumeDown),
    ),
    bind(
        &["m"],
        "Mute",
        G::Play,
        PLAYERS,
        A::Transport(Transport::Mute),
    ),
    bind(
        &["0"],
        "Seek to 0–90 %",
        G::Play,
        PLAYERS,
        A::Transport(Transport::SeekTenths(0)),
    ),
    bind(
        &["1"],
        "Seek to 0–90 %",
        G::Play,
        PLAYERS,
        A::Transport(Transport::SeekTenths(1)),
    ),
    bind(
        &["2"],
        "Seek to 0–90 %",
        G::Play,
        PLAYERS,
        A::Transport(Transport::SeekTenths(2)),
    ),
    bind(
        &["3"],
        "Seek to 0–90 %",
        G::Play,
        PLAYERS,
        A::Transport(Transport::SeekTenths(3)),
    ),
    bind(
        &["4"],
        "Seek to 0–90 %",
        G::Play,
        PLAYERS,
        A::Transport(Transport::SeekTenths(4)),
    ),
    bind(
        &["5"],
        "Seek to 0–90 %",
        G::Play,
        PLAYERS,
        A::Transport(Transport::SeekTenths(5)),
    ),
    bind(
        &["6"],
        "Seek to 0–90 %",
        G::Play,
        PLAYERS,
        A::Transport(Transport::SeekTenths(6)),
    ),
    bind(
        &["7"],
        "Seek to 0–90 %",
        G::Play,
        PLAYERS,
        A::Transport(Transport::SeekTenths(7)),
    ),
    bind(
        &["8"],
        "Seek to 0–90 %",
        G::Play,
        PLAYERS,
        A::Transport(Transport::SeekTenths(8)),
    ),
    bind(
        &["9"],
        "Seek to 0–90 %",
        G::Play,
        PLAYERS,
        A::Transport(Transport::SeekTenths(9)),
    ),
    bind(
        &["f"],
        "Fullscreen",
        G::Play,
        VIDEO,
        A::Transport(Transport::Fullscreen),
    ),
    bind(
        &["c"],
        "Next subtitles",
        G::Play,
        VIDEO,
        A::Transport(Transport::NextSubtitle),
    ),
    bind(
        &["a"],
        "Next audio track",
        G::Play,
        VIDEO,
        A::Transport(Transport::NextAudio),
    ),
    bind(
        &["s"],
        "Skip intro or credits",
        G::Play,
        VIDEO,
        A::Transport(Transport::SkipSegment),
    ),
    bind(
        &["a"],
        "Add to the end of the queue",
        G::Page,
        QUEUEING,
        A::Press("queue-end"),
    ),
    bind(
        &["A"],
        "Play next",
        G::Page,
        QUEUEING,
        A::Press("queue-next"),
    ),
    bind(
        &["x"],
        "Start a mix like it",
        G::Page,
        BROWSE,
        A::Press("mix"),
    ),
    bind(
        &["F"],
        "Open the full player",
        G::Play,
        NOT_VIDEO,
        A::Press("full-player"),
    ),
    bind(
        &["M"],
        "Mini bar or floating player",
        G::Play,
        SMALL_PLAYER,
        A::Press("player-size"),
    ),
    // In the full player the same key makes it small again.
    bind(
        &["M"],
        "Shrink to the mini bar or floating player",
        G::Play,
        LISTENING,
        A::Press("shrink"),
    ),
    bind(
        &["Escape"],
        "Back to the library",
        G::Page,
        MUSIC,
        A::Press("shrink"),
    ),
    // The video goes on in the mini bar or floating window (B905).
    bind(
        &["Escape"],
        "Shrink to the mini bar or floating player",
        G::Play,
        VIDEO,
        A::Press("shrink"),
    ),
    bind(
        &["Backspace"],
        "Back to the library",
        G::Page,
        MUSIC,
        A::Press("shrink"),
    ),
    bind(&["q"], "Go to the queue", G::Page, MUSIC, A::Focus("queue")),
    // The video queue, opened if it is closed (V853).
    bind(&["q"], "Go to the queue", G::Page, VIDEO, A::Focus("queue")),
    bind(
        &["s"],
        "Shuffle on or off",
        G::Page,
        LISTENING,
        A::Press("shuffle"),
    ),
    bind(
        &["C"],
        "Clear the queue",
        G::Page,
        LISTENING,
        A::Press("clear"),
    ),
    bind(&["x"], "Mix", G::Page, LISTENING, A::Press("mix")),
    bind(
        &["y"],
        "Lyrics on or off",
        G::Page,
        LISTENING,
        A::Press("lyrics"),
    ),
    bind(
        &["d"],
        "Remove from the queue",
        G::Page,
        QUEUE,
        A::Press("remove"),
    ),
    bind(
        &["Delete"],
        "Remove from the queue",
        G::Page,
        QUEUE,
        A::Press("remove"),
    ),
    bind(
        &["K"],
        "Move up the queue",
        G::Page,
        QUEUE,
        A::Press("move-up"),
    ),
    bind(
        &["J"],
        "Move down the queue",
        G::Page,
        QUEUE,
        A::Press("move-down"),
    ),
    bind(&["g", "h"], "Home", G::GoTo, Modes::All, A::Go(Place::Home)),
    bind(
        &["g", "q"],
        "Now playing",
        G::GoTo,
        NOT_VIDEO,
        A::Press("full-player"),
    ),
    bind(
        &["g", "t"],
        "Watch together",
        G::GoTo,
        Modes::All,
        A::Go(Place::Together),
    ),
    bind(
        &["g", "s"],
        "Settings",
        G::GoTo,
        Modes::All,
        A::Go(Place::Settings),
    ),
    bind(&["/"], "Search", G::General, Modes::All, A::Palette),
    bind(&["?"], "Show these keys", G::General, Modes::All, A::Help),
    bind(
        &["Escape"],
        "Back, or close",
        G::General,
        NOT_PLAYING,
        A::Native,
    ),
];

/// What the keys typed so far come to in a mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolved {
    Run(Action),
    /// A sequence has begun: wait for its next key.
    Pending(Prefix),
    Unbound,
}

pub fn resolve(mode: Mode, keys: &[&str]) -> Resolved {
    let live = || BINDINGS.iter().filter(|b| b.modes.has(mode));
    if let Some(b) = live().find(|b| b.keys == keys) {
        return Resolved::Run(b.action);
    }
    let longer = live().any(|b| b.keys.len() > keys.len() && b.keys.starts_with(keys));
    match keys.first() {
        Some(first) if longer => PREFIXES
            .iter()
            .find(|p| p.key == *first)
            .map_or(Resolved::Unbound, |p| Resolved::Pending(*p)),
        _ => Resolved::Unbound,
    }
}

/// The player's action for one key, for the player's own key handler.
pub fn transport(mode: Mode, key: &str) -> Option<Transport> {
    match resolve(mode, &[key]) {
        Resolved::Run(Action::Transport(t)) => Some(t),
        _ => None,
    }
}

/// How a key is written in the help.
pub fn key_name(key: &str) -> &str {
    match key {
        "ArrowUp" => "↑",
        "ArrowDown" => "↓",
        "ArrowLeft" => "←",
        "ArrowRight" => "→",
        " " => "Space",
        "Escape" => "Esc",
        other => other,
    }
}

/// One line of the help: a description and every sequence that does it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HelpRow {
    pub group: Group,
    pub label: &'static str,
    /// Each sequence written out, such as `↑` or `g h`.
    pub keys: Vec<String>,
}

/// The help for `mode`: after a prefix, the keys that can follow it;
/// without one, every key of the mode whose action `offered` says the page
/// can do now, so a film's page lists no season keys. Rows sharing a label
/// are joined.
pub fn help(mode: Mode, after: Option<&str>, offered: impl Fn(Action) -> bool) -> Vec<HelpRow> {
    let mut rows: Vec<HelpRow> = Vec::new();
    for b in BINDINGS
        .iter()
        .filter(|b| b.modes.has(mode) && offered(b.action))
    {
        let shown: Vec<&str> = match after {
            Some(p) => match b.keys.split_first() {
                Some((first, rest)) if *first == p && !rest.is_empty() => rest.to_vec(),
                _ => continue,
            },
            None => b.keys.to_vec(),
        };
        let written = shown
            .iter()
            .map(|k| key_name(k))
            .collect::<Vec<_>>()
            .join(" ");
        match rows
            .iter_mut()
            .find(|r| r.label == b.label && r.group == b.group)
        {
            Some(row) => row.keys.push(written),
            None => rows.push(HelpRow {
                group: b.group,
                label: b.label,
                keys: vec![written],
            }),
        }
    }
    for row in &mut rows {
        if let Some(run) = digit_run(&row.keys) {
            row.keys = vec![run];
        }
    }
    rows.sort_by_key(|r| r.group);
    rows
}

/// `0–9` for the keys `0` to `9` in order, so the panel shows one key,
/// not ten, for the seek-to-a-tenth row.
fn digit_run(keys: &[String]) -> Option<String> {
    let digits: Vec<u32> = keys
        .iter()
        .map(|k| {
            let mut c = k.chars();
            match (c.next(), c.next()) {
                (Some(d), None) => d.to_digit(10),
                _ => None,
            }
        })
        .collect::<Option<_>>()?;
    let (first, last) = (*digits.first()?, *digits.last()?);
    let contiguous = digits.windows(2).all(|w| w[1] == w[0] + 1);
    (contiguous && digits.len() > 2).then(|| format!("{first}–{last}"))
}

/// A control's box on screen: left, top, width, height.
pub type Rect = (f64, f64, f64, f64);

/// The control the focus moves to from `from` in `dir`, among `controls`
/// (the focused one may be among them). Up and Down take the nearest
/// control wholly above or below, keeping the column; Left and Right the
/// nearest on the same line. First and Last are the first and last in
/// page order.
pub fn pick(dir: Dir, from: Rect, controls: &[Rect]) -> Option<usize> {
    let (fx, fy, fw, fh) = from;
    let centre = |r: &Rect| (r.0 + r.2 / 2.0, r.1 + r.3 / 2.0);
    let (fcx, fcy) = centre(&from);
    // Half a pixel of slack: boxes that touch are not overlapping.
    const SLACK: f64 = 0.5;
    let score = |r: &Rect| -> Option<f64> {
        let (cx, cy) = centre(r);
        let same_line = r.1 < fy + fh - SLACK && r.1 + r.3 > fy + SLACK;
        match dir {
            Dir::Down if r.1 >= fy + fh - SLACK => Some((r.1 - (fy + fh)) * 4.0 + (cx - fcx).abs()),
            Dir::Up if r.1 + r.3 <= fy + SLACK => Some((fy - (r.1 + r.3)) * 4.0 + (cx - fcx).abs()),
            Dir::Right if same_line && r.0 >= fx + fw - SLACK => {
                Some(r.0 - (fx + fw) + (cy - fcy).abs())
            }
            Dir::Left if same_line && r.0 + r.2 <= fx + SLACK => {
                Some(fx - (r.0 + r.2) + (cy - fcy).abs())
            }
            _ => None,
        }
    };
    match dir {
        Dir::First => (!controls.is_empty()).then_some(0),
        Dir::Last => controls.len().checked_sub(1),
        _ => controls
            .iter()
            .enumerate()
            .filter_map(|(i, r)| score(r).map(|s| (i, s)))
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(i, _)| i),
    }
}

/// [`pick`] with the player docked in a bar along the bottom (the TV's
/// compact player) reached last: from the page, Down reaches the bar only
/// with no page control below, so the arrows still scroll the page; in the
/// bar, Left and Right stay in it and Up goes back to the page. `controls`
/// pairs each rect with whether it is in the bar.
pub fn pick_docked(
    dir: Dir,
    from: Rect,
    from_docked: bool,
    controls: &[(Rect, bool)],
) -> Option<usize> {
    let among = |docked: bool| -> Option<usize> {
        let (index, rects): (Vec<usize>, Vec<Rect>) = controls
            .iter()
            .enumerate()
            .filter(|(_, (_, d))| *d == docked)
            .map(|(i, (r, _))| (i, *r))
            .unzip();
        pick(dir, from, &rects).map(|at| index[at])
    };
    match (dir, from_docked) {
        (Dir::First | Dir::Last, _) => {
            let rects: Vec<Rect> = controls.iter().map(|(r, _)| *r).collect();
            pick(dir, from, &rects)
        }
        // The bar is fixed over the page's foot, so the rows scrolled to the
        // foot can sit partly under it, with no bar control wholly below
        // them nor page control wholly above it (B1035): Down then takes the
        // bar's control nearest across, and Up the page's nearest whose
        // middle is above the bar.
        (Dir::Down, false) => among(false)
            .or_else(|| among(true))
            .or_else(|| nearest_across(from, controls)),
        (Dir::Up, true) => among(false).or_else(|| nearest_above(from, controls)),
        (Dir::Down, true) => None,
        (_, docked) => among(docked),
    }
}

/// The page control nearest above `from` whose middle is above its top.
fn nearest_above(from: Rect, controls: &[(Rect, bool)]) -> Option<usize> {
    let fcx = from.0 + from.2 / 2.0;
    controls
        .iter()
        .enumerate()
        .filter(|(_, (_, docked))| !*docked)
        .filter_map(|(i, (r, _))| {
            let (cx, cy) = (r.0 + r.2 / 2.0, r.1 + r.3 / 2.0);
            (cy < from.1).then(|| (i, (from.1 - cy) * 4.0 + (cx - fcx).abs()))
        })
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(i, _)| i)
}

/// The docked control whose centre is nearest `from`'s across the screen.
fn nearest_across(from: Rect, controls: &[(Rect, bool)]) -> Option<usize> {
    let centre = |r: &Rect| r.0 + r.2 / 2.0;
    let fcx = centre(&from);
    controls
        .iter()
        .enumerate()
        .filter(|(_, (_, docked))| *docked)
        .map(|(i, (r, _))| (i, (centre(r) - fcx).abs()))
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(i, _)| i)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    /// B1035/V995: the last row scrolled to the foot of the screen sits
    /// under the fixed bar; Down still reaches the bar, at the control
    /// nearest across, and Up leaves it for the page.
    #[test]
    fn down_reaches_a_bar_laid_over_the_last_row() {
        // The TV's 960x540 page: the last track ends at the screen's foot,
        // the bar's buttons sit over it.
        let ninth = (96.0, 464.0, 830.0, 38.0);
        let last = (96.0, 502.0, 830.0, 38.0);
        let queue = (880.0, 506.0, 30.0, 30.0);
        let play = (784.0, 495.0, 32.0, 32.0);
        let close = (920.0, 495.0, 32.0, 32.0);
        let page = [
            (ninth, false),
            (last, false),
            (queue, false),
            (play, true),
            (close, true),
        ];
        assert_eq!(pick_docked(Dir::Down, last, false, &page), Some(3));
        assert_eq!(pick_docked(Dir::Down, queue, false, &page), Some(4));
        assert_eq!(pick_docked(Dir::Down, ninth, false, &page), Some(1));
        assert_eq!(pick_docked(Dir::Up, play, true, &page), Some(0));
        assert_eq!(pick_docked(Dir::Down, play, true, &page), None);
    }

    #[test]
    fn the_docked_bar_is_reached_last_and_left_by_up() {
        let play = (0.0, 650.0, 40.0, 40.0);
        let close = (60.0, 650.0, 40.0, 40.0);
        // A row on screen and one below the fold, under the fixed bar: Down
        // goes on down the page.
        let row = (0.0, 100.0, 100.0, 50.0);
        let below = (0.0, 900.0, 100.0, 50.0);
        let page = [(row, false), (below, false), (play, true), (close, true)];
        assert_eq!(pick_docked(Dir::Down, row, false, &page), Some(1));
        assert_eq!(pick_docked(Dir::Left, row, false, &page), None);
        // Scrolled to the end, the last row sits above the bar: Down
        // reaches it, Right moves in it, Up goes back to the page.
        let last = (0.0, 500.0, 100.0, 50.0);
        let end = [(row, false), (last, false), (play, true), (close, true)];
        assert_eq!(pick_docked(Dir::Down, last, false, &end), Some(2));
        assert_eq!(pick_docked(Dir::Right, play, true, &end), Some(3));
        assert_eq!(pick_docked(Dir::Up, play, true, &end), Some(1));
        assert_eq!(pick_docked(Dir::Down, play, true, &end), None);
    }

    const ALL: [Mode; 6] = [
        Mode::Browse,
        Mode::Detail,
        Mode::Player,
        Mode::Music,
        Mode::Queue,
        Mode::Reader,
    ];

    /// V510: within a mode no two bindings share a sequence, and no binding
    /// is the start of another unless it opens a declared prefix.
    #[test]
    fn no_mode_binds_a_sequence_twice_or_shadows_one() {
        for mode in ALL {
            let live: Vec<&Binding> = BINDINGS.iter().filter(|b| b.modes.has(mode)).collect();
            for (i, a) in live.iter().enumerate() {
                for b in live.iter().skip(i + 1) {
                    assert_ne!(a.keys, b.keys, "{mode:?} binds {:?} twice", a.keys);
                    let (short, long) = if a.keys.len() < b.keys.len() {
                        (a, b)
                    } else {
                        (b, a)
                    };
                    assert!(
                        !long.keys.starts_with(short.keys),
                        "{mode:?}: {:?} hides {:?}",
                        short.keys,
                        long.keys
                    );
                }
                if a.keys.len() > 1 {
                    assert!(
                        PREFIXES.iter().any(|p| Some(&p.key) == a.keys.first()),
                        "{:?} starts with an undeclared prefix",
                        a.keys
                    );
                }
            }
        }
    }

    /// V854: Escape in the full video player shrinks it to the mini bar or
    /// floating window, as it leaves the music player; elsewhere it is the
    /// browser's (a menu, a dialog, leaving fullscreen).
    #[test]
    fn escape_shrinks_the_full_video_player() {
        assert_eq!(
            resolve(Mode::Player, &["Escape"]),
            Resolved::Run(Action::Press("shrink"))
        );
        for mode in [Mode::Browse, Mode::Detail, Mode::Queue] {
            assert_eq!(
                resolve(mode, &["Escape"]),
                Resolved::Run(Action::Native),
                "{mode:?}"
            );
        }
    }

    /// V853: films and episodes queue from their pages as songs do from
    /// the library, and q goes to the video player's queue.
    #[test]
    fn videos_queue_from_their_pages_and_q_finds_the_queue() {
        for mode in [Mode::Browse, Mode::Detail] {
            assert_eq!(
                resolve(mode, &["a"]),
                Resolved::Run(Action::Press("queue-end")),
                "{mode:?}"
            );
            assert_eq!(
                resolve(mode, &["A"]),
                Resolved::Run(Action::Press("queue-next")),
                "{mode:?}"
            );
        }
        assert_eq!(
            resolve(Mode::Player, &["q"]),
            Resolved::Run(Action::Focus("queue"))
        );
    }

    /// V512: the music player, its queue and the album pages share the
    /// table: Escape leaves the player, the queue moves with j/k, and the
    /// same key queues or mixes wherever a song is focused.
    #[test]
    fn music_keys_resolve_by_mode() {
        let run = |mode, keys: &[&str]| match resolve(mode, keys) {
            Resolved::Run(a) => Some(a),
            _ => None,
        };
        assert_eq!(run(Mode::Music, &["Escape"]), Some(Action::Press("shrink")));
        for mode in [Mode::Music, Mode::Queue] {
            assert_eq!(run(mode, &["M"]), Some(Action::Press("shrink")), "{mode:?}");
        }
        for mode in [Mode::Browse, Mode::Detail] {
            assert_eq!(
                run(mode, &["M"]),
                Some(Action::Press("player-size")),
                "{mode:?}"
            );
        }
        assert_eq!(run(Mode::Queue, &["Escape"]), Some(Action::Native));
        assert_eq!(run(Mode::Music, &["q"]), Some(Action::Focus("queue")));
        assert_eq!(
            run(Mode::Music, &["k"]),
            Some(Action::Transport(Transport::VolumeUp))
        );
        assert_eq!(run(Mode::Queue, &["k"]), Some(Action::Move(Dir::Up)));
        assert_eq!(
            run(Mode::Queue, &["g", "g"]),
            Some(Action::Move(Dir::First))
        );
        assert_eq!(run(Mode::Queue, &["K"]), Some(Action::Press("move-up")));
        assert_eq!(run(Mode::Browse, &["a"]), Some(Action::Press("queue-end")));
        assert_eq!(
            run(Mode::Player, &["a"]),
            Some(Action::Transport(Transport::NextAudio))
        );
        for mode in [Mode::Music, Mode::Queue, Mode::Browse] {
            assert_eq!(run(mode, &["x"]), Some(Action::Press("mix")), "{mode:?}");
            assert_eq!(
                run(mode, &["g", "q"]),
                Some(Action::Press("full-player")),
                "{mode:?}"
            );
        }
    }

    #[test]
    fn the_video_player_lists_no_music_player_keys() {
        for keys in [&["F"][..], &["M"], &["g", "q"]] {
            assert_eq!(resolve(Mode::Player, keys), Resolved::Unbound, "{keys:?}");
        }
        let labels: Vec<&str> = help(Mode::Player, None, |_| true)
            .iter()
            .map(|r| r.label)
            .collect();
        for gone in [
            "Open the full player",
            "Mini bar or floating player",
            "Now playing",
        ] {
            assert!(!labels.contains(&gone), "{gone}");
        }
        assert_eq!(
            resolve(Mode::Browse, &["F"]),
            Resolved::Run(Action::Press("full-player"))
        );
    }

    #[test]
    fn the_help_lists_only_what_the_page_offers() {
        // A film's page: no season tabs, no group to watch with.
        let film = |a: Action| {
            !matches!(
                a,
                Action::Season(_) | Action::Press("together" | "together-start")
            )
        };
        let labels: Vec<&str> = help(Mode::Detail, None, film)
            .iter()
            .filter(|r| r.group == Group::Page)
            .map(|r| r.label)
            .collect();
        assert!(labels.contains(&"Play or resume"));
        for gone in [
            "Previous season",
            "Next season",
            "Watch together",
            "Watch together from the start",
        ] {
            assert!(!labels.contains(&gone), "{gone}");
        }
        // Go to's Watch together is a place, always offered.
        let after_g: Vec<&str> = help(Mode::Detail, Some("g"), film)
            .iter()
            .map(|r| r.label)
            .collect();
        assert!(after_g.contains(&"Watch together"));
    }

    #[test]
    fn the_seek_to_a_tenth_keys_show_as_one_run() {
        let row = help(Mode::Player, None, |_| true)
            .into_iter()
            .find(|r| r.keys.iter().any(|k| k.contains('–')))
            .expect("a run of digits");
        assert_eq!(row.keys, vec!["0–9".to_owned()]);
    }

    #[test]
    fn every_binding_says_what_it_does() {
        assert!(BINDINGS
            .iter()
            .all(|b| !b.label.trim().is_empty() && !b.keys.is_empty()));
    }

    #[test]
    fn vim_and_standard_keys_do_the_same_thing() {
        for (standard, vim) in [
            ("ArrowUp", "k"),
            ("ArrowDown", "j"),
            ("ArrowLeft", "h"),
            ("ArrowRight", "l"),
        ] {
            for mode in [Mode::Browse, Mode::Detail] {
                assert_eq!(
                    resolve(mode, &[standard]),
                    resolve(mode, &[vim]),
                    "{mode:?} {vim}"
                );
            }
        }
        assert_eq!(
            resolve(Mode::Browse, &["Home"]),
            resolve(Mode::Browse, &["g", "g"])
        );
        assert_eq!(
            resolve(Mode::Browse, &["End"]),
            resolve(Mode::Browse, &["G"])
        );
        assert_eq!(
            resolve(Mode::Player, &["ArrowLeft"]),
            resolve(Mode::Player, &["h"])
        );
    }

    /// 023: the reader turns pages on these keys itself, so the layer must
    /// neither move focus on them nor take the press.
    #[test]
    fn the_reader_leaves_its_page_turn_keys_unbound() {
        for key in [
            "ArrowLeft",
            "ArrowRight",
            "PageUp",
            "PageDown",
            " ",
            "Escape",
        ] {
            assert_eq!(resolve(Mode::Reader, &[key]), Resolved::Unbound, "{key}");
        }
        assert_eq!(
            resolve(Mode::Reader, &["g", "h"]),
            Resolved::Run(Action::Go(Place::Home))
        );
    }

    #[test]
    fn g_waits_for_its_next_key_and_names_the_group() {
        assert_eq!(
            resolve(Mode::Detail, &["g"]),
            Resolved::Pending(PREFIXES[0])
        );
        assert_eq!(
            resolve(Mode::Detail, &["g", "h"]),
            Resolved::Run(Action::Go(Place::Home))
        );
        assert_eq!(resolve(Mode::Detail, &["g", "z"]), Resolved::Unbound);
        // The player has no `gg`, but `g` still goes places from it.
        assert_eq!(
            resolve(Mode::Player, &["g"]),
            Resolved::Pending(PREFIXES[0])
        );
    }

    #[test]
    fn the_player_keeps_its_shipped_keys_and_gains_vim_seeks() {
        let t = |k| transport(Mode::Player, k);
        assert_eq!(t(" "), Some(Transport::TogglePlay));
        // The Vim keys mirror the arrows: k and j set the volume like Up
        // and Down, so k no longer plays and pauses.
        assert_eq!(t("k"), Some(Transport::VolumeUp));
        assert_eq!(t("j"), Some(Transport::VolumeDown));
        assert_eq!(t("h"), Some(Transport::SeekBack));
        assert_eq!(t("l"), Some(Transport::SeekForward));
        assert_eq!(t("ArrowUp"), Some(Transport::VolumeUp));
        assert_eq!(t("m"), Some(Transport::Mute));
        assert_eq!(t("3"), Some(Transport::SeekTenths(3)));
        assert_eq!(t("c"), Some(Transport::NextSubtitle));
        assert_eq!(t("a"), Some(Transport::NextAudio));
        // B805: `s` presses the skip button; the music player's `s` is shuffle.
        assert_eq!(t("s"), Some(Transport::SkipSegment));
        assert_eq!(transport(Mode::Music, "s"), None);
        let help_row = help(Mode::Player, None, |_| true)
            .into_iter()
            .find(|r| r.label == "Skip intro or credits");
        assert_eq!(help_row.map(|r| r.keys), Some(vec!["s".to_owned()]));
        // Video-only keys are not the music player's.
        assert_eq!(transport(Mode::Music, "c"), None);
        assert_eq!(transport(Mode::Music, "k"), Some(Transport::VolumeUp));
    }

    #[test]
    fn detail_keys_press_the_page_controls() {
        assert_eq!(
            resolve(Mode::Detail, &["p"]),
            Resolved::Run(Action::Press("play"))
        );
        assert_eq!(
            resolve(Mode::Detail, &["w"]),
            Resolved::Run(Action::Press("watched"))
        );
        assert_eq!(
            resolve(Mode::Detail, &["f"]),
            Resolved::Run(Action::Press("favorite"))
        );
        assert_eq!(
            resolve(Mode::Detail, &["]"]),
            Resolved::Run(Action::Season(1))
        );
        assert_eq!(resolve(Mode::Browse, &["p"]), Resolved::Unbound);
    }

    #[test]
    fn help_after_g_lists_only_what_follows_it() {
        let rows = help(Mode::Detail, Some("g"), |_| true);
        let labels: Vec<_> = rows.iter().map(|r| r.label).collect();
        assert_eq!(
            labels,
            ["First", "Home", "Now playing", "Watch together", "Settings"]
        );
        assert_eq!(rows[0].keys, ["g"]);
    }

    #[test]
    fn help_joins_standard_and_vim_keys_on_one_row() {
        let rows = help(Mode::Browse, None, |_| true);
        let up = rows.iter().find(|r| r.label == "Up").unwrap();
        assert_eq!(up.keys, ["↑", "k"]);
        let first = rows.iter().find(|r| r.label == "First").unwrap();
        assert_eq!(first.keys, ["Home", "g g"]);
        assert!(rows.windows(2).all(|w| w[0].group <= w[1].group));
    }

    // Three rows of two 100x40 controls, 10 px apart.
    const GRID: [Rect; 6] = [
        (0.0, 0.0, 100.0, 40.0),
        (110.0, 0.0, 100.0, 40.0),
        (0.0, 50.0, 100.0, 40.0),
        (110.0, 50.0, 100.0, 40.0),
        (0.0, 100.0, 100.0, 40.0),
        (110.0, 100.0, 100.0, 40.0),
    ];

    #[test]
    fn down_keeps_the_column_and_right_keeps_the_line() {
        assert_eq!(pick(Dir::Down, GRID[1], &GRID), Some(3));
        assert_eq!(pick(Dir::Down, GRID[3], &GRID), Some(5));
        assert_eq!(pick(Dir::Up, GRID[4], &GRID), Some(2));
        assert_eq!(pick(Dir::Right, GRID[2], &GRID), Some(3));
        assert_eq!(pick(Dir::Left, GRID[3], &GRID), Some(2));
    }

    #[test]
    fn nothing_further_is_no_move() {
        assert_eq!(pick(Dir::Down, GRID[5], &GRID), None);
        assert_eq!(pick(Dir::Right, GRID[1], &GRID), None);
        assert_eq!(pick(Dir::First, GRID[3], &GRID), Some(0));
        assert_eq!(pick(Dir::Last, GRID[0], &GRID), Some(5));
        assert_eq!(pick(Dir::Last, GRID[0], &[]), None);
    }

    #[test]
    fn down_from_a_wide_control_reaches_a_narrow_one_below() {
        let page = [(0.0, 0.0, 400.0, 40.0), (300.0, 60.0, 80.0, 30.0)];
        assert_eq!(pick(Dir::Down, page[0], &page), Some(1));
    }
}
