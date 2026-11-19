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
    /// A new session replaced the one this user was learned on, and its
    /// credentials are not known to be theirs: nothing is served from disk
    /// until the new session's user is established (its `me` answers, or
    /// its token is paired with this same user). Whose downloads to park
    /// at a session end is still this user.
    Fenced(String),
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

/// The device token of the session published last ([`session_replaced`]),
/// once there is one. Written under the identity lock. A cold start read
/// after it is bound to it: only that token's paired user may be installed.
#[cfg(not(test))]
static SESSION_TOKEN: std::sync::Mutex<Option<TokenSeen>> = std::sync::Mutex::new(None);

#[cfg(test)]
thread_local! {
    static SESSION_TOKEN: std::cell::RefCell<Option<TokenSeen>> = const { std::cell::RefCell::new(None) };
}

fn with_session_token<R>(f: impl FnOnce(&mut Option<TokenSeen>) -> R) -> R {
    #[cfg(not(test))]
    {
        f(&mut SESSION_TOKEN
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner))
    }
    #[cfg(test)]
    {
        SESSION_TOKEN.with(|t| f(&mut t.borrow_mut()))
    }
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

/// A new session replaces the one before it (a reconnect, a sign-in): that
/// session has ended, so a `me` answer asked on it is out of date and must
/// not install its user. The identity stays as it is; only answers asked
/// before this call are refused ([`MeAsk::answered`]). Call it before the
/// new session is published, so an ask taken on the new one sees the new
/// generation.
pub fn session_replaced<A>(session: &Session<A>) {
    // Who the new session's token is paired with, from one read of the
    // token file, and only while it is still that token: pairing is written
    // under the file's compare-and-set, so the user named belongs to it.
    let paired = paired_user(&session.token);
    let (now, verdict, reason, user) = with_identity(|i| {
        bump_generation();
        with_session_token(|t| *t = Some(session.token.clone()));
        let (verdict, reason, user) = match i.clone() {
            Identity::User(u) | Identity::Fenced(u) if paired.as_deref() == Some(u.as_str()) => {
                *i = Identity::User(u.clone());
                ("kept", "token_paired_with_same_user", u)
            }
            Identity::User(u) | Identity::Fenced(u) => {
                *i = Identity::Fenced(u.clone());
                let why = if paired.is_some() {
                    "token_paired_with_another_user"
                } else {
                    "token_not_paired"
                };
                ("fenced", why, u)
            }
            _ => ("unchanged", "no_user_known", String::new()),
        };
        (generation(), verdict, reason, user)
    });
    super::platform::info(&format!(
        "pharos: video user generation verdict=advanced reason=session_replaced generation={now}"
    ));
    note_fence(format!(
        "pharos: video user fence verdict={verdict} reason={reason} user={user} generation={now}"
    ));
}

/// Logs a fence verdict (and keeps it for a test to read).
fn note_fence(line: String) {
    #[cfg(test)]
    tests::LOGGED.with(|l| l.borrow_mut().push(line.clone()));
    super::platform::info(&line);
}

/// The user stored with the device token, only while the token on file is
/// the one `seen`; `None` where no token decides sign-in, with no token,
/// an unpaired one, or one replaced since.
fn paired_user(seen: &TokenSeen) -> Option<String> {
    let seen = seen.0.as_deref()?;
    #[cfg(test)]
    if let Some(seam) = tests::PREF.with(|p| p.borrow().clone()) {
        return (seam.token_seen().as_deref() == Some(seen))
            .then(|| seam.token_user().ok().flatten())
            .flatten();
    }
    if !super::platform::TOKEN_DECIDES_SIGN_IN {
        return None;
    }
    match super::platform::token_user_if(seen) {
        Ok(user) => user,
        Err(e) => {
            super::platform::warn(&format!(
                "pharos: video user fence pairing_unreadable error={e}"
            ));
            None
        }
    }
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
        if let (Identity::Fenced(was), Identity::User(now)) = (&*i, &next) {
            note_fence(format!(
                "pharos: video user fence verdict=lifted reason=user_established was={was} user={now}"
            ));
        }
        // A new identity state is a new chance for a cold start (only a
        // test's restart sets Unknown) and a new refusal to log.
        set_flag(REFUSAL_LOGGED, false);
        if next == Identity::Unknown {
            set_flag(COLD_TRIED, false);
            with_session_token(|t| *t = None);
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
    // Once a session is published, its token decides: the remembered user
    // is installed only if that token is still on file and paired with
    // them, never because the file names them (a token stored since).
    if let Some(seen) = with_session_token(|t| t.clone()).filter(|t| t.0.is_some()) {
        let paired = paired_user(&seen);
        return match (paired, pref) {
            (Some(t), Some(p)) if t == p => Identity::User(p),
            _ => {
                super::platform::info(
                    "pharos: video user verdict=none reason=session_token_not_paired_with_preference",
                );
                Identity::Unknown
            }
        };
    }
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
    // The generation is taken before the reads, while the identity is
    // Unknown: what they found is installed only through the one
    // compare-and-set every install uses, so a sign-in, a sign-out or a
    // session published during them (each advances the generation) wins.
    let asked_at = identity_generation();
    let found = cold_start_identity();
    #[cfg(test)]
    tests::AFTER_COLD_READ.with(|h| {
        if let Some(f) = h.borrow_mut().take() {
            f();
        }
    });
    if found == Identity::Unknown {
        set_flag(COLD_TRIED, true);
    } else {
        let verdict = if matches!(found, Identity::User(_)) {
            "known"
        } else {
            "none"
        };
        match set_identity_if(Some(asked_at), found) {
            Ok(()) => super::platform::info(&format!(
                "pharos: video user verdict={verdict} reason=cold_start"
            )),
            Err(now) => super::platform::info(&format!(
                "pharos: video user verdict=not_installed reason=identity_changed_during_cold_start asked_at={asked_at} now={now}"
            )),
        }
    }
    match identity() {
        Identity::User(u) => Some(u),
        _ => None,
    }
}

/// Why a resume did not give a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResumeError<E> {
    /// The server knows no session for the token: sign in again.
    Refused,
    /// Anything else, kept for the caller to report.
    Failed(E),
}

/// The platform side of resuming a session: the device token on file and
/// the server's resume, so the decision in [`resume_session`] runs the same
/// on the phone and in a host test with a fake server.
pub trait Resumer {
    type Api;
    type Error;
    /// The device token on file, read once for this resume.
    fn load_token(&self) -> Option<String>;
    /// Resumes the session `token` names.
    fn resume<'a>(
        &'a self,
        token: &'a str,
    ) -> futures_util::future::LocalBoxFuture<'a, Result<Self::Api, ResumeError<Self::Error>>>;
    /// Drops the token on file while it is still `refused` (the server
    /// refused it), as one step under the file's lock. False when a sign-in
    /// replaced it since: the replacement is kept.
    fn forget_token(&self, refused: &str) -> bool;
}

