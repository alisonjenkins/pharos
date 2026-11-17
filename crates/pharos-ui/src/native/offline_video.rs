//! The decisions around a downloaded video, with no I/O and no rendering
//! (spec 048): which copy plays, what the download asks the server for,
//! and the detail the offline player is given in place of the server's.

use pharos_client::api::{Capabilities, Decision, Detail, Item, ItemKind, ItemRef, Track};

use super::kept_video::{KeepVideoRequest, KeptSubtitle, KeptVideo};
use super::tv_page::NO_DIRECT_PLAY;

/// Where the signed-in user's id is remembered, so a cut-off app still knows
/// whose downloads to list. Cleared on sign-out.
pub const LAST_USER_KEY: &str = "pharos.video_user";

/// The play session an offline playback carries: there is no server session,
/// and a constant keeps the player mounted when the connection returns.
pub const OFFLINE_SESSION: &str = "offline";

/// The `method` an offline playback reports in the log and the player.
pub const OFFLINE_METHOD: &str = "offline";

/// The copy to play instead of asking the server: the newest ready one of
/// `item`. A version the viewer picked on purpose (`wanted`) that is not the
/// downloaded one goes to the server.
pub fn ready_copy(videos: &[KeptVideo], item: &str, wanted: Option<&str>) -> Option<KeptVideo> {
    videos
        .iter()
        .find(|v| {
            v.item == item && v.ready() && wanted.map_or(true, |w| w.is_empty() || w == v.version)
        })
        .cloned()
}

/// How near the end a saved position counts as finished, so the film
/// starts over rather than resuming into the credits.
const FINISHED_WITHIN_MS: u64 = 30_000;

/// Where a downloaded copy starts: where the viewer stopped, or the
/// beginning when that was the end.
pub fn resume_ms(position_ms: u64, runtime_ms: u64) -> u64 {
    if runtime_ms > 0 && position_ms.saturating_add(FINISHED_WITHIN_MS) >= runtime_ms {
        0
    } else {
        position_ms
    }
}

/// The capabilities a download negotiates with: no container the server may
/// play directly, so the answer is HLS, and the video capped at `quality_bps`.
pub fn download_caps(mut caps: Capabilities, quality_bps: u64) -> Capabilities {
    caps.containers = vec![NO_DIRECT_PLAY.to_owned()];
    caps.max_bitrate = quality_bps;
    caps
}

/// A film's name, or an episode's "Show · S1E2 · Name".
pub fn video_title(item: &Item) -> String {
    let name = super::views::display_name(item);
    match (item.kind, &item.series_name, item.season, item.episode) {
        (ItemKind::Episode, Some(series), Some(s), Some(e)) => {
            format!("{series} · S{s}E{e} · {name}")
        }
        (ItemKind::Episode, Some(series), _, _) => format!("{series} · {name}"),
        _ => name,
    }
}

/// Text subtitles as the player fetches them: the track's own WebVTT URL.
pub fn kept_subtitles(tracks: &[Track]) -> Vec<KeptSubtitle> {
    tracks
        .iter()
        .filter_map(|t| {
            let url = t.vtt_url.clone()?;
            let label = [&t.title, &t.language]
                .into_iter()
                .find(|s| !s.is_empty())
                .cloned()
                .unwrap_or_else(|| format!("Track {}", t.index));
            Some(KeptSubtitle {
                index: t.index,
                label,
                language: (!t.language.is_empty()).then(|| t.language.clone()),
                url,
            })
        })
        .collect()
}

/// What the store is asked to download, from a negotiation made with
/// [`download_caps`]. The error names the method and URL the server chose
/// when it is not an HLS stream the store can keep.
pub fn keep_request(
    detail: &Detail,
    version: &str,
    user: &str,
    decision: &Decision,
    quality_bps: u64,
) -> Result<KeepVideoRequest, String> {
    if !decision.is_hls() {
        return Err(format!(
            "The server chose {} for this video, which cannot be downloaded (stream {}).",
            decision.method, decision.stream_url
        ));
    }
    let item = &detail.item;
    Ok(KeepVideoRequest {
        item: item.id.to_string(),
        version: version.to_owned(),
        user: user.to_owned(),
        title: video_title(item),
        kind: if item.kind == ItemKind::Episode {
            "episode"
        } else {
            "movie"
        }
        .to_owned(),
        runtime_ms: item.runtime_ms,
        master_url: decision.stream_url.clone(),
        audio: decision.audio,
        subtitles: kept_subtitles(&detail.subtitle_tracks),
        quality_bps,
    })
}