/// What [`resume_session`] gave.
pub enum Resumed<A, E> {
    /// A session, carrying the token it was resumed with: the one read for
    /// it.
    Opened(Session<A>),
    /// No token on file: nothing was sent.
    NoToken,
    /// The server refused the token, which is dropped.
    Refused,
    /// Each token read was replaced by a sign-in before its refusal came
    /// back, [`RESUME_TRIES`] times: the latest is kept, for the next
    /// attempt.
    Superseded,
    Failed(E),
}

/// How many tokens one resume tries when each refused one was replaced by
/// a sign-in in the meantime.
pub const RESUME_TRIES: u32 = 3;

/// Reads the token once and resumes with exactly that token: the session
/// that comes back answers for it, whatever the file holds afterwards.
/// A refused token is dropped only while it is still the one on file; one
/// a sign-in stored meanwhile is kept and resumed with in turn.
pub async fn resume_session<R: Resumer>(r: &R) -> Resumed<R::Api, R::Error> {
    for attempt in 1..=RESUME_TRIES {
        let Some(token) = r.load_token() else {
            return Resumed::NoToken;
        };
        match r.resume(&token).await {
            Ok(api) => return Resumed::Opened(Session::opened(api, Some(token))),
            Err(ResumeError::Refused) => {
                if r.forget_token(&token) {
                    return Resumed::Refused;
                }
                super::platform::info(&format!(
                    "pharos: resume verdict=superseded reason=token_changed_since_read attempt={attempt} of={RESUME_TRIES}"
                ));
            }
            Err(ResumeError::Failed(e)) => return Resumed::Failed(e),
        }
    }
    Resumed::Superseded
}

/// Notes the user who has signed in, paired with the test seam's token.
/// Known in this process first; the preference follows, and a failed write
/// is logged, not served around. The app learns its user only through
/// [`MeAsk::answered`], with the session's token.
#[cfg(test)]
pub(crate) fn remember_user(user: &str) {
    set_identity(Identity::User(user.to_owned()));
    note_signed_in(user, &tests::seam_token());
}

/// The device token a request was built with, taken when it is built: its
/// answer pairs its user with this token only, never with one stored since.
/// Its `Debug` never shows the value.
#[derive(Clone, PartialEq, Eq)]
pub struct TokenSeen(Option<String>);

impl std::fmt::Debug for TokenSeen {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(if self.0.is_some() {
            "TokenSeen(present)"
        } else {
            "TokenSeen(none)"
        })
    }
}

/// A signed-in session as `open` gave it: its API and the device token it
/// was resumed with, one value. Made only where a session is opened
/// ([`resume_session`], or a platform with no device token); nothing sets
/// or clears the token after, so a re-open is a new value and a request
/// built from the old one still carries the old token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session<A> {
    api: A,
    token: TokenSeen,
}

impl<A> Session<A> {
    /// A session resumed with `token` (none where the platform has no
    /// device token).
    pub(crate) fn opened(api: A, token: Option<String>) -> Self {
        Self {
            api,
            token: TokenSeen(token),
        }
    }

    pub fn api(&self) -> &A {
        &self.api
    }
}

/// A `me` request that may be sent: the API it goes to and the identity
/// generation it was asked at, both taken at one point, before the request
/// is built, so a session that ends after this point is seen when the
/// answer comes back ([`MeAsk::answered`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MeAsk<A> {
    pub api: A,
    pub asked_at: u64,
    /// The token the session was resumed with: the answer's user is paired
    /// with this token only.
    pub token: TokenSeen,
}

/// Whether to ask who is signed in: only with a session and a ready
/// machine (`Ready` at this point). The generation is taken here, at the
/// same point as the session, never later in the request's future; the API
/// and the token come from that one session value. Logs a refusal with its
/// reason.
pub fn me_request<A>(ready: bool, session: Option<Session<A>>) -> Option<MeAsk<A>> {
    let session = session?;
    if !ready {
        super::platform::info("pharos: video user me verdict=not_sent reason=session_not_ready");
        return None;
    }
    Some(MeAsk {
        api: session.api,
        asked_at: identity_generation(),
        token: session.token,
    })
}

impl<A> MeAsk<A> {
    /// The answer to this request names `user`: remembered only while no
    /// sign-in or sign-out has landed since the request was taken.
    pub fn answered(&self, user: &str) -> bool {
        remember_user_seen(self.asked_at, user, &self.token)
    }
}

/// [`remember_user`] for an answer asked for at `asked_at` (from
/// [`identity_generation`]): written only while no sign-in or sign-out has
/// landed since, so a `me` that completes after its session ended cannot
/// bring the previous user back. Returns whether it was written.
#[cfg(test)]
pub(crate) fn remember_user_if(asked_at: u64, user: &str) -> bool {
    remember_user_seen(asked_at, user, &tests::seam_token())
}

/// [`remember_user_if`] for an answer to a request built with `seen`.
fn remember_user_seen(asked_at: u64, user: &str, seen: &TokenSeen) -> bool {
    match set_identity_if(Some(asked_at), Identity::User(user.to_owned())) {
        Ok(()) => {
            note_signed_in(user, seen);
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

/// The store of downloaded videos, when there is one.
fn store() -> Option<std::sync::Arc<dyn super::kept_video::VideoStore>> {
    #[cfg(test)]
    if let Some(store) = tests::STORE.with(|s| s.borrow().clone()) {
        return Some(store);
    }
    super::ports::video_store()
}

/// Runs `f` on the store of downloaded videos, when there is one.
fn with_store(f: impl FnOnce(&dyn super::kept_video::VideoStore)) {
    if let Some(store) = store() {
        f(&*store);
    }
}

/// A sign-in whose resume found no store yet: the user and the token they
/// were paired with, replayed when the store is published
/// ([`store_published`]). Decided and taken under this one lock, and the
/// store is published before its replay takes it, so a sign-in either sees
/// the store or leaves its resume here for the replay: never neither.
#[cfg(not(test))]
static RESUME_OWED: std::sync::Mutex<Option<(String, TokenSeen)>> = std::sync::Mutex::new(None);

#[cfg(test)]
thread_local! {
    static RESUME_OWED: std::cell::RefCell<Option<(String, TokenSeen)>> = const { std::cell::RefCell::new(None) };
}

fn with_owed<R>(f: impl FnOnce(&mut Option<(String, TokenSeen)>) -> R) -> R {
    #[cfg(not(test))]
    {
        f(&mut RESUME_OWED
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner))
    }
    #[cfg(test)]
    {
        RESUME_OWED.with(|o| f(&mut o.borrow_mut()))
    }
}

/// Resumes `user`'s downloads on the store now, or, with no store published
/// yet, owes the resume to its publication.
fn resume_or_owe(user: &str, seen: &TokenSeen) {
    let now = with_owed(|owed| {
        let store = store();
        if store.is_none() {
            *owed = Some((user.to_owned(), seen.clone()));
        }
        store
    });
    match now {
        Some(store) => {
            super::platform::info(&format!(
                "pharos: video keep resume verdict=resumed reason=token_paired user={user}"
            ));
            store.resume_user(user);
        }
        None => super::platform::info(&format!(
            "pharos: video keep resume verdict=owed reason=store_not_published user={user}"
        )),
    }
}

/// Called once the store of downloaded videos is published: replays a
/// sign-in that landed before it, so a download recovery parked is resumed,
/// only while that user is still the one signed in and the token on file is
/// still the one paired with them (fail closed otherwise).
pub fn store_published() {
    let Some((user, seen)) = with_owed(Option::take) else {
        return;
    };
    let reason = if identity() != Identity::User(user.clone()) {
        Some("identity_changed")
    } else if paired_user(&seen).as_deref() != Some(user.as_str()) {
        Some("token_not_paired")
    } else {
        None
    };
    match (reason, store()) {
        (None, Some(store)) => {
            super::platform::info(&format!(
                "pharos: video keep resume verdict=resumed reason=store_published_after_sign_in user={user}"
            ));
            store.resume_user(&user);
        }
        (why, _) => super::platform::info(&format!(
            "pharos: video keep resume verdict=not_resumed reason={} user={user}",
            why.unwrap_or("store_not_published")
        )),
    }
}

fn note_signed_in(user: &str, seen: &TokenSeen) {
    super::platform::info(&format!(
        "pharos: video user verdict=known reason=signed_in user={user}"
    ));
    // Their downloads parked at a session end, or by another user's token,
    // run again, but only once the token is paired with them: a run resumed
    // before would read an unpaired token, send nothing and park until the
    // next sign-in. A pairing that fails resumes nothing (fail closed).
    match note_token_user(user, seen) {
        Ok(()) => resume_or_owe(user, seen),
        Err(e) => super::platform::warn(&format!(
            "pharos: video keep resume verdict=not_resumed reason=token_not_paired user={user} error={e}"
        )),
    }
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
/// token it is: only with the token `seen` when the user was learned. A
/// token stored since stays unpaired (`verdict=refused
/// reason=token_changed`), so a check that needs its user fails closed.
/// Err (logged) when it could not.
fn note_token_user(user: &str, seen: &TokenSeen) -> Result<(), String> {
    #[cfg(test)]
    let kept = match tests::PREF.with(|p| p.borrow().clone()) {
        Some(seam) => seam.note_token_user(user, seen.0.as_deref()),
        None => super::platform::note_token_user(user, seen.0.as_deref()),
    };
    #[cfg(not(test))]
    let kept = super::platform::note_token_user(user, seen.0.as_deref());
    if let Err(e) = &kept {
        super::platform::warn(&format!(
            "pharos: video user pairing verdict=refused reason={e} user={user}"
        ));
    }
    kept
}

/// Notes a sign-out. Nobody is signed in from this call on, whatever the
/// preference write does.
pub fn forget_user() {
    // Whose session it was, before it is forgotten: their downloads stop
    // before another user's token can be used (all of them when it is not
    // known). Bound to their owner's token as well; this is the hygiene.
    let ended = match identity() {
        Identity::User(u) | Identity::Fenced(u) => Some(u),
        _ => None,
    };
    with_store(|store| store.park_runs(ended.as_deref()));
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

/// The headers a stored file is answered with. Never cached: the WebView
/// keys its cache by path and query, which do not name the user, so a
/// cached answer would go to whoever asks next, across a sign-out or an
/// account switch. The proxy reads the file from local disk, so nothing is
/// lost.
pub fn offline_headers(file: &super::kept_video::StoredFile) -> [(&'static str, &'static str); 3] {
    [
        ("access-control-allow-origin", "*"),
        ("content-type", file.content_type),
        ("cache-control", "no-store"),
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

    /// A folder only this test made, removed when the guard drops, a
    /// failed assertion's unwinding included. Its name is new each time,
    /// and it is made with `create_dir`, so a guard never takes over (or
    /// later removes) a folder something else made.
    struct TmpDir(std::path::PathBuf);

    impl TmpDir {
        fn new(tag: &str) -> Self {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            loop {
                let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let nanos = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.subsec_nanos());
                let path = std::env::temp_dir().join(format!(
                    "pharos-ov-{tag}-{}-{n}-{nanos}",
                    std::process::id()
                ));
                match std::fs::create_dir(&path) {
                    Ok(()) => return Self(path),
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(e) => panic!("no test folder: {e}"),
                }
            }
        }

        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl Drop for TmpDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A test that fails part way leaves no folder behind.
    #[test]
    fn a_test_folder_goes_when_its_test_panics() {
        let made = std::sync::Mutex::new(None);
        let outcome = std::panic::catch_unwind(|| {
            let d = TmpDir::new("guard");
            std::fs::write(d.path().join("f"), b"x").unwrap();
            *made.lock().unwrap() = Some(d.path().to_path_buf());
            panic!("an assertion failed");
        });
        assert!(outcome.is_err());
        let made = made.lock().unwrap().clone().unwrap();
        assert!(!made.exists(), "{} was left behind", made.display());
    }

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
        /// When set, pairing the user with the token fails.
        fail_pair: Rc<Cell<bool>>,
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
        pub(super) fn note_token_user(&self, user: &str, seen: Option<&str>) -> Result<(), String> {
            if self.fail_pair.get() {
                return Err("token file not writable".to_owned());
            }
            if let Some(path) = self.file.borrow().as_ref() {
                // As the phone does it: only with the token seen.
                return super::super::token_file::pair(path, seen, user);
            }
            if self.token_present.get() {
                *self.token_user.borrow_mut() = Some(user.to_owned());
            }
            Ok(())
        }
        /// The token a test session is opened with: the file's, or "tok"
        /// while the modelled token is present.
        pub(super) fn token_seen(&self) -> Option<String> {
            match self.file.borrow().as_ref() {
                Some(path) => super::super::token_file::read(path)
                    .ok()
                    .flatten()
                    .map(|s| s.token),
                None => self.token_present.get().then(|| "tok".to_owned()),
            }
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
        /// The store the session hooks reach, in place of the registered one.
        pub(super) static STORE: RefCell<Option<std::sync::Arc<dyn VideoStore>>> = const { RefCell::new(None) };
    }

    /// The token a test's `remember_user` pairs with: the seam's.
    pub(super) fn seam_token() -> TokenSeen {
        TokenSeen(PREF.with(|p| p.borrow().as_ref().and_then(Pref::token_seen)))
    }

    thread_local! {
        /// Runs once after a cold start's reads, before what they found is
        /// installed.
        pub(super) static AFTER_COLD_READ: RefCell<Option<Box<dyn FnOnce()>>> = const { RefCell::new(None) };
        /// The fence verdicts logged on this thread.
        pub(super) static LOGGED: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
    }

    fn logged(prefix: &str) -> Vec<String> {
        LOGGED.with(|l| {
            l.borrow()
                .iter()
                .filter(|x| x.starts_with(prefix))
                .cloned()
                .collect()
        })
    }

    /// A session opened with the seam's token, as a test's `open` gives it.
    fn session<A>(api: A) -> Session<A> {
        Session::opened(api, seam_token().0)
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

    /// Records what the session hooks asked of the store.
    #[derive(Default)]
    struct Runs(std::sync::Mutex<Vec<String>>);

    impl VideoStore for Runs {
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
        fn park_runs(&self, owner: Option<&str>) {
            self.0
                .lock()
                .unwrap()
                .push(format!("park {}", owner.unwrap_or("all")));
        }
        fn resume_user(&self, user: &str) {
            self.0.lock().unwrap().push(format!("resume {user}"));
        }
    }

    /// A store whose resumed run reads the token as a fetch does: sent only
    /// while the token on file is paired with the run's owner.
    #[derive(Default)]
    struct TokenRuns(std::sync::Mutex<Vec<String>>);

    impl VideoStore for TokenRuns {
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
        fn resume_user(&self, user: &str) {
            let on_file = PREF.with(|p| {
                p.borrow()
                    .as_ref()
                    .and_then(|p| p.token_user.borrow().clone())
            });
            let verdict = match super::super::kept_video::token_for_owner(
                Some("tok"),
                on_file.as_deref(),
                user,
            ) {
                Ok(_) => "sent".to_owned(),
                Err(why) => format!("refused {why}"),
            };
            self.0
                .lock()
                .unwrap()
                .push(format!("resume {user} {verdict}"));
        }
    }

    /// A sign-in resumes the user's interrupted downloads only once their
    /// id is paired with the token: a run resumed before would read an
    /// unpaired token, send nothing and park until another sign-in. A
    /// pairing that fails resumes nothing (fail closed), and says so.
    #[test]
    fn a_sign_in_resumes_downloads_only_once_the_token_is_paired() {
        let p = pref();
        let runs = std::sync::Arc::new(TokenRuns::default());
        STORE.with(|s| *s.borrow_mut() = Some(runs.clone()));
        remember_user("A");
        assert_eq!(*runs.0.lock().unwrap(), ["resume A sent"]);
        forget_user();
        p.clear_token();
        p.token_present.set(true);
        p.fail_pair.set(true);
        remember_user("A");
        STORE.with(|s| *s.borrow_mut() = None);
        assert_eq!(
            *runs.0.lock().unwrap(),
            ["resume A sent"],
            "resumed with an unpaired token"
        );
    }

    /// A fresh video slot for this thread, as a store published late finds.
    fn late_slot() -> &'static super::super::ports::Slot<dyn VideoStore> {
        let slot = Box::leak(Box::new(super::super::ports::Slot::new()));
        super::super::ports::use_test_video_slot(Some(slot));
        slot
    }

    /// A sign-in that lands before the store is published (recovery parked
    /// the download first) resumes it once the store is: the publication
    /// replays the accepted sign-in, with the token still paired.
    #[test]
    fn a_sign_in_before_the_store_is_published_resumes_on_publication() {
        let _p = pref();
        late_slot();
        remember_user("A");
        let runs = std::sync::Arc::new(TokenRuns::default());
        super::super::ports::register_video_store(runs.clone()).unwrap();
        super::super::ports::use_test_video_slot(None);
        assert_eq!(
            *runs.0.lock().unwrap(),
            ["resume A sent"],
            "the sign-in before publication resumed nothing"
        );
    }

    /// The replay keeps the sign-in's checks: a user who signed out, or a
    /// token replaced, before the store is published resumes nothing.
    #[test]
    fn a_replayed_sign_in_resumes_nothing_once_its_user_or_token_changed() {
        let p = pref();
        late_slot();
        remember_user("A");
        forget_user();
        let runs = std::sync::Arc::new(Runs::default());
        super::super::ports::register_video_store(runs.clone()).unwrap();
        assert_eq!(*runs.0.lock().unwrap(), Vec::<String>::new());

        late_slot();
        remember_user("A");
        p.clear_token();
        p.token_present.set(true);
        let runs = std::sync::Arc::new(Runs::default());
        super::super::ports::register_video_store(runs.clone()).unwrap();
        super::super::ports::use_test_video_slot(None);
        assert_eq!(
            *runs.0.lock().unwrap(),
            Vec::<String>::new(),
            "resumed with the token replaced since the sign-in"
        );
    }

    /// The session-end hook parks the ended user's downloads (all of them
    /// when whose session it was is not known), and a sign-in resumes the
    /// new user's.
    #[test]
    fn a_session_end_parks_the_users_downloads_and_a_sign_in_resumes_theirs() {
        let _p = pref();
        let runs = std::sync::Arc::new(Runs::default());
        STORE.with(|s| *s.borrow_mut() = Some(runs.clone()));
        remember_user("A");
        forget_user();
        forget_user();
        remember_user("B");
        STORE.with(|s| *s.borrow_mut() = None);
        assert_eq!(
            *runs.0.lock().unwrap(),
            ["resume A", "park A", "park all", "resume B"]
        );
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

    /// A reconnect with B's token fences A at once: while B's `me` is
    /// pending, and after it fails, nothing of A's is served. A `me` of the
    /// new session lifts the fence for its user.
    #[test]
    fn a_reconnect_with_another_token_serves_nothing_until_its_user_is_known() {
        let _p = pref();
        remember_user("A");
        assert!(stored_for(&Files, last_user().as_deref(), "/k").is_some());
        let b = Session::opened((), Some("tokB".to_owned()));
        session_replaced(&b);
        // B's `me` is pending.
        assert_eq!(last_user(), None, "A was served on B's session");
        assert!(stored_for(&Files, last_user().as_deref(), "/k").is_none());
        // B's `me` failed: nothing answered, so nothing changes.
        assert_eq!(last_user(), None, "a failed me lifted the fence");
        assert!(stored_for(&Files, last_user().as_deref(), "/k").is_none());
        let fence = logged("pharos: video user fence verdict=fenced");
        assert!(
            fence
                .iter()
                .any(|l| l.contains("reason=token_not_paired user=A")),
            "{fence:?}"
        );
        let ask = me_request(true, Some(b)).unwrap();
        assert!(ask.answered("B"));
        assert_eq!(last_user().as_deref(), Some("B"));
    }

    /// A reconnect as the same user, with a token paired with them, keeps
    /// them served; one fenced by an unpaired token is served again once
    /// its own `me` names them.
    #[test]
    fn a_reconnect_as_the_same_user_is_served_once_established() {
        let _p = pref();
        remember_user("A");
        // The seam's token is paired with A: the reconnect keeps A.
        session_replaced(&session(()));
        assert_eq!(last_user().as_deref(), Some("A"));
        assert!(stored_for(&Files, last_user().as_deref(), "/k").is_some());
        // A reconnect with a token not yet paired fences A until A's `me`.
        let fresh = Session::opened((), Some("tokA2".to_owned()));
        session_replaced(&fresh);
        assert_eq!(last_user(), None);
        let ask = me_request(true, Some(fresh)).unwrap();
        assert!(ask.answered("A"));
        assert_eq!(last_user().as_deref(), Some("A"));
        assert!(stored_for(&Files, last_user().as_deref(), "/k").is_some());
        let lifted = logged("pharos: video user fence verdict=lifted");
        assert!(!lifted.is_empty());
    }

    /// A `me` asked on a session that a new one replaced (B's, published
    /// by the connect) names the old session's user: it installs nothing,
    /// and writes no preference. One asked on the new session does.
    #[test]
    fn a_me_asked_on_a_replaced_session_installs_nobody() {
        let p = pref();
        remember_user("A");
        let pref_before = p.value.borrow().clone();
        let ask = me_request(true, Some(session(()))).expect("a ready session asks");
        // The new session carries a token nobody is paired with yet.
        session_replaced(&Session::opened((), Some("tokB".to_owned())));
        assert!(
            !ask.answered("C"),
            "the replaced session's answer was written"
        );
        assert_eq!(last_user(), None, "A was served on another user's session");
        assert_eq!(
            *p.value.borrow(),
            pref_before,
            "the stale answer wrote the preference"
        );
        let ask = me_request(true, Some(session(()))).unwrap();
        assert!(ask.answered("B"));
        assert_eq!(last_user().as_deref(), Some("B"));
    }

    /// A `me` request taken while the session is up, the session ending
    /// before its future runs, and then its answer: the sign-out stands.
    #[test]
    fn a_me_taken_before_its_session_ended_does_not_bring_the_user_back() {
        let p = pref();
        remember_user("A");
        let ask = me_request(true, Some(session(()))).expect("a ready session asks");
        // The session ends after the request was taken, before it is sent.
        forget_user();
        assert!(!ask.answered("A"), "the stale answer was written");
        assert_eq!(last_user(), None, "the user came back after the sign-out");
        assert_eq!(p.value.borrow().as_deref(), Some(""));
        // A session that is not ready sends nothing.
        assert_eq!(me_request(false, Some(session(()))), None);
        assert_eq!(me_request::<()>(true, None), None);
        // The next session's request is remembered.
        let ask = me_request(true, Some(session(()))).unwrap();
        assert!(ask.answered("A"));
        assert_eq!(last_user().as_deref(), Some("A"));
    }

    /// The app's `me` resource itself (`app::use_me`, what the native app
    /// renders): rendered while Ready, its request taken; the session-end
    /// hook runs before the request's first poll; then `me` answers. The
    /// sign-out stands: no user, no ready copies, the preference cleared.
    #[test]
    fn the_me_resource_answered_after_the_session_ended_keeps_the_user_out() {
        use super::super::app::use_me;
        use super::super::machine::{Event, Machine};
        use dioxus::prelude::*;
        thread_local! {
            static ANSWER: RefCell<Option<tokio::sync::oneshot::Receiver<String>>> =
                const { RefCell::new(None) };
            static ON_ME_RAN: Cell<bool> = const { Cell::new(false) };
            /// The resource's value as last rendered: `None` while the
            /// request is pending, `Some(None)` once a rejected answer is
            /// through.
            static SEEN: RefCell<Option<Option<String>>> = const { RefCell::new(None) };
        }
        ON_ME_RAN.with(|r| r.set(false));
        SEEN.with(|s| *s.borrow_mut() = None);
        let p = pref();
        remember_user("A");
        let (tx, rx) = tokio::sync::oneshot::channel::<String>();
        ANSWER.with(|a| *a.borrow_mut() = Some(rx));
        fn probe() -> Element {
            let machine = use_signal(|| {
                let mut m = Machine::new().with_session_end(forget_user);
                m.apply(Event::Resumed { epoch: 0 });
                m
            });
            let session = use_signal(|| Some(session(())));
            let me = use_me(
                machine,
                session,
                Rc::new(|()| {
                    Box::pin(async {
                        let rx = ANSWER.with(|a| a.borrow_mut().take())?;
                        rx.await.ok()
                    })
                }),
                String::as_str,
                Rc::new(|_: &String| ON_ME_RAN.with(|r| r.set(true))),
            );
            let value = me.read().clone();
            SEEN.with(|s| *s.borrow_mut() = value);
            rsx! {}
        }
        let mut dom = VirtualDom::new(probe);
        dom.rebuild_in_place();
        // The session ends after the request was taken, before it runs.
        forget_user();
        tx.send("A".to_owned()).unwrap();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        // Driven until the answer is through: each wait must find work, so
        // an answer that never arrives fails here rather than passing.
        rt.block_on(async {
            for _ in 0..5 {
                if SEEN.with(|s| s.borrow().is_some()) {
                    break;
                }
                let woke =
                    tokio::time::timeout(std::time::Duration::from_secs(2), dom.wait_for_work())
                        .await;
                assert!(woke.is_ok(), "no work came: the answer never arrived");
                dom.render_immediate_to_vec();
            }
        });
        assert_eq!(
            SEEN.with(|s| s.borrow().clone()),
            Some(None),
            "the late answer was not processed, or was accepted"
        );
        assert_eq!(last_user(), None, "the stale answer brought the user back");
        assert!(
            !ON_ME_RAN.with(Cell::get),
            "the stale answer's follow-ups ran"
        );
        assert!(ready_for_last_user().is_empty());
        assert_eq!(
            p.value.borrow().as_deref(),
            Some(""),
            "the preference was not cleared"
        );
    }

    /// A stored file's answer is never cached by the WebView, whose cache
    /// does not know which user asked.
    #[test]
    fn a_stored_files_answer_is_never_cached() {
        let file = super::super::kept_video::StoredFile {
            path: std::path::PathBuf::new(),
            len: 0,
            content_type: "video/mp4",
        };
        let headers = offline_headers(&file);
        let cache = headers
            .iter()
            .find(|(name, _)| *name == "cache-control")
            .map(|(_, value)| *value);
        assert_eq!(cache, Some("no-store"));
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
        let d = TmpDir::new("seq");
        let path = d.path().join("device-token");
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
    }

    /// Another account's token stored between A's `me` request and its
    /// answer (a Quick Connect approved meanwhile) is never paired with A:
    /// the pairing compares the token the request was built with, under the
    /// file's lock, and refuses. The new token stays unpaired, so a
    /// download run of A's is refused its token, and A's downloads are not
    /// resumed.
    #[test]
    fn a_token_replaced_between_me_and_the_pairing_is_not_paired_with_a() {
        use super::super::kept_video::token_for_owner;
        use super::super::token_file;
        let d = TmpDir::new("pair-race");
        let path = d.path().join("device-token");
        let p = pref();
        *p.file.borrow_mut() = Some(path.clone());
        let runs = std::sync::Arc::new(Runs::default());
        STORE.with(|s| *s.borrow_mut() = Some(runs.clone()));
        token_file::store(&path, "tokA").unwrap();
        let ask = me_request(true, Some(session(()))).unwrap();
        // B's Quick Connect lands before A's answer is paired.
        token_file::store(&path, "tokB").unwrap();
        let _ = ask.answered("A");
        STORE.with(|s| *s.borrow_mut() = None);
        let on_file = token_file::read(&path).unwrap().unwrap();
        assert_eq!(on_file.user, None, "B's token was paired with A");
        assert!(
            token_for_owner(Some(&on_file.token), on_file.user.as_deref(), "A").is_err(),
            "A's run would be sent B's token"
        );
        assert!(
            runs.0
                .lock()
                .unwrap()
                .iter()
                .all(|r| !r.starts_with("resume")),
            "{:?}",
            runs.0.lock().unwrap()
        );
    }

    /// The phone's download client, over a real token file: each request
    /// takes its token through `token_file::run_token`, as the phone's
    /// fetcher does, and what went out is captured.
    struct FileFetcher {
        file: std::path::PathBuf,
        /// What the server answers a sent request with; a key with no
        /// bytes here is answered 403.
        files: std::collections::HashMap<String, Vec<u8>>,
        /// (owner, key, token) of each request sent.
        sent: std::sync::Mutex<Vec<(String, String, String)>>,
        /// (owner, reason) of each request not sent.
        refused: std::sync::Mutex<Vec<(String, String)>>,
    }

    impl FileFetcher {
        fn new(file: &std::path::Path) -> std::sync::Arc<Self> {
            Self::serving(file, Vec::new())
        }
        /// A fetcher whose server answers `files`.
        fn serving(
            file: &std::path::Path,
            files: Vec<(&'static str, Vec<u8>)>,
        ) -> std::sync::Arc<Self> {
            std::sync::Arc::new(Self {
                file: file.to_owned(),
                files: files.into_iter().map(|(k, v)| (k.to_owned(), v)).collect(),
                sent: std::sync::Mutex::default(),
                refused: std::sync::Mutex::default(),
            })
        }
        fn decide(&self, owner: &str) -> Result<String, String> {
            let decided = super::super::token_file::run_token(&self.file, owner);
            if let Err(e) = &decided {
                self.refused
                    .lock()
                    .unwrap()
                    .push((owner.to_owned(), e.clone()));
            }
            decided
        }
        fn sent(&self) -> Vec<(String, String, String)> {
            self.sent.lock().unwrap().clone()
        }
        fn refused(&self) -> Vec<(String, String)> {
            self.refused.lock().unwrap().clone()
        }
    }

    impl super::super::kept_video_disk::Fetcher for FileFetcher {
        fn open<'a>(
            &'a self,
            owner: &'a str,
            key: &'a str,
        ) -> super::super::kept_video_disk::BoxFut<
            'a,
            Result<
                Box<dyn super::super::kept_video_disk::Body>,
                super::super::kept_video_disk::OpenFailed,
            >,
        > {
            use super::super::kept_video_disk::{FetchError, OpenFailed};
            Box::pin(async move {
                let token = self.decide(owner).map_err(OpenFailed::NotOwner)?;
                self.sent
                    .lock()
                    .unwrap()
                    .push((owner.to_owned(), key.to_owned(), token));
                match self.files.get(key) {
                    Some(bytes) => Ok(Box::new(OneChunk(Some(bytes.clone())))
                        as Box<dyn super::super::kept_video_disk::Body>),
                    None => Err(OpenFailed::Fetch(FetchError {
                        status: Some(403),
                        message: String::new(),
                    })),
                }
            })
        }
        fn check_owner(&self, owner: &str) -> Result<(), String> {
            self.decide(owner).map(|_| ())
        }
    }

    /// A body sent whole in one chunk.
    struct OneChunk(Option<Vec<u8>>);

    impl super::super::kept_video_disk::Body for OneChunk {
        fn next_chunk(
            &mut self,
        ) -> super::super::kept_video_disk::BoxFut<
            '_,
            Result<Option<Vec<u8>>, super::super::kept_video_disk::FetchError>,
        > {
            let next = self.0.take();
            Box::pin(async move { Ok(next) })
        }
    }

    struct Roomy;

    impl super::super::kept_video_disk::Budget for Roomy {
        fn cap_bytes(&self) -> u64 {
            1 << 40
        }
        fn other_used_bytes(&self) -> u64 {
            0
        }
        fn free_bytes(&self) -> u64 {
            1 << 41
        }
    }

    /// The phone's store of downloads in `dir`, fetching through `fetcher`.
    fn disk_store(
        dir: &std::path::Path,
        fetcher: std::sync::Arc<FileFetcher>,
    ) -> std::sync::Arc<super::super::kept_video_disk::DiskVideoStore> {
        super::super::kept_video_disk::DiskVideoStore::open(
            dir.to_owned(),
            super::super::kept_video_disk::Env {
                fetcher,
                budget: std::sync::Arc::new(Roomy),
                sleep: std::sync::Arc::new(|_| Box::pin(async {})),
                now: || "2026-11-18T09:00:00Z".to_owned(),
                runtime: tokio::runtime::Handle::current(),
                remove_dir: |d| std::fs::remove_dir_all(d),
            },
        )
    }

    fn keep(user: &str, item: &str) -> KeepVideoRequest {
        KeepVideoRequest {
            item: item.into(),
            version: String::new(),
            user: user.into(),
            title: "Clip".into(),
            kind: "movie".into(),
            runtime_ms: 12_000,
            master_url: "/videos/1/master.m3u8?PlaySessionId=p".into(),
            audio: None,
            subtitles: vec![],
            quality_bps: super::super::kept_video::DEFAULT_QUALITY_BPS,
        }
    }

    /// Starts a download of `user`'s and waits, bounded, until its first
    /// request has been decided: sent, or refused before it went out.
    async fn first_request_decided(
        store: &super::super::kept_video_disk::DiskVideoStore,
        fetcher: &FileFetcher,
        user: &str,
        item: &str,
    ) {
        let before = fetcher.sent().len() + fetcher.refused().len();
        if store.start(keep(user, item)).is_err() {
            return;
        }
        for _ in 0..3000 {
            if fetcher.sent().len() + fetcher.refused().len() > before {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        panic!("{user}'s download never asked for its first file");
    }

    /// The late store, end to end on the host, through the code the phone
    /// runs: A's download was interrupted, and the restart's recovery parks
    /// it again (the token is not paired yet). A's session opens and its
    /// `me` pairs the token before the store is published. Publishing the
    /// store replays that sign-in: the download's requests go out with A's
    /// token and its files land on disk.
    #[test]
    fn a_download_parked_by_recovery_resumes_when_the_store_is_published_after_me() {
        use super::super::kept_video_disk::tests::stream_files;
        use super::super::token_file;
        let d = TmpDir::new("late-store");
        let path = d.path().join("device-token");
        let videos = d.path().join("videos");
        let p = pref();
        *p.file.borrow_mut() = Some(path.clone());
        token_file::store(&path, "tokA").unwrap();
        run(async {
            let fetcher = FileFetcher::serving(&path, stream_files());
            // A's download, interrupted: its first request is refused (the
            // token names nobody yet) and the run parks.
            {
                let before = disk_store(&videos, fetcher.clone());
                first_request_decided(&before, &fetcher, "A", "1").await;
                before.settled().await;
            }
            assert_eq!(fetcher.sent(), [], "a request went out unpaired");
            // The restart: recovery's run is refused and parks again.
            let refused = fetcher.refused().len();
            late_slot();
            let store = disk_store(&videos, fetcher.clone());
            store.settled().await;
            assert!(fetcher.refused().len() > refused, "recovery did not run");
            assert_eq!(store.used_bytes("A"), 0);
            // A's session and `me`, before the store is published.
            let Resumed::Opened(session_a) = resume_session(&FakeResumer::new(&path)).await else {
                panic!("A's token did not open A's session");
            };
            session_replaced(&session_a);
            let ask = me_request(true, Some(session_a)).unwrap();
            assert!(ask.answered("A"));
            assert_eq!(fetcher.sent(), [], "a request went out before publication");
            // Publication replays the sign-in.
            super::super::ports::register_video_store(store.clone()).unwrap();
            store.settled().await;
            super::super::ports::use_test_video_slot(None);
            let sent = fetcher.sent();
            assert!(
                !sent.is_empty() && sent.iter().all(|(o, _, t)| o == "A" && t == "tokA"),
                "the download did not resume with A's token: {sent:?} refused={:?}",
                fetcher.refused()
            );
            assert!(store.used_bytes("A") > 0, "no bytes landed");
            assert!(
                matches!(store.videos("A")[0].state, KeptState::Ready),
                "{:?}",
                store.videos("A")
            );
        });
    }

    /// The whole chain on the host, through the code the phone runs: open
    /// resumes A's session with A's token; B's token is stored before `me`
    /// answers; `me` names A. The pairing compares A's session token with
    /// the file and refuses, and A's download sends nothing: the server
    /// sees no request with B's token.
    #[test]
    fn a_token_stored_after_open_and_before_me_is_not_paired_and_a_sends_nothing() {
        use super::super::token_file;
        let d = TmpDir::new("open-me-b");
        let path = d.path().join("device-token");
        let p = pref();
        *p.file.borrow_mut() = Some(path.clone());
        token_file::store(&path, "tokA").unwrap();
        run(async {
            let fetcher = FileFetcher::new(&path);
            let store = disk_store(&d.path().join("videos"), fetcher.clone());
            STORE.with(|s| *s.borrow_mut() = Some(store.clone()));
            let Resumed::Opened(session_a) = resume_session(&FakeResumer::new(&path)).await else {
                panic!("A's token did not open A's session");
            };
            // B signs in on the media service: the file now holds B's token.
            token_file::store(&path, "tokB").unwrap();
            let ask = me_request(true, Some(session_a)).unwrap();
            let _ = ask.answered("A");
            assert_eq!(
                token_file::read(&path).unwrap().unwrap().user,
                None,
                "B's token was paired with A"
            );
            first_request_decided(&store, &fetcher, "A", "1").await;
            STORE.with(|s| *s.borrow_mut() = None);
            assert_eq!(fetcher.sent(), [], "A's download sent a request");
            assert!(
                fetcher
                    .refused()
                    .iter()
                    .any(|(o, why)| o == "A" && why.contains("token_user_unknown")),
                "{:?}",
                fetcher.refused()
            );
        });
    }

    /// A re-open as B races A's `me`: A's request is built on A's session;
    /// B's token is stored and the app re-opens with it before A's answer
    /// lands. A's answer carries A's token, so its pairing is refused, and
    /// no request with B's token goes out for A's run, before B's own `me`
    /// pairs the token with B or after.
    #[test]
    fn a_re_open_as_b_racing_as_me_sends_no_request_with_bs_token_for_a() {
        use super::super::token_file;
        let d = TmpDir::new("reopen-race");
        let path = d.path().join("device-token");
        let p = pref();
        *p.file.borrow_mut() = Some(path.clone());
        token_file::store(&path, "tokA").unwrap();
        run(async {
            let fetcher = FileFetcher::new(&path);
            let store = disk_store(&d.path().join("videos"), fetcher.clone());
            STORE.with(|s| *s.borrow_mut() = Some(store.clone()));
            let resumer = FakeResumer::new(&path);
            let Resumed::Opened(session_a) = resume_session(&resumer).await else {
                panic!("A's token did not open A's session");
            };
            // A's `me` is in flight.
            let ask_a = me_request(true, Some(session_a)).unwrap();
            // B signs in and the app re-opens with B's token.
            token_file::store(&path, "tokB").unwrap();
            let Resumed::Opened(session_b) = resume_session(&resumer).await else {
                panic!("B's token did not open B's session");
            };
            assert_eq!(*resumer.sent.borrow(), ["tokA", "tokB"]);
            // A's answer lands after the re-open.
            let _ = ask_a.answered("A");
            assert_eq!(
                token_file::read(&path).unwrap().unwrap().user,
                None,
                "A's late answer paired B's token with A"
            );
            first_request_decided(&store, &fetcher, "A", "1").await;
            // B's own `me` pairs B's token with B; A's run still gets none.
            let ask_b = me_request(true, Some(session_b)).unwrap();
            assert!(ask_b.answered("B"));
            assert_eq!(
                token_file::read(&path).unwrap().unwrap().user.as_deref(),
                Some("B")
            );
            first_request_decided(&store, &fetcher, "A", "2").await;
            STORE.with(|s| *s.borrow_mut() = None);
            let for_a: Vec<_> = fetcher
                .sent()
                .into_iter()
                .filter(|(o, _, _)| o == "A")
                .collect();
            assert_eq!(for_a, [], "a request went out for A's run");
            assert!(
                !fetcher
                    .sent()
                    .iter()
                    .any(|(o, _, t)| o == "A" && t == "tokB"),
                "B's token went out for A's run"
            );
            let why: Vec<_> = fetcher
                .refused()
                .into_iter()
                .filter(|(o, _)| o == "A")
                .map(|(_, why)| why)
                .collect();
            assert!(
                why.iter().any(|w| w.contains("token_user_unknown"))
                    && why.iter().any(|w| w.contains("token_owner_mismatch")),
                "A's requests were not refused for the token's owner: {why:?}"
            );
        });
    }

    /// The server side of a resume, over a real token file: a token it
    /// knows resumes the session of the user it was issued to (the API is
    /// that user's name); any other is refused.
    struct FakeResumer {
        file: std::path::PathBuf,
        users: Vec<(&'static str, &'static str)>,
        /// The tokens each resume was sent.
        sent: RefCell<Vec<String>>,
        /// Runs once, as a refused token is about to be dropped: a sign-in
        /// landing between the refusal and the drop.
        before_forget: RefCell<Option<Box<dyn FnOnce()>>>,
    }

    impl FakeResumer {
        fn new(file: &std::path::Path) -> Self {
            Self {
                file: file.to_owned(),
                users: vec![("tokA", "A"), ("tokB", "B")],
                sent: RefCell::default(),
                before_forget: RefCell::default(),
            }
        }
    }

    impl Resumer for FakeResumer {
        type Api = String;
        type Error = String;
        fn load_token(&self) -> Option<String> {
            super::super::token_file::read(&self.file)
                .ok()
                .flatten()
                .map(|s| s.token)
        }
        fn resume<'a>(
            &'a self,
            token: &'a str,
        ) -> futures_util::future::LocalBoxFuture<'a, Result<String, ResumeError<String>>> {
            self.sent.borrow_mut().push(token.to_owned());
            let user = self
                .users
                .iter()
                .find(|(t, _)| *t == token)
                .map(|(_, u)| u.to_string());
            Box::pin(async move { user.ok_or(ResumeError::Refused) })
        }
        fn forget_token(&self, refused: &str) -> bool {
            if let Some(f) = self.before_forget.borrow_mut().take() {
                f();
            }
            !matches!(
                super::super::token_file::forget_if(&self.file, refused),
                Ok(super::super::token_file::ForgetIf::Changed)
            )
        }
    }

    fn run<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(f)
    }

    /// A resume reads the token once and the session it gives answers for
    /// exactly that token; a refused token is dropped; no token sends
    /// nothing.
    #[test]
    fn a_resume_gives_the_session_with_the_token_it_was_sent() {
        use super::super::token_file;
        let d = TmpDir::new("resume");
        let path = d.path().join("device-token");
        let r = FakeResumer::new(&path);
        assert!(matches!(run(resume_session(&r)), Resumed::NoToken));
        assert!(
            r.sent.borrow().is_empty(),
            "a resume went out with no token"
        );
        token_file::store(&path, "tokA").unwrap();
        match run(resume_session(&r)) {
            Resumed::Opened(session) => {
                assert_eq!(
                    session,
                    Session::opened("A".to_owned(), Some("tokA".to_owned()))
                );
            }
            _ => panic!("A's token did not resume A's session"),
        }
        assert_eq!(*r.sent.borrow(), ["tokA"]);
        token_file::store(&path, "tokX").unwrap();
        assert!(matches!(run(resume_session(&r)), Resumed::Refused));
        assert_eq!(
            token_file::read(&path).unwrap(),
            None,
            "a refused token was kept"
        );
    }

    /// A sign-in that stores B's token while a refusal of the old token is
    /// on its way back keeps it: only the refused token is dropped, and the
    /// resume goes on with B's, which gives B's session.
    #[test]
    fn a_refusal_drops_only_the_refused_token_and_resumes_its_replacement() {
        use super::super::token_file;
        let d = TmpDir::new("resume-superseded");
        let path = d.path().join("device-token");
        let r = FakeResumer::new(&path);
        token_file::store(&path, "tokX").unwrap();
        let p2 = path.clone();
        *r.before_forget.borrow_mut() = Some(Box::new(move || {
            token_file::store(&p2, "tokB").unwrap();
        }));
        match run(resume_session(&r)) {
            Resumed::Opened(session) => assert_eq!(
                session,
                Session::opened("B".to_owned(), Some("tokB".to_owned()))
            ),
            _ => panic!("the replacement token was not resumed"),
        }
        assert_eq!(*r.sent.borrow(), ["tokX", "tokB"]);
        assert_eq!(
            token_file::read(&path).unwrap().map(|s| s.token).as_deref(),
            Some("tokB"),
            "the replacement was dropped with the refused token"
        );
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
        let d = TmpDir::new("answer");
        let file = d.path().join("a.m4s");
        std::fs::write(&file, b"A-own").unwrap();
        let store = OnDisk(file.clone());
        let path = d.path().join("device-token");
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

    /// A cold start read overtaken by a new session installs nothing: the
    /// read found A, B's session was published after it read, and the
    /// install's compare-and-set on the generation refuses A. A cold start
    /// read again after it is bound to B's token, which is not paired with
    /// A, so A's files are never selected.
    #[test]
    fn a_cold_start_overtaken_by_a_new_session_installs_nobody() {
        let p = pref();
        *p.value.borrow_mut() = Some("A".to_owned());
        *p.token_user.borrow_mut() = Some("A".to_owned());
        AFTER_COLD_READ.with(|h| {
            *h.borrow_mut() = Some(Box::new(|| {
                session_replaced(&Session::opened((), Some("tokB".to_owned())));
            }));
        });
        assert_eq!(last_user(), None, "the overtaken cold start installed A");
        assert!(stored_for(&Files, last_user().as_deref(), "/k").is_none());
        // Read again on B's session: bound to B's token, so still nobody.
        set_flag(COLD_TRIED, false);
        assert_eq!(last_user(), None, "a read on B's session installed A");
        assert!(stored_for(&Files, last_user().as_deref(), "/k").is_none());

        // The control: a session with A's own paired token reads A.
        let p = pref();
        *p.value.borrow_mut() = Some("A".to_owned());
        *p.token_user.borrow_mut() = Some("A".to_owned());
        session_replaced(&session(()));
        assert_eq!(last_user().as_deref(), Some("A"));
        assert!(stored_for(&Files, last_user().as_deref(), "/k").is_some());
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
            stale_on_disk: 0,
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