/// The detail the player is given for a downloaded video: only what it
/// shows, with its text subtitles as stored. `None` when the record names
/// no item.
pub fn offline_detail(v: &KeptVideo) -> Option<Detail> {
    let id: ItemRef = v.item.parse().ok()?;
    let kind = if v.kind == "episode" {
        ItemKind::Episode
    } else {
        ItemKind::Movie
    };
    Some(Detail {
        item: Item {
            id,
            kind,
            name: v.title.clone(),
            year: None,
            runtime_ms: v.runtime_ms,
            image: None,
            played: false,
            favorite: false,
            position_ms: v.position_ms,
            series_name: None,
            season: None,
            episode: None,
            parent: None,
            library_kind: None,
            imdb_rating: None,
            critic_rating: None,
            artist: None,
            stereo: None,
            damaged: false,
            new_children: 0,
            audiobook: false,
        },
        overview: String::new(),
        genres: Vec::new(),
        chapters: Vec::new(),
        audio_tracks: Vec::new(),
        subtitle_tracks: v
            .subtitles
            .iter()
            .map(|s| Track {
                index: s.index,
                language: s.language.clone().unwrap_or_default(),
                title: s.label.clone(),
                codec: "webvtt".to_owned(),
                is_default: false,
                is_forced: false,
                is_hearing_impaired: false,
                channels: 0,
                vtt_url: Some(s.url.clone()),
                images_url: None,
                styled_url: None,
            })
            .collect(),
        trickplay: Vec::new(),
        segments: Vec::new(),
        fonts: Vec::new(),
        artists: Vec::new(),
        album: None,
        cast: Vec::new(),
        versions: Vec::new(),
        damage: None,
        book: None,
        read_position: None,
    })
}

/// Who this process believes is signed in. It wins over the stored
/// preference, which is only where a cold start learns it from: a write to
/// the preference can fail, and the proxy must not then serve the files of
/// someone who has signed out.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Identity {
    /// Not known yet: nothing has been signed in or out, and the preference
    /// has not been read (or could not be). Nothing is served from disk.
    Unknown,
    /// Signed out, or no user was ever remembered.
    SignedOut,
    User(String),
}

#[cfg(not(test))]
static IDENTITY: std::sync::Mutex<Identity> = std::sync::Mutex::new(Identity::Unknown);

#[cfg(test)]
thread_local! {
    static IDENTITY: std::cell::RefCell<Identity> = const { std::cell::RefCell::new(Identity::Unknown) };
}

/// Reads or changes the identity under its one lock.
fn with_identity<R>(f: impl FnOnce(&mut Identity) -> R) -> R {
    #[cfg(not(test))]
    {
        f(&mut IDENTITY
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner))
    }
    #[cfg(test)]
    {
        IDENTITY.with(|i| f(&mut i.borrow_mut()))
    }
}

fn identity() -> Identity {
    with_identity(|i| i.clone())
}

/// Per-identity-state facts: whether a cold start has read and failed
/// (so a proxied request does not read the preference and the token again),
/// and whether the refusal for nobody signed in has been logged since the
/// identity last changed.
#[cfg(not(test))]
static FLAGS: [std::sync::atomic::AtomicBool; 2] = [
    std::sync::atomic::AtomicBool::new(false),
    std::sync::atomic::AtomicBool::new(false),
];

#[cfg(test)]
thread_local! {
    static FLAGS: [std::cell::Cell<bool>; 2] = const { [std::cell::Cell::new(false), std::cell::Cell::new(false)] };
}

const COLD_TRIED: usize = 0;
const REFUSAL_LOGGED: usize = 1;

fn flag(i: usize) -> bool {
    #[cfg(not(test))]
    {
        FLAGS[i].load(std::sync::atomic::Ordering::SeqCst)
    }
    #[cfg(test)]
    {
        FLAGS.with(|f| f[i].get())
    }
}

fn set_flag(i: usize, v: bool) {
    #[cfg(not(test))]
    FLAGS[i].store(v, std::sync::atomic::Ordering::SeqCst);
    #[cfg(test)]
    FLAGS.with(|f| f[i].set(v));
}

/// Counts sign-ins and sign-outs, so an answer that was asked for before
/// one of them can tell it is out of date (see [`remember_user_if`]).
/// Changed only under the identity lock.
#[cfg(not(test))]
static GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[cfg(test)]
thread_local! {
    static GENERATION: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

fn generation() -> u64 {
    #[cfg(not(test))]
    {
        GENERATION.load(std::sync::atomic::Ordering::SeqCst)
    }
    #[cfg(test)]
    {
        GENERATION.with(std::cell::Cell::get)
    }
}

fn bump_generation() {
    #[cfg(not(test))]
    GENERATION.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    #[cfg(test)]
    GENERATION.with(|g| g.set(g.get().wrapping_add(1)));
}

/// The identity's generation now: taken before asking who is signed in, and
/// handed to [`remember_user_if`] with the answer.
pub fn identity_generation() -> u64 {
    with_identity(|_| generation())
}

fn set_identity(next: Identity) {
    let _ = set_identity_if(None, next);
}

/// Sets the identity, under the identity lock, only while the generation is
/// still `expected` (any generation with `None`), and starts a new one.
/// Returns the generation found when it is not the expected one.
fn set_identity_if(expected: Option<u64>, next: Identity) -> Result<(), u64> {
    with_identity(|i| {
        let now = generation();
        if expected.is_some_and(|e| e != now) {
            return Err(now);
        }
        // A new identity state is a new chance for a cold start (only a
        // test's restart sets Unknown) and a new refusal to log.
        set_flag(REFUSAL_LOGGED, false);
        if next == Identity::Unknown {
            set_flag(COLD_TRIED, false);
        }
        *i = next;
        bump_generation();
        Ok(())
    })
}

fn read_pref() -> Result<Option<String>, String> {
    #[cfg(test)]
    if let Some(seam) = tests::PREF.with(|p| p.borrow().clone()) {
        return seam.read();
    }
    super::platform::stored_theme(LAST_USER_KEY)
}

fn write_pref(value: &str) -> Result<(), String> {
    #[cfg(test)]
    if let Some(seam) = tests::PREF.with(|p| p.borrow().clone()) {
        return seam.write(value);
    }
    super::platform::store_theme(LAST_USER_KEY, value)
}

/// What the device token says about who is signed in, where a token decides
/// it (the phone and TV apps): `Some(Ok(user))` for the user stored with a
/// readable token, `Some(Ok(None))` for no token or none with a user id, and
/// `None` where the preference decides alone.
fn token_user() -> Option<Result<Option<String>, String>> {
    #[cfg(test)]
    if let Some(seam) = tests::PREF.with(|p| p.borrow().clone()) {
        return Some(seam.token_user());
    }
    super::platform::TOKEN_DECIDES_SIGN_IN.then(super::platform::token_user)
}

/// Who a cold start may take to be signed in: the remembered user, and
/// where a token decides sign-in, only with a readable token that was issued
/// to that same user. No token, an unreadable one, or one with no user id
/// stored is nobody.
fn cold_start_identity() -> Identity {
    let pref = match read_pref() {
        Ok(p) => p.filter(|u| !u.is_empty()),
        Err(e) => {
            super::platform::warn(&format!(
                "pharos: video user verdict=none reason=preference_unreadable error={e}"
            ));
            return Identity::Unknown;
        }
    };
    let Some(token) = token_user() else {
        return pref.map_or(Identity::SignedOut, Identity::User);
    };
    match (token, pref) {
        (Err(e), _) => {
            super::platform::warn(&format!(
                "pharos: video user verdict=none reason=token_unreadable error={e}"
            ));
            Identity::Unknown
        }
        (Ok(None), _) => {
            super::platform::info(
                "pharos: video user verdict=none reason=no_token_or_no_user_with_it",
            );
            Identity::Unknown
        }
        (Ok(Some(t)), Some(p)) if t == p => Identity::User(p),
        (Ok(Some(_)), _) => {
            super::platform::info(
                "pharos: video user verdict=none reason=preference_not_the_tokens_user",
            );
            Identity::Unknown
        }
    }
}

/// The id of the user signed in, and so the one whose downloads may be
/// listed or served. The process's own knowledge wins. While it is unknown
/// (a cold start) it is worked out from the device token and the stored
/// preference (`cold_start_identity`) and installed only if nothing set it
/// meanwhile, so a sign-in or sign-out that lands during the read is never
/// overwritten by what was read before it.
pub fn last_user() -> Option<String> {
    let known = identity();
    if known != Identity::Unknown {
        return match known {
            Identity::User(u) => Some(u),
            _ => None,
        };
    }
    // A cold start that has run and found nobody is not run again for every
    // proxied request: a sign-in or `me` sets the identity, which is what
    // changes the answer.
    if flag(COLD_TRIED) {
        return None;
    }
    let found = cold_start_identity();
    if found == Identity::Unknown {
        set_flag(COLD_TRIED, true);
    }
    let now = with_identity(|i| {
        if *i == Identity::Unknown && found != Identity::Unknown {
            *i = found.clone();
            super::platform::info(&format!(
                "pharos: video user verdict={} reason=cold_start",
                if matches!(found, Identity::User(_)) {
                    "known"
                } else {
                    "none"
                }
            ));
        }
        i.clone()
    });
    match now {
        Identity::User(u) => Some(u),
        _ => None,
    }
}

/// Notes the user who has signed in. Known in this process first; the
/// preference follows, and a failed write is logged, not served around.
pub fn remember_user(user: &str) {
    set_identity(Identity::User(user.to_owned()));
    note_signed_in(user);
}

/// [`remember_user`] for an answer asked for at `asked_at` (from
/// [`identity_generation`]): written only while no sign-in or sign-out has
/// landed since, so a `me` that completes after its session ended cannot
/// bring the previous user back. Returns whether it was written.
pub fn remember_user_if(asked_at: u64, user: &str) -> bool {
    match set_identity_if(Some(asked_at), Identity::User(user.to_owned())) {
        Ok(()) => {
            note_signed_in(user);
            true
        }
        Err(now) => {
            super::platform::info(&format!(
                "pharos: video user verdict=not_remembered reason=identity_changed_since_asked asked_at={asked_at} now={now} user={user}"
            ));
            false
        }
    }
}

fn note_signed_in(user: &str) {
    super::platform::info(&format!(
        "pharos: video user verdict=known reason=signed_in user={user}"
    ));
    note_token_user(user);
    if matches!(read_pref(), Ok(Some(p)) if p == user) {
        return;
    }
    if let Err(e) = write_pref(user) {
        super::platform::warn(&format!(
            "pharos: video user not remembered (served for this run only) error={e}"
        ));
    }
}

/// Keeps the user id with the device token, so a cold start can tell whose
/// token it is.
fn note_token_user(user: &str) {
    #[cfg(test)]
    if let Some(seam) = tests::PREF.with(|p| p.borrow().clone()) {
        if let Err(e) = seam.note_token_user(user) {
            super::platform::warn(&format!(
                "pharos: video user not kept with the token error={e}"
            ));
        }
        return;
    }
    if let Err(e) = super::platform::note_token_user(user) {
        super::platform::warn(&format!(
            "pharos: video user not kept with the token error={e}"
        ));
    }
}

/// Notes a sign-out. Nobody is signed in from this call on, whatever the
/// preference write does.
pub fn forget_user() {
    set_identity(Identity::SignedOut);
    super::platform::info("pharos: video user verdict=none reason=signed_out");
    if let Err(e) = write_pref("") {
        super::platform::warn(&format!(
            "pharos: video user not forgotten on disk (nobody served for this run) error={e}"
        ));
    }
}

/// The stored file the proxy answers `key` with for `user` (the user as of
/// the request): none with nobody signed in, and none of another user's.
pub fn stored_for(
    store: &dyn super::kept_video::VideoStore,
    user: Option<&str>,
    key: &str,
) -> Option<super::kept_video::StoredFile> {
    let Some(user) = user else {
        // Once per identity state, not once per segment.
        if !flag(REFUSAL_LOGGED) {
            set_flag(REFUSAL_LOGGED, true);
            super::platform::info(&format!(
                "pharos: video request verdict=not_served_offline reason=no_signed_in_user key_len={}",
                key.len()
            ));
        }
        return None;
    };
    store.stored_file(user, key)
}

/// What the proxy answers a request for `path_and_query` with from the
/// stored downloads, decided in one place: the signed-in user as of now
/// (read on the caller's thread, so a sign-out after it does not change the
/// request's answer), the request's key, and the store's lookup. `None` with
/// no store, nobody signed in, another user's file, or nothing stored. The
/// Android proxy keeps only the WebView glue around this.
pub fn offline_file_for(
    store: Option<&dyn super::kept_video::VideoStore>,
    path_and_query: &str,
) -> Option<super::kept_video::StoredFile> {
    let user = last_user();
    stored_for(
        store?,
        user.as_deref(),
        &super::kept_video::request_key(path_and_query),
    )
}

/// The headers a stored file is answered with.
pub fn offline_headers(file: &super::kept_video::StoredFile) -> [(&'static str, &'static str); 3] {
    [
        ("access-control-allow-origin", "*"),
        ("content-type", file.content_type),
        ("cache-control", "private, max-age=31536000, immutable"),
    ]
}

/// The ready downloads of the remembered user, newest first; empty with no
/// store or no remembered user.
pub fn ready_for_last_user() -> Vec<KeptVideo> {
    let (Some(store), Some(user)) = (super::ports::video_store(), last_user()) else {
        return Vec::new();
    };
    store
        .videos(&user)
        .into_iter()
        .filter(KeptVideo::ready)
        .collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::super::kept_video::KeptState;
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

    use super::super::kept_video::{KeepVideoRefusal, KeepVideoRequest, StoredFile, VideoStore};

    type ReadHook = Box<dyn Fn()>;

    /// The stored preference, replaceable: its reads and writes can fail.
    #[derive(Clone, Default)]
    pub(super) struct Pref {
        value: Rc<RefCell<Option<String>>>,
        fail_read: Rc<Cell<bool>>,
        fail_write: Rc<Cell<bool>>,
        /// Whether a readable device token exists, and the user stored with
        /// it.
        token_present: Rc<Cell<bool>>,
        token_user: Rc<RefCell<Option<String>>>,
        /// When set, the token and its user are this real file, read and
        /// written by the code the phone runs (`token_file`).
        file: Rc<RefCell<Option<std::path::PathBuf>>>,
        /// Runs once as the preference is read: a sign-in or sign-out that
        /// lands during a cold start's reads.
        on_read: Rc<RefCell<Option<ReadHook>>>,
        /// How many times the preference has been read.
        reads: Rc<Cell<usize>>,
    }

    impl Pref {
        pub(super) fn token_user(&self) -> Result<Option<String>, String> {
            if let Some(path) = self.file.borrow().as_ref() {
                return Ok(super::super::token_file::read(path)?.and_then(|s| s.user));
            }
            Ok(if self.token_present.get() {
                self.token_user.borrow().clone()
            } else {
                None
            })
        }
        pub(super) fn note_token_user(&self, user: &str) -> Result<(), String> {
            if let Some(path) = self.file.borrow().as_ref() {
                return super::super::token_file::store_user(path, user);
            }
            if self.token_present.get() {
                *self.token_user.borrow_mut() = Some(user.to_owned());
            }
            Ok(())
        }
        /// What the sign-out does to the token: it and its user are gone.
        pub(super) fn clear_token(&self) {
            self.token_present.set(false);
            *self.token_user.borrow_mut() = None;
        }
        pub(super) fn read(&self) -> Result<Option<String>, String> {
            self.reads.set(self.reads.get() + 1);
            let read = if self.fail_read.get() {
                Err("unreadable".to_owned())
            } else {
                Ok(self.value.borrow().clone())
            };
            // What was read is already in hand when the change lands.
            let hook = self.on_read.borrow_mut().take();
            if let Some(hook) = hook {
                hook();
            }
            read
        }
        pub(super) fn write(&self, v: &str) -> Result<(), String> {
            if self.fail_write.get() {
                return Err("read-only".to_owned());
            }
            *self.value.borrow_mut() = Some(v.to_owned());
            Ok(())
        }
    }

    thread_local! {
        pub(super) static PREF: RefCell<Option<Pref>> = const { RefCell::new(None) };
    }

    fn pref() -> Pref {
        let p = Pref::default();
        p.token_present.set(true);
        PREF.with(|c| *c.borrow_mut() = Some(p.clone()));
        set_identity(Identity::Unknown);
        p
    }

    /// A store whose `A` has a file for the key and nobody else does.
    struct Files;

    impl VideoStore for Files {
        fn videos(&self, _: &str) -> Vec<KeptVideo> {
            Vec::new()
        }
        fn start(&self, _: KeepVideoRequest) -> Result<(), KeepVideoRefusal> {
            Ok(())
        }
        fn remove(&self, _: &str, _: &str) {}
        fn save_position(&self, _: &str, _: &str, _: u64) {}
        fn cap_bytes(&self) -> u64 {
            0
        }
        fn used_bytes(&self, _: &str) -> u64 {
            0
        }
        fn free_bytes(&self) -> u64 {
            0
        }
        fn stored_file(&self, user: &str, key: &str) -> Option<StoredFile> {
            (key == "/k" && user == "A").then(|| StoredFile {
                path: "/x".into(),
                len: 1,
                content_type: "video/mp2t",
            })
        }
    }

    fn serves(user: Option<String>) -> bool {
        stored_for(&Files, user.as_deref(), "/k").is_some()
    }

    /// A sign-out whose preference write fails still serves nothing.
    #[test]
    fn a_sign_out_whose_preference_write_fails_serves_nothing() {
        let p = pref();
        remember_user("A");
        assert!(serves(last_user()));
        p.fail_write.set(true);
        forget_user();
        assert_eq!(p.value.borrow().as_deref(), Some("A"), "the write failed");
        assert_eq!(last_user(), None, "the stored user outlived the sign-out");
        assert!(!serves(last_user()));
    }

    /// A switch from A to B: nothing is served between the sign-out and B's
    /// `me`, and then only B's.
    #[test]
    fn a_switch_serves_only_the_new_user_and_nothing_before_their_me() {
        let p = pref();
        remember_user("A");
        assert!(serves(last_user()), "A's own file");
        // The preference is not writable for the whole switch.
        p.fail_write.set(true);
        forget_user();
        assert!(
            !serves(last_user()),
            "served after the sign-out, before B's me"
        );
        remember_user("B");
        assert!(!serves(last_user()), "B was given A's file");
        assert_eq!(last_user().as_deref(), Some("B"));
    }

    /// A `me` asked before its session ended and answered after: the
    /// session end's sign-out stands, and the answer is not remembered.
    #[test]
    fn a_me_answered_after_its_session_ended_does_not_bring_the_user_back() {
        let p = pref();
        remember_user("A");
        let asked_at = identity_generation();
        // The session ends while `me` is in flight.
        forget_user();
        assert!(
            !remember_user_if(asked_at, "A"),
            "the stale answer was written"
        );
        assert_eq!(last_user(), None, "the user came back after the sign-out");
        assert!(!serves(last_user()));
        assert_eq!(
            p.value.borrow().as_deref(),
            Some(""),
            "the preference was rewritten"
        );
        // A `me` asked after the sign-out (the next session's) is written.
        let asked_at = identity_generation();
        assert!(remember_user_if(asked_at, "A"));
        assert_eq!(last_user().as_deref(), Some("A"));
    }

    /// A `remember_user` whose write fails still serves that user here.
    #[test]
    fn a_failed_remember_still_serves_the_right_user_in_this_process() {
        let p = pref();
        p.fail_write.set(true);
        remember_user("A");
        assert_eq!(last_user().as_deref(), Some("A"));
        assert!(serves(last_user()));
    }

    /// A real offline cold start: the token is there and was issued to the
    /// remembered user, so that user's files are served; a preference that
    /// cannot be read serves nobody.
    #[test]
    fn a_cold_start_with_a_token_for_the_remembered_user_serves_them() {
        let p = pref();
        *p.value.borrow_mut() = Some("A".to_owned());
        *p.token_user.borrow_mut() = Some("A".to_owned());
        p.fail_read.set(true);
        assert_eq!(last_user(), None);
        assert!(!serves(last_user()));
        // A cold start that found nobody is not read again by every request
        // (`a_failed_cold_start_is_not_read_again_for_every_request`); the
        // next start reads afresh.
        p.fail_read.set(false);
        set_identity(Identity::Unknown);
        assert_eq!(last_user().as_deref(), Some("A"));
        assert!(serves(last_user()));
    }

    /// The preference alone is not a sign-in: a failed sign-out write leaves
    /// the old user in it, and the next start (the token gone) serves nobody.
    #[test]
    fn a_failed_sign_out_write_followed_by_a_restart_serves_nothing() {
        let p = pref();
        remember_user("A");
        assert!(serves(last_user()));
        p.fail_write.set(true);
        p.clear_token();
        forget_user();
        assert_eq!(p.value.borrow().as_deref(), Some("A"), "the write failed");
        // The restart: the process knows nothing, the preference says A.
        set_identity(Identity::Unknown);
        assert_eq!(last_user(), None, "the stale preference signed A back in");
        assert!(!serves(last_user()));
    }

    /// A sign-out whose token delete fails, then another user's sign-in,
    /// then a restart before that user's `me` has answered: the new token
    /// has no user of its own yet, so nothing is served, and above all not
    /// the first user's downloads. Through the real token file the phone
    /// writes, and the lookup the proxy answers from.
    #[test]
    fn a_failed_sign_out_then_another_sign_in_never_serves_the_first_user() {
        use super::super::token_file;
        let d = std::env::temp_dir().join(format!("pharos-ov-seq-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let path = d.join("device-token");
        let p = pref();
        *p.file.borrow_mut() = Some(path.clone());

        // A signs in: the token, then `me` says whose it is.
        token_file::store(&path, "tokA").unwrap();
        remember_user("A");
        set_identity(Identity::Unknown);
        assert!(serves(last_user()), "A's own restart serves A");

        // A signs out; the token's delete fails, so the file stays as it is.
        forget_user();

        // B signs in: the new token replaces the file whole.
        token_file::store(&path, "tokB").unwrap();

        // The restart, before B's `me` has answered. The preference still
        // names A.
        *p.value.borrow_mut() = Some("A".to_owned());
        set_identity(Identity::Unknown);
        assert_eq!(last_user(), None, "B's token vouched for A");
        assert!(!serves(last_user()));

        // `me` answers: B is known, and pairs with B's token for good.
        remember_user("B");
        set_identity(Identity::Unknown);
        assert_eq!(last_user().as_deref(), Some("B"));
        assert!(!serves(last_user()), "B has nothing downloaded");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A store holding A's file on disk, for the answer the proxy gives.
    struct OnDisk(std::path::PathBuf);

    impl super::super::kept_video::VideoStore for OnDisk {
        fn start(
            &self,
            _: super::super::kept_video::KeepVideoRequest,
        ) -> Result<(), super::super::kept_video::KeepVideoRefusal> {
            Ok(())
        }
        fn remove(&self, _: &str, _: &str) {}
        fn save_position(&self, _: &str, _: &str, _: u64) {}
        fn videos(&self, _: &str) -> Vec<KeptVideo> {
            Vec::new()
        }
        fn cap_bytes(&self) -> u64 {
            0
        }
        fn used_bytes(&self, _: &str) -> u64 {
            0
        }
        fn free_bytes(&self) -> u64 {
            0
        }
        fn stored_file(&self, user: &str, key: &str) -> Option<StoredFile> {
            (user == "A" && key.starts_with("/videos/1/seg.m4s")).then(|| StoredFile {
                path: self.0.clone(),
                len: 5,
                content_type: "video/mp4",
            })
        }
    }

    /// The whole answer the proxy gives, through the function it calls: A
    /// keeps a download, signs out with the token's delete failing, B signs
    /// in, the app restarts: nothing of A's is answered. A signing in again
    /// gets A's bytes. Real token file; the store holds A's file on disk.
    #[test]
    fn the_proxys_answer_after_a_failed_sign_out_and_a_switch_never_gives_a_the_bytes_to_b() {
        use super::super::token_file;
        let d = std::env::temp_dir().join(format!("pharos-ov-answer-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let file = d.join("a.m4s");
        std::fs::write(&file, b"A-own").unwrap();
        let store = OnDisk(file.clone());
        let path = d.join("device-token");
        let p = pref();
        *p.file.borrow_mut() = Some(path.clone());
        let ask = || offline_file_for(Some(&store), "/videos/1/seg.m4s?PlaySessionId=p");
        let bytes = |f: StoredFile| std::fs::read(f.path).unwrap();

        token_file::store(&path, "tokA").unwrap();
        remember_user("A");
        set_identity(Identity::Unknown);
        assert_eq!(bytes(ask().unwrap()), b"A-own", "A's own restart");

        forget_user();
        assert!(ask().is_none(), "answered after the sign-out");
        token_file::store(&path, "tokB").unwrap();
        // The restart, before B's `me`; the preference still names A.
        *p.value.borrow_mut() = Some("A".to_owned());
        set_identity(Identity::Unknown);
        assert!(ask().is_none(), "A's bytes answered under B's token");
        remember_user("B");
        set_identity(Identity::Unknown);
        assert!(ask().is_none(), "A's bytes answered to B");

        // A signs in again.
        token_file::store(&path, "tokA2").unwrap();
        remember_user("A");
        set_identity(Identity::Unknown);
        let answer = ask().expect("A gets A's file");
        assert_eq!(offline_headers(&answer)[1], ("content-type", "video/mp4"));
        assert_eq!(bytes(answer), b"A-own");
        assert!(offline_file_for(None, "/videos/1/seg.m4s").is_none());
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A cold start that found nobody is not run again for each proxied
    /// request, and a later sign-in still sets the user.
    #[test]
    fn a_failed_cold_start_is_not_read_again_for_every_request() {
        let p = pref();
        p.fail_read.set(true);
        assert_eq!(last_user(), None);
        let after_first = p.reads.get();
        assert_eq!(after_first, 1, "the first request reads once");
        for _ in 0..5 {
            assert_eq!(last_user(), None);
        }
        assert_eq!(p.reads.get(), after_first, "later requests read again");
        remember_user("A");
        assert_eq!(last_user().as_deref(), Some("A"), "a sign-in is ignored");
    }

    /// A token with no user id stored with it, or another user's, does not
    /// vouch for the remembered user.
    #[test]
    fn a_token_that_is_not_the_remembered_users_serves_nobody() {
        let p = pref();
        *p.value.borrow_mut() = Some("A".to_owned());
        assert_eq!(last_user(), None, "a token with no user id vouched for A");
        *p.token_user.borrow_mut() = Some("B".to_owned());
        // The first read found no token user and the cold start is not run
        // again until the identity is unknown: reset it, so the mismatch
        // branch is the one that runs.
        set_identity(Identity::Unknown);
        assert_eq!(last_user(), None, "B's token vouched for A");
        assert!(!serves(last_user()));
    }

    /// A sign-in or sign-out that lands while a cold start is reading is
    /// not overwritten by what the read found.
    #[test]
    fn a_change_during_the_cold_start_reads_is_not_overwritten() {
        let p = pref();
        *p.value.borrow_mut() = Some("A".to_owned());
        *p.token_user.borrow_mut() = Some("A".to_owned());
        *p.on_read.borrow_mut() = Some(Box::new(forget_user));
        assert_eq!(last_user(), None, "the cold start brought A back");
        assert!(!serves(last_user()));

        let p = pref();
        *p.value.borrow_mut() = Some("A".to_owned());
        *p.token_user.borrow_mut() = Some("A".to_owned());
        *p.on_read.borrow_mut() = Some(Box::new(|| remember_user("B")));
        assert_eq!(last_user().as_deref(), Some("B"), "B was overwritten");
        assert!(!serves(last_user()), "A's file served to B");
    }

    pub(crate) fn video(item: &str, version: &str, state: KeptState) -> KeptVideo {
        KeptVideo {
            item: item.into(),
            version: version.into(),
            user: "u".into(),
            title: "Invented Film".into(),
            kind: "movie".into(),
            runtime_ms: 5_400_000,
            master_url: format!(
                "/videos/{item}/master.m3u8?PlaySessionId=abc&VideoBitrate=2000000"
            ),
            audio: Some(1),
            subtitles: vec![KeptSubtitle {
                index: 3,
                label: "English".into(),
                language: Some("eng".into()),
                url: format!("/videos/{item}/subtitles/3/stream.vtt"),
            }],
            files: vec![],
            quality_bps: 2_000_000,
            estimated_bytes: 1_000_000,
            state,
            position_ms: 61_000,
            saved_at: "2026-11-16T09:00:00Z".into(),
            estimate_from_adoption: false,
        }
    }

    /// Disarm check: making `ready_copy` ignore `ready()` fails the second
    /// assertion.
    #[test]
    fn only_a_ready_copy_of_the_item_is_played() {
        let downloading = KeptState::Downloading {
            done: 1,
            total: 9,
            bytes: 10,
        };
        let videos = vec![
            video("7", "", downloading),
            video("8", "", KeptState::Ready),
            video("9", "", KeptState::Failed { reason: "x".into() }),
        ];
        assert_eq!(
            ready_copy(&videos, "8", None).map(|v| v.item),
            Some("8".into())
        );
        assert_eq!(ready_copy(&videos, "7", None), None);
        assert_eq!(ready_copy(&videos, "9", None), None);
        assert_eq!(ready_copy(&videos, "10", None), None);
    }

    #[test]
    fn a_version_picked_on_purpose_goes_to_the_server() {
        let videos = vec![video("8", "v2", KeptState::Ready)];
        assert!(ready_copy(&videos, "8", Some("v2")).is_some());
        assert!(ready_copy(&videos, "8", Some("")).is_some());
        assert!(ready_copy(&videos, "8", Some("v3")).is_none());
        assert!(ready_copy(&videos, "8", None).is_some());
    }

    #[test]
    fn the_offline_detail_carries_the_subtitles_and_the_resume_point() {
        let d = offline_detail(&video("8", "", KeptState::Ready)).unwrap();
        assert_eq!(d.item.id, ItemRef::Real(8));
        assert_eq!(d.item.position_ms, 61_000);
        assert_eq!(d.subtitle_tracks.len(), 1);
        assert_eq!(
            d.subtitle_tracks[0].vtt_url.as_deref(),
            Some("/videos/8/subtitles/3/stream.vtt")
        );
        assert!(offline_detail(&video("not an id", "", KeptState::Ready)).is_none());
    }

    #[test]
    fn a_finished_copy_starts_over() {
        assert_eq!(resume_ms(61_000, 5_400_000), 61_000);
        assert_eq!(resume_ms(5_380_000, 5_400_000), 0);
        assert_eq!(resume_ms(61_000, 0), 61_000);
    }

    #[test]
    fn a_download_asks_for_hls_at_its_quality() {
        let caps = download_caps(
            Capabilities {
                max_bitrate: 0,
                ..Capabilities::default()
            },
            2_000_000,
        );
        assert_eq!(caps.containers, [NO_DIRECT_PLAY]);
        assert_eq!(caps.max_bitrate, 2_000_000);
    }

    #[test]
    fn a_direct_stream_is_refused_naming_the_method() {
        let detail = offline_detail(&video("8", "", KeptState::Ready)).unwrap();
        let direct = Decision {
            method: "direct_play".into(),
            direct_play_block: String::new(),
            downgrade: String::new(),
            stream_url: "/videos/8/stream?static=true&tag=t".into(),
            play_session: "t".into(),
            audio: Some(1),
            subtitle: None,
        };
        let err = keep_request(&detail, "", "u", &direct, 2_000_000).unwrap_err();
        assert!(err.contains("direct_play"), "{err}");
        let hls = Decision {
            method: "transcode".into(),
            stream_url: "/videos/8/master.m3u8?PlaySessionId=t".into(),
            ..direct
        };
        let req = keep_request(&detail, "", "u", &hls, 2_000_000).unwrap();
        assert_eq!(req.master_url, "/videos/8/master.m3u8?PlaySessionId=t");
        assert_eq!(req.audio, Some(1));
        assert_eq!(req.kind, "movie");
    }

    #[test]
    fn only_text_subtitles_are_kept() {
        let text = Track {
            index: 2,
            language: "eng".into(),
            title: String::new(),
            codec: "subrip".into(),
            is_default: false,
            is_forced: false,
            is_hearing_impaired: false,
            channels: 0,
            vtt_url: Some("/videos/8/subtitles/2/stream.vtt".into()),
            images_url: None,
            styled_url: None,
        };
        let image = Track {
            index: 4,
            vtt_url: None,
            images_url: Some("/i.json".into()),
            ..text.clone()
        };
        let kept = kept_subtitles(&[text, image]);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].label, "eng");
    }

    #[test]
    fn an_episode_is_named_with_its_show_and_number() {
        let mut item = offline_detail(&video("8", "", KeptState::Ready))
            .unwrap()
            .item;
        item.kind = ItemKind::Episode;
        item.series_name = Some("Invented Show".into());
        item.season = Some(1);
        item.episode = Some(2);
        assert_eq!(video_title(&item), "Invented Show · S1E2 · Invented Film");
    }
}
