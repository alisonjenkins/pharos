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

/// Everything a sign-in, a sign-out, a session change and a cold start
/// read and change, behind one lock, so none of them ever sees one field
/// from before another's change and one from after it.
#[derive(Debug)]
struct IdState {
    identity: Identity,
    /// Counts every identity change and session replacement, so an answer
    /// asked before one of them can tell it is out of date (see
    /// [`remember_user_if`]).
    generation: u64,
    /// The device token of the session published last
    /// ([`session_replaced`]), once there is one. A cold start read after
    /// it is bound to it: only that token's paired user may be installed.
    session_token: Option<TokenSeen>,
    /// A cold start has read and found nobody, so a proxied request does
    /// not read the preference and the token again.
    cold_tried: bool,
    /// The refusal for nobody signed in has been logged since the identity
    /// last changed.
    refusal_logged: bool,
}

const ID_START: IdState = IdState {
    identity: Identity::Unknown,
    generation: 0,
    session_token: None,
    cold_tried: false,
    refusal_logged: false,
};

#[cfg(not(test))]
static ID: std::sync::Mutex<IdState> = std::sync::Mutex::new(ID_START);

#[cfg(test)]
thread_local! {
    static ID: std::cell::RefCell<IdState> = const { std::cell::RefCell::new(ID_START) };
}

/// Reads or changes the identity state under its one lock.
fn with_id<R>(f: impl FnOnce(&mut IdState) -> R) -> R {
    #[cfg(not(test))]
    {
        f(&mut ID.lock().unwrap_or_else(std::sync::PoisonError::into_inner))
    }
    #[cfg(test)]
    {
        ID.with(|i| f(&mut i.borrow_mut()))
    }
}

fn identity() -> Identity {
    with_id(|s| s.identity.clone())
}

/// The identity's generation now: taken before asking who is signed in, and
/// handed to [`remember_user_if`] with the answer.
pub fn identity_generation() -> u64 {
    with_id(|s| s.generation)
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
    let (now, verdict, reason, user) = with_id(|st| {
        st.generation = st.generation.wrapping_add(1);
        st.session_token = Some(session.token.clone());
        // A cold start that found nobody did so for the session before:
        // this one's token may be paired, so the next ask reads again.
        st.cold_tried = false;
        let i = &mut st.identity;
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
        (st.generation, verdict, reason, user)
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

#[cfg(test)]
fn set_identity(next: Identity) -> u64 {
    set_identity_if(None, next).unwrap_or_else(|now| now)
}

/// Sets the identity, under the identity lock, only while the generation is
/// still `expected` (any generation with `None`), and starts a new one,
/// which it returns. Returns the generation found when it is not the
/// expected one.
fn set_identity_if(expected: Option<u64>, next: Identity) -> Result<u64, u64> {
    with_id(|st| {
        let now = st.generation;
        if expected.is_some_and(|e| e != now) {
            return Err(now);
        }
        if let (Identity::Fenced(was), Identity::User(now)) = (&st.identity, &next) {
            note_fence(format!(
                "pharos: video user fence verdict=lifted reason=user_established was={was} user={now}"
            ));
        }
        // A new identity state is a new chance for a cold start (only a
        // test's restart sets Unknown) and a new refusal to log.
        st.refusal_logged = false;
        if next == Identity::Unknown {
            st.cold_tried = false;
            st.session_token = None;
        }
        st.identity = next;
        st.generation = st.generation.wrapping_add(1);
        Ok(st.generation)
    })
}

/// Installs what a cold start found, only while the identity is still
/// Unknown at generation `asked_at`. The generation is left as it is: a cold
/// start only fills in who was already signed in, so a `me` asked on the
/// current session before it still lands (its answer is authoritative),
/// while a sign-in, sign-out, revocation or new session since (each
/// advances the generation) refuses it. Returns the generation found when
/// it is refused.
fn install_cold_start(asked_at: u64, found: Identity) -> Result<(), u64> {
    with_id(|st| {
        if st.generation != asked_at || st.identity != Identity::Unknown {
            return Err(st.generation);
        }
        st.refusal_logged = false;
        st.identity = found;
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
fn cold_start_identity(session_token: Option<TokenSeen>) -> Identity {
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
    if let Some(seen) = session_token.filter(|t| t.0.is_some()) {
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
    // One snapshot of the state, the generation with it: a sign-out (or
    // anything else) between reading Unknown and reading the generation
    // would otherwise be missed by the install's compare-and-set.
    let (known, asked_at, cold_tried, session_token) = with_id(|s| {
        (
            s.identity.clone(),
            s.generation,
            s.cold_tried,
            s.session_token.clone(),
        )
    });
    #[cfg(test)]
    tests::AFTER_STATE_READ.with(|h| {
        if let Some(f) = h.borrow_mut().take() {
            f();
        }
    });
    if known != Identity::Unknown {
        return match known {
            Identity::User(u) => Some(u),
            _ => None,
        };
    }
    // A cold start that has run and found nobody is not run again for every
    // proxied request: a sign-in or `me` sets the identity, which is what
    // changes the answer.
    if cold_tried {
        return None;
    }
    // What the reads find is installed only through the one compare-and-set
    // every install uses, against the generation of the snapshot above, so
    // a sign-in, a sign-out or a session published since (each advances the
    // generation) wins.
    let found = cold_start_identity(session_token.clone());
    #[cfg(test)]
    tests::AFTER_COLD_READ.with(|h| {
        if let Some(f) = h.borrow_mut().take() {
            f();
        }
    });
    // A token stored since the read (a Quick Connect sign-in writes it before
    // it records its user, so no generation moved yet) must not be installed
    // under who the old one named: what decided is read again, and the
    // install goes ahead only if it still says the same.
    let confirmed = found == Identity::Unknown || cold_start_identity(session_token) == found;
    if !confirmed {
        super::platform::info(&format!(
            "pharos: video user verdict=not_installed reason=token_changed_during_cold_start asked_at={asked_at}"
        ));
    } else if found == Identity::Unknown {
        with_id(|s| {
            if s.generation == asked_at {
                s.cold_tried = true;
            }
        });
    } else {
        let verdict = if matches!(found, Identity::User(_)) {
            "known"
        } else {
            "none"
        };
        match install_cold_start(asked_at, found) {
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
    /// refused it), as one step under the file's lock, and says how that
    /// ended ([`TokenDropped`]).
    fn forget_token(&self, refused: &str) -> TokenDropped;
}

/// How dropping a refused token ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenDropped {
    /// Removed, or marked revoked on disk: it never resumes again.
    Gone,
    /// Removed, so refused in this process, but the folder's entry could
    /// not be made durable: a restart may bring the token back.
    GoneNotDurable,
    /// A sign-in replaced it since: the replacement is kept, and resumed.
    Replaced,
    /// Neither removed nor marked: this process refuses it (fenced), but
    /// a restart may resume its account from the file.
    Unsafe,
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
    /// The server refused the token, and it could be neither removed nor
    /// marked revoked: signed out in this process, but a restart may
    /// resume the account. Never retried.
    RefusedNotForgotten,
    /// The server refused the token and it was removed, but the removal
    /// may not be on disk: signed out in this process, and a restart may
    /// resume the account. Never retried.
    RefusedNotDurable,
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
            Err(ResumeError::Refused) => match r.forget_token(&token) {
                TokenDropped::Gone => return Resumed::Refused,
                TokenDropped::GoneNotDurable => {
                    super::platform::warn(&format!(
                        "pharos: resume verdict=refused_not_durable reason=token_removal_not_synced attempt={attempt}"
                    ));
                    return Resumed::RefusedNotDurable;
                }
                TokenDropped::Unsafe => {
                    super::platform::warn(&format!(
                        "pharos: resume verdict=refused_not_forgotten reason=token_neither_removed_nor_marked attempt={attempt}"
                    ));
                    return Resumed::RefusedNotForgotten;
                }
                TokenDropped::Replaced => super::platform::info(&format!(
                    "pharos: resume verdict=superseded reason=token_changed_since_read attempt={attempt} of={RESUME_TRIES}"
                )),
            },
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
    let at = set_identity(Identity::User(user.to_owned()));
    note_signed_in(user, &tests::seam_token(), at);
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
    /// Names this session among every one opened in the process: an answer
    /// asked on it is shown only while it is the published one.
    id: u64,
}

/// The id the next opened session gets.
static NEXT_SESSION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

impl<A> Session<A> {
    /// A session resumed with `token` (none where the platform has no
    /// device token).
    pub(crate) fn opened(api: A, token: Option<String>) -> Self {
        Self {
            api,
            token: TokenSeen(token),
            id: NEXT_SESSION.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        }
    }

    pub fn api(&self) -> &A {
        &self.api
    }

    /// This session's id, unique in the process.
    pub fn id(&self) -> u64 {
        self.id
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
    /// The id of the session it is sent on ([`Session::id`]).
    pub session: u64,
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
        session: session.id,
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
        Ok(at) => {
            note_signed_in(user, seen, at);
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

/// Starts `user`'s parked downloads on `store`, under the identity lock,
/// only while the identity is still `user` at generation `at` (taken under
/// that lock when the resume was decided). A session end fences the state
/// (signed out, a new generation) under the same lock before it parks, so
/// a resume either starts before the fence, and the park that follows
/// stops it, or finds the fence and starts nothing. Lock order: the
/// identity, then the store's own; the store never calls back into the
/// identity. Returns the generation found when it refuses.
fn start_resume_if(
    at: u64,
    user: &str,
    store: &dyn super::kept_video::VideoStore,
) -> Result<(), u64> {
    with_id(|st| {
        if st.generation != at || st.identity != Identity::User(user.to_owned()) {
            return Err(st.generation);
        }
        store.resume_user(user);
        Ok(())
    })
}

/// Resumes `user`'s downloads on the store now, or, with no store published
/// yet, owes the resume to its publication. `at` is the generation the
/// sign-in installed `user` at.
fn resume_or_owe(user: &str, seen: &TokenSeen, at: u64) {
    let now = with_owed(|owed| {
        let store = store();
        if store.is_none() {
            *owed = Some((user.to_owned(), seen.clone()));
        }
        store
    });
    match now {
        Some(store) => match start_resume_if(at, user, &*store) {
            Ok(()) => super::platform::info(&format!(
                "pharos: video keep resume verdict=resumed reason=token_paired user={user}"
            )),
            Err(now) => super::platform::info(&format!(
                "pharos: video keep resume verdict=not_resumed reason=identity_changed_since_sign_in user={user} at={at} now={now}"
            )),
        },
        None => super::platform::info(&format!(
            "pharos: video keep resume verdict=owed reason=store_not_published user={user}"
        )),
    }
}

/// Whether the session's token pairing lets `user`'s owed resume go on, by
/// the rule the immediate resume follows: with a token, it must be paired
/// with `user`; with none (the web's cookie session has no device token to
/// pair), there is nothing to check where no token decides who is signed
/// in, so the resume goes on. Where a token decides, a session without one
/// resumes nothing.
fn resume_pairing_holds(user: &str, seen: &TokenSeen) -> bool {
    if seen.0.is_none() && !token_decides_sign_in() {
        return true;
    }
    paired_user(seen).as_deref() == Some(user)
}

/// Whether a device token decides who is signed in on this platform.
fn token_decides_sign_in() -> bool {
    #[cfg(test)]
    if tests::PREF.with(|p| p.borrow().is_some()) {
        return true;
    }
    super::platform::TOKEN_DECIDES_SIGN_IN
}

/// Called once the store of downloaded videos is published: replays a
/// sign-in that landed before it, so a download recovery parked is resumed,
/// only while that user is still the one signed in and the token on file is
/// still the one paired with them (fail closed otherwise).
pub fn store_published() {
    let Some((user, seen)) = with_owed(Option::take) else {
        return;
    };
    // Validated against a generation taken with the identity, and started
    // only while that generation still holds (`start_resume_if`), so a
    // session end that fences between the two starts nothing.
    let (at, signed_in) =
        with_id(|st| (st.generation, st.identity == Identity::User(user.clone())));
    let reason = if !signed_in {
        Some("identity_changed")
    } else if !resume_pairing_holds(&user, &seen) {
        Some("token_not_paired")
    } else {
        None
    };
    #[cfg(test)]
    tests::AFTER_RESUME_VALIDATED.with(|h| {
        if let Some(f) = h.borrow_mut().take() {
            f();
        }
    });
    match (reason, store()) {
        (None, Some(store)) => {
            let mut at = at;
            for attempt in 1..=RESUME_REVALIDATIONS {
                let Err(now) = start_resume_if(at, &user, &*store) else {
                    super::platform::info(&format!(
                        "pharos: video keep resume verdict=resumed reason=store_published_after_sign_in user={user} attempt={attempt}"
                    ));
                    return;
                };
                // The generation moved since the validation. A reconnect
                // with the same credentials (A kept, same token) moves it
                // too: revalidated against the session as it is now, the
                // resume goes on at the new generation. A sign-out or a
                // credential change still refuses it.
                match revalidate_resume(&user, &seen) {
                    Ok(next) if attempt < RESUME_REVALIDATIONS => {
                        super::platform::info(&format!(
                            "pharos: video keep resume verdict=revalidated reason=same_session_credentials user={user} at={at} now={now} attempt={attempt}"
                        ));
                        at = next;
                    }
                    Ok(_) => super::platform::info(&format!(
                        "pharos: video keep resume verdict=not_resumed reason=generation_kept_moving user={user} at={at} now={now} attempt={attempt}"
                    )),
                    Err(why) => {
                        super::platform::info(&format!(
                            "pharos: video keep resume verdict=not_resumed reason={why} user={user} at={at} now={now}"
                        ));
                        return;
                    }
                }
            }
        }
        (why, _) => super::platform::info(&format!(
            "pharos: video keep resume verdict=not_resumed reason={} user={user}",
            why.unwrap_or("store_not_published")
        )),
    }
}

/// How many times an owed resume is revalidated after the generation
/// moved under it before it gives up.
const RESUME_REVALIDATIONS: u32 = 3;

/// An owed resume validated for `user` with the paired token `seen`, whose
/// generation moved before it started: the generation to start at when
/// `user` is still signed in, the current session's token is still `seen`
/// and the token on file is still paired with them; else why not.
fn revalidate_resume(user: &str, seen: &TokenSeen) -> Result<u64, &'static str> {
    let (at, signed_in, same_token) = with_id(|st| {
        (
            st.generation,
            st.identity == Identity::User(user.to_owned()),
            st.session_token.as_ref() == Some(seen),
        )
    });
    if !signed_in {
        Err("identity_changed_since_validated")
    } else if !same_token {
        Err("credentials_changed_since_validated")
    } else if !resume_pairing_holds(user, seen) {
        Err("token_not_paired")
    } else {
        Ok(at)
    }
}

fn note_signed_in(user: &str, seen: &TokenSeen, at: u64) {
    super::platform::info(&format!(
        "pharos: video user verdict=known reason=signed_in user={user}"
    ));
    // Their downloads parked at a session end, or by another user's token,
    // run again, but only once the token is paired with them: a run resumed
    // before would read an unpaired token, send nothing and park until the
    // next sign-in. A pairing that fails resumes nothing (fail closed).
    match note_token_user(user, seen) {
        Ok(()) => resume_or_owe(user, seen, at),
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

/// A signed-in session has ended for good (the app shows sign-in after it
/// was Ready: a revocation, a refused resume, a sign-out). Its user is
/// forgotten ([`forget_user`]), and the device token the session was
/// opened with is invalidated, only while it is still the one on file: a
/// replacement a new sign-in stored is kept. Without that, a revoked user's
/// token and a preference whose clearing failed would both still name
/// them, and an offline restart would show their downloads again. An
/// outage never comes here (the machine keeps the session). Returns the
/// warning for the sign-in screen when the token could be neither removed
/// nor marked ([`restart_warning`]).
pub fn session_ended() -> Option<String> {
    let end = fence_signed_out(true);
    park_ended(&end);
    tear_down_session(end)
}

/// Who a session end or sign-out ended, and the session's token, taken
/// under the identity lock by the fence.
struct SessionEnded {
    user: Option<String>,
    /// Who the session's token belongs to, when the identity says so: only
    /// a signed-in user. A fenced user is not paired with a replacement's
    /// token, so its owner comes from the token's pairing alone.
    token_user: Option<String>,
    token: Option<TokenSeen>,
    /// Nobody was signed in: an earlier session end already fenced and
    /// parked, and nothing can have started since.
    already_ended: bool,
}

/// The first step of every sign-out and session end: fence first, then
/// tear down. Under the identity lock, the state becomes signed out and
/// its generation advances (taking the session's token with `take_token`),
/// before anything is parked or any token removed, so a resume validated
/// before it starts nothing after it ([`start_resume_if`]).
fn fence_signed_out(take_token: bool) -> SessionEnded {
    let (end, now) = with_id(|st| {
        let (user, token_user) = match &st.identity {
            Identity::User(u) => (Some(u.clone()), Some(u.clone())),
            Identity::Fenced(u) => (Some(u.clone()), None),
            _ => (None, None),
        };
        let token = if take_token {
            st.session_token.take()
        } else {
            None
        };
        let already_ended = st.identity == Identity::SignedOut;
        st.identity = Identity::SignedOut;
        st.refusal_logged = false;
        st.generation = st.generation.wrapping_add(1);
        (
            SessionEnded {
                user,
                token_user,
                token,
                already_ended,
            },
            st.generation,
        )
    });
    super::platform::info(&format!(
        "pharos: video user verdict=none reason=signed_out fence=first generation={now}"
    ));
    end
}

/// The second step: the ended user's downloads stop (all of them when it
/// is not known), after the fence, before any token is removed.
fn park_ended(end: &SessionEnded) {
    if end.already_ended {
        super::platform::info(
            "pharos: video park verdict=skipped reason=already_ended_by_earlier_session_end",
        );
    } else {
        with_store(|store| store.park_runs(end.user.as_deref()));
    }
    #[cfg(test)]
    tests::AFTER_PARK.with(|h| {
        if let Some(f) = h.borrow_mut().take() {
            f();
        }
    });
}

/// The last step of a session end: the preference is cleared and the
/// session's token dropped, only while it is still the one on file.
/// Returns [`restart_warning`] for what happened to the token.
fn tear_down_session(end: SessionEnded) -> Option<String> {
    forget_pref();
    let Some(token) = end.token.and_then(|t| t.0) else {
        super::platform::info("pharos: token forget verdict=none reason=no_session_token");
        return None;
    };
    let outcome = forget_session_token(&token, end.token_user.as_deref());
    note_session_token(&outcome, end.token_user.as_deref());
    restart_warning(&outcome)
}

/// What the sign-in screen says when a session end's cleanup was not
/// saved: the token was neither removed nor marked revoked, so a restart
/// may sign its account back in to its downloads. The in-process sign-out
/// and the parked downloads stand; the warning says the cleanup did not.
/// None for any other end: the ordinary message stays as it is.
fn restart_warning(outcome: &TokenEnd) -> Option<String> {
    match outcome {
        TokenEnd::Unsafe { removal, .. } => Some(format!(
            "This device could not save the sign-out ({removal}), so a restart may sign the account back in to its downloads. Sign in and sign out again to retry."
        )),
        // The token is gone, but its absence may not be on disk: the same
        // warning the explicit sign-out gives (`SignOutReply::NotDurable`).
        TokenEnd::ForgottenNotDurable(why) => Some(format!(
            "This device removed the sign-in but could not confirm the removal was saved ({why}), so a restart may sign the account back in to its downloads. Sign in and sign out again to retry."
        )),
        // Durable removal, a kept replacement, and a synced revocation
        // marker (restarts refuse the token) stay quiet.
        TokenEnd::Forgotten | TokenEnd::KeptReplacement | TokenEnd::Marked(_) => None,
    }
}

/// What a session's end did to its device token, on every platform (the
/// phone's `token_file::Ended`; nothing to do where there is no token).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenEnd {
    /// Gone, or there was none.
    Forgotten,
    /// Gone, but its folder could not be synced (why).
    ForgottenNotDurable(String),
    /// A sign-in replaced it since: the replacement is kept.
    KeptReplacement,
    /// It would not go (why), but a synced marker names it revoked.
    Marked(String),
    /// Neither removed nor marked: a restart may resume its account.
    Unsafe { removal: String, marker: String },
}

impl TokenEnd {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Forgotten => "forgotten",
            Self::ForgottenNotDurable(_) => "forgotten_not_durable",
            Self::KeptReplacement => "kept_replacement",
            Self::Marked(_) => "marked_revoked",
            Self::Unsafe { .. } => "restart_unsafe",
        }
    }
}

#[cfg(any(target_os = "android", test))]
impl From<super::token_file::Ended> for TokenEnd {
    fn from(e: super::token_file::Ended) -> Self {
        use super::token_file::{Ended, Forgotten};
        match e {
            Ended::Forgotten(Forgotten::Durable) => Self::Forgotten,
            Ended::Forgotten(Forgotten::NotDurable(why)) => Self::ForgottenNotDurable(why),
            Ended::Changed => Self::KeptReplacement,
            Ended::Marked(why) => Self::Marked(why),
            Ended::Unsafe {
                removal, marker, ..
            } => Self::Unsafe { removal, marker },
        }
    }
}

/// Logs what a session's end did to its token, at warn when the token stayed.
fn note_session_token(outcome: &TokenEnd, user: Option<&str>) {
    let user = user.unwrap_or("");
    let line = match outcome {
        TokenEnd::Forgotten | TokenEnd::KeptReplacement => {
            let line = format!(
                "pharos: token forget verdict={} reason=session_ended user={user}",
                outcome.label()
            );
            super::platform::info(&line);
            line
        }
        TokenEnd::ForgottenNotDurable(why) | TokenEnd::Marked(why) => {
            let line = format!(
                "pharos: token forget verdict={} reason=session_ended user={user} error={why}",
                outcome.label()
            );
            super::platform::warn(&line);
            line
        }
        TokenEnd::Unsafe { removal, marker } => {
            let line = format!(
                "pharos: token forget verdict=restart_unsafe reason=session_ended user={user} removal_error={removal} marker_error={marker} restart_safety=not_guaranteed"
            );
            super::platform::warn(&line);
            line
        }
    };
    #[cfg(test)]
    tests::LOGGED.with(|l| l.borrow_mut().push(line));
    #[cfg(not(test))]
    let _ = line;
}

/// Drops `token` only while it is the one on file, marking it revoked for
/// `user` when it will not go.
fn forget_session_token(token: &str, user: Option<&str>) -> TokenEnd {
    #[cfg(test)]
    if let Some(path) = tests::PREF.with(|p| p.borrow().as_ref().and_then(tests::Pref::file)) {
        return super::token_file::end_session(&path, Some(token), user).into();
    }
    super::platform::forget_token_if(token, user)
}

/// Notes a sign-out. Nobody is signed in from this call on, whatever the
/// preference write does.
pub fn forget_user() {
    // Fence first, then tear down: signed out under the identity lock,
    // then whose session it was has their downloads stopped (all of them
    // when it is not known), before another user's token can be used.
    let end = fence_signed_out(false);
    park_ended(&end);
    forget_pref();
}

fn forget_pref() {
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
        if !with_id(|s| std::mem::replace(&mut s.refusal_logged, true)) {
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

/// Where the phone's hls.js loads a stream from. A master the store holds
/// for the signed-in user loads by its path and query on the page's own
/// origin, so it and every playlist and segment it names (resolved against
/// it) reach the local handler, which answers stored files from disk,
/// online or not: `Some(path)`. Anything else loads straight from the
/// server (`None`): a stored master sent there would fail with no network.
pub fn hls_local_source(
    store: Option<&dyn super::kept_video::VideoStore>,
    url: &str,
) -> Option<String> {
    offline_file_for(store, url).map(|_| super::kept_video::request_key(url))
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
    ready_snapshot().videos
}

/// One read of the ready videos and the owner they were read for: what a
/// screen decides on, logs and renders, so all three agree.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReadyVideos {
    /// The authorized owner the videos were read for ([`last_user`]).
    pub owner: Option<String>,
    pub videos: Vec<KeptVideo>,
}

/// The ready videos of the authorized owner, read once.
pub fn ready_snapshot() -> ReadyVideos {
    let (Some(store), Some(user)) = (super::ports::video_store(), last_user()) else {
        return ReadyVideos::default();
    };
    let videos = store.ready_videos(&user);
    ReadyVideos {
        owner: Some(user),
        videos,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::super::kept_video::KeptState;
    use super::*;

    /// CR review 6018104500 (offline_video.rs:1103-1104): an implicit
    /// session end whose removal is not durable warns like an explicit
    /// sign-out; durable removals and synced markers stay quiet.
    #[test]
    fn a_session_end_that_is_not_durable_warns() {
        let warned = restart_warning(&TokenEnd::ForgottenNotDurable("sync failed".into()));
        assert!(
            warned.is_some_and(|w| w.contains("could not confirm") && w.contains("sync failed")),
            "no warning for an unconfirmed removal"
        );
        assert_eq!(restart_warning(&TokenEnd::Forgotten), None);
        assert_eq!(restart_warning(&TokenEnd::KeptReplacement), None);
        assert_eq!(restart_warning(&TokenEnd::Marked("x".into())), None);
    }
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

    use super::super::test_dir::TmpDir;

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
        /// The real token file the seam reads, when it has one.
        pub(super) fn file(&self) -> Option<std::path::PathBuf> {
            self.file.borrow().clone()
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
        /// Runs once `last_user` has taken its snapshot of the state.
        pub(super) static AFTER_STATE_READ: RefCell<Option<Box<dyn FnOnce()>>> = const { RefCell::new(None) };
        /// Runs in `store_published` once the owed resume is validated,
        /// before it starts.
        pub(super) static AFTER_RESUME_VALIDATED: RefCell<Option<Box<dyn FnOnce()>>> = const { RefCell::new(None) };
        /// Runs in a session end once the runs are parked, before the
        /// token is removed.
        pub(super) static AFTER_PARK: RefCell<Option<Box<dyn FnOnce()>>> = const { RefCell::new(None) };
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

    /// Installs the stored-preference seam, whose token the next
    /// `remember_user` pairs its user with, for another module's test.
    pub(crate) fn use_pref_seam() {
        let _ = pref();
    }

    /// A session opened with the seam's token, for another module's test.
    pub(crate) fn seam_session() -> Session<()> {
        session(())
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

    /// The replay is refused for a changed token even when the new token
    /// is paired with the same user: A's sign-in on tokA, then tokA2 stored
    /// and paired with A before the store is published, resumes nothing.
    /// The control replays on the unchanged token and resumes A.
    #[test]
    fn a_replayed_sign_in_resumes_nothing_once_its_token_was_replaced() {
        use super::super::token_file;
        for replaced in [false, true] {
            let d = TmpDir::new("replay-token");
            let path = d.path().join("device-token");
            let p = pref();
            *p.file.borrow_mut() = Some(path.clone());
            token_file::store(&path, "tokA").unwrap();
            late_slot();
            remember_user("A");
            assert_eq!(p.token_user(), Ok(Some("A".to_owned())));
            if replaced {
                token_file::store(&path, "tokA2").unwrap();
                token_file::pair(&path, Some("tokA2"), "A").unwrap();
                assert_eq!(p.token_user(), Ok(Some("A".to_owned())));
            }
            let runs = std::sync::Arc::new(Runs::default());
            super::super::ports::register_video_store(runs.clone()).unwrap();
            super::super::ports::use_test_video_slot(None);
            let resumed = runs.0.lock().unwrap().clone();
            if replaced {
                assert_eq!(resumed, Vec::<String>::new(), "resumed on a replaced token");
            } else {
                assert_eq!(resumed, ["resume A"], "the control resumed nothing");
            }
        }
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
            ["resume A", "park A", "resume B"],
            "a second sign-out parked again"
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
                let mut m = Machine::new().with_session_end(|| {
                    forget_user();
                    None
                });
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
            let value = me.read().clone().map(|s| s.me);
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
    /// request has been decided: sent, or refused before it went out. A
    /// start the store refuses fails the test: a race test whose download
    /// never started would pass on nothing.
    async fn first_request_decided(
        store: &super::super::kept_video_disk::DiskVideoStore,
        fetcher: &FileFetcher,
        user: &str,
        item: &str,
    ) {
        let before = fetcher.sent().len() + fetcher.refused().len();
        if let Err(e) = store.start(keep(user, item)) {
            panic!("{user}'s download of {item} did not start: {e:?}");
        }
        for _ in 0..3000 {
            if fetcher.sent().len() + fetcher.refused().len() > before {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        panic!("{user}'s download never asked for its first file");
    }

    /// A session that ends for good (a revocation from Ready) drops the
    /// token it was opened with, so an offline restart whose preference
    /// could not be cleared serves nobody; a token a new sign-in stored
    /// since is kept.
    #[test]
    fn a_session_end_drops_its_own_token_and_keeps_a_replacement() {
        use super::super::token_file;
        let d = TmpDir::new("session-end-token");
        let path = d.path().join("device-token");
        let p = pref();
        *p.file.borrow_mut() = Some(path.clone());
        token_file::store(&path, "tokA").unwrap();
        run(async {
            let resumer = FakeResumer::new(&path);
            let Resumed::Opened(session_a) = resume_session(&resumer).await else {
                panic!("A's token did not open A's session");
            };
            session_replaced(&session_a);
            assert!(me_request(true, Some(session_a)).unwrap().answered("A"));
            // The preference cannot be cleared.
            p.fail_write.set(true);
            session_ended();
            assert_eq!(
                token_file::read(&path).unwrap(),
                None,
                "the revoked token stayed"
            );
            // An offline restart.
            set_identity(Identity::Unknown);
            assert_eq!(last_user(), None, "the revoked user came back offline");
            p.fail_write.set(false);

            token_file::store(&path, "tokA").unwrap();
            let Resumed::Opened(session_a) = resume_session(&resumer).await else {
                panic!("A's new token did not open A's session");
            };
            session_replaced(&session_a);
            assert!(me_request(true, Some(session_a)).unwrap().answered("A"));
            // B signs in on the media service before A's session ends.
            token_file::store(&path, "tokB").unwrap();
            session_ended();
            assert_eq!(
                token_file::read(&path).unwrap().map(|s| s.token).as_deref(),
                Some("tokB"),
                "the replacement token was dropped"
            );
        });
    }

    /// A's session ends and A's token will not go: it is marked revoked, so
    /// an offline restart (with the preference still naming A) restores
    /// nobody. With the marker unwritable too, the end says a restart is not
    /// safe.
    // Fault injection exists in debug builds only.
    #[cfg(debug_assertions)]
    #[test]
    fn a_session_end_whose_token_will_not_go_marks_it_and_restores_nobody() {
        use super::super::token_file;
        use pharos_content::token::faults;
        let d = TmpDir::new("session-end-marked");
        let path = d.path().join("device-token");
        let p = pref();
        *p.file.borrow_mut() = Some(path.clone());
        for both_fail in [false, true] {
            token_file::store(&path, "tokA").unwrap();
            run(async {
                let resumer = FakeResumer::new(&path);
                let Resumed::Opened(session_a) = resume_session(&resumer).await else {
                    panic!("A's token did not open A's session");
                };
                session_replaced(&session_a);
                assert!(me_request(true, Some(session_a)).unwrap().answered("A"));
            });
            p.fail_write.set(true);
            LOGGED.with(|l| l.borrow_mut().clear());
            faults::REMOVE.with(|f| f.set(true));
            faults::MARKER.with(|f| f.set(both_fail));
            let warned = session_ended();
            faults::REMOVE.with(|f| f.set(false));
            faults::MARKER.with(|f| f.set(false));
            assert!(path.exists(), "the fault removed the token");
            // Offline_video 1037: the sign-in screen is told when, and only
            // when, the cleanup was not saved.
            assert_eq!(
                warned
                    .as_deref()
                    .is_some_and(|w| w.contains("a restart may sign the account back in")),
                both_fail,
                "{warned:?}"
            );
            let lines = logged("pharos: token forget");
            if both_fail {
                assert!(
                    lines.iter().any(|l| l.contains("verdict=restart_unsafe")
                        && l.contains("restart_safety=not_guaranteed")
                        && l.contains("user=A")),
                    "{lines:?}"
                );
            } else {
                assert!(
                    lines
                        .iter()
                        .any(|l| l.contains("verdict=marked_revoked") && l.contains("user=A")),
                    "{lines:?}"
                );
                // An offline restart: the preference and the token both
                // still name A.
                set_identity(Identity::Unknown);
                assert_eq!(last_user(), None, "the revoked user came back offline");
            }
            p.fail_write.set(false);
            let _ = std::fs::remove_file(&path);
            let _ = std::fs::remove_file(pharos_content::token::revoked_path(
                &path,
                &pharos_content::token::fingerprint("tokA"),
            ));
            set_identity(Identity::Unknown);
        }
    }

    /// CR outside-diff offline_video.rs:971: a replacement session that
    /// ends before its `me` answers has A fenced, not signed in. Its token
    /// (B's, unpaired) is marked revoked naming nobody, never A: A's
    /// identity is not attributed to another user's token.
    #[cfg(debug_assertions)]
    #[test]
    fn a_replacement_session_ending_fenced_names_no_owner_for_its_token() {
        use super::super::token_file;
        use pharos_content::token::faults;
        let d = TmpDir::new("session-end-fenced");
        let path = d.path().join("device-token");
        let p = pref();
        *p.file.borrow_mut() = Some(path.clone());
        token_file::store(&path, "tokA").unwrap();
        run(async {
            let resumer = FakeResumer::new(&path);
            let Resumed::Opened(session_a) = resume_session(&resumer).await else {
                panic!("A's token did not open A's session");
            };
            session_replaced(&session_a);
            assert!(me_request(true, Some(session_a)).unwrap().answered("A"));
        });
        // B's sign-in replaces the token; its session opens before `me`.
        token_file::store(&path, "tokB").unwrap();
        session_replaced(&Session::opened((), Some("tokB".to_owned())));
        assert_eq!(last_user(), None, "A was not fenced");
        LOGGED.with(|l| l.borrow_mut().clear());
        faults::REMOVE.with(|f| f.set(true));
        session_ended();
        faults::REMOVE.with(|f| f.set(false));
        let marker =
            pharos_content::token::revoked(&path, &pharos_content::token::fingerprint("tokB"))
                .expect("B's token was not marked revoked");
        assert_eq!(marker.user, None, "A was named as B's token's owner");
        let lines = logged("pharos: token forget");
        assert!(
            lines
                .iter()
                .any(|l| l.contains("verdict=marked_revoked") && !l.contains("user=A")),
            "{lines:?}"
        );
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
            assert_eq!(store.video_bytes("A"), 0);
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
            assert!(store.video_bytes("A") > 0, "no bytes landed");
            assert!(
                matches!(store.videos("A")[0].state, KeptState::Ready),
                "{:?}",
                store.videos("A")
            );
        });
    }

    /// A session end that lands between an owed resume's validation and
    /// its start wins. Publication is paused once it has validated A's
    /// resume; the session end fences and parks; publication continues
    /// before the token is removed (it is still readable and paired with
    /// A). No run starts, and no request goes out.
    #[test]
    fn a_session_end_between_a_resumes_validation_and_its_start_starts_nothing() {
        use super::super::kept_video_disk::tests::stream_files;
        use super::super::token_file;
        let d = TmpDir::new("end-mid-resume");
        let path = d.path().join("device-token");
        let videos = d.path().join("videos");
        let p = pref();
        *p.file.borrow_mut() = Some(path.clone());
        token_file::store(&path, "tokA").unwrap();
        run(async {
            let fetcher = FileFetcher::serving(&path, stream_files());
            {
                let before = disk_store(&videos, fetcher.clone());
                first_request_decided(&before, &fetcher, "A", "1").await;
                before.settled().await;
            }
            assert!(
                !fetcher.refused().is_empty(),
                "A's first request was not refused, so no run parked"
            );
            // The restart: recovery's run is refused and parks again, so
            // there is a parked run for the race below.
            let refused = fetcher.refused().len();
            late_slot();
            let store = disk_store(&videos, fetcher.clone());
            store.settled().await;
            assert!(fetcher.refused().len() > refused, "recovery did not run");
            let Resumed::Opened(session_a) = resume_session(&FakeResumer::new(&path)).await else {
                panic!("A's token did not open A's session");
            };
            session_replaced(&session_a);
            let ask = me_request(true, Some(session_a)).unwrap();
            assert!(ask.answered("A"));
            assert_eq!(fetcher.sent(), [], "a request went out before publication");
            // Validated, paused: the session end fences and parks, and the
            // rest of it (the token's removal) waits until after.
            let ended = std::rc::Rc::new(std::cell::RefCell::new(None));
            let ended2 = ended.clone();
            AFTER_RESUME_VALIDATED.with(|h| {
                *h.borrow_mut() = Some(Box::new(move || {
                    let end = fence_signed_out(true);
                    park_ended(&end);
                    *ended2.borrow_mut() = Some(end);
                }));
            });
            super::super::ports::register_video_store(store.clone()).unwrap();
            assert_eq!(
                token_file::read(&path).unwrap().map(|s| s.token).as_deref(),
                Some("tokA"),
                "the token went before the resume continued"
            );
            store.settled().await;
            let sent = fetcher.sent();
            let end = ended
                .borrow_mut()
                .take()
                .expect("the session end did not run");
            tear_down_session(end);
            store.settled().await;
            super::super::ports::use_test_video_slot(None);
            assert_eq!(sent, [], "a run started after the fence sent a request");
            assert_eq!(
                fetcher.sent(),
                [],
                "a request went out after the session end"
            );
            assert_eq!(
                store.video_bytes("A"),
                0,
                "bytes landed after the session end"
            );
        });
    }

    /// A tokenless session (the web's) that signed in before the store was
    /// published has its owed resume replayed: there is no token to pair, as
    /// the immediate resume does not need one. Where a token decides, a
    /// session without one still resumes nothing.
    #[test]
    fn a_tokenless_sessions_owed_resume_is_replayed_when_the_store_is_published() {
        use super::super::kept_video::{KeepVideoRefusal, KeepVideoRequest};
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        #[derive(Default)]
        struct Counting(AtomicUsize);
        impl VideoStore for Counting {
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
            fn resume_user(&self, _: &str) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        let store = Arc::new(Counting::default());
        STORE.with(|s| *s.borrow_mut() = Some(store.clone()));
        PREF.with(|p| *p.borrow_mut() = None);
        set_identity(Identity::User("A".to_owned()));
        with_owed(|o| *o = Some(("A".to_owned(), TokenSeen(None))));
        store_published();
        assert_eq!(
            store.0.load(Ordering::SeqCst),
            1,
            "a tokenless session's owed resume was dropped"
        );
        // Where a token decides, no token is not a pairing.
        let _ = pref();
        set_identity(Identity::User("A".to_owned()));
        with_owed(|o| *o = Some(("A".to_owned(), TokenSeen(None))));
        store_published();
        assert_eq!(store.0.load(Ordering::SeqCst), 1, "an unpaired resume ran");
        PREF.with(|p| *p.borrow_mut() = None);
        STORE.with(|s| *s.borrow_mut() = None);
    }

    /// Outside diff offline_video.rs:901 (review 5995575621), the control
    /// beside the test above: a reconnect with A's same token between the
    /// owed resume's validation and its start moves the generation but
    /// keeps A. The resume is revalidated against the session now and
    /// goes on: A's parked download sends its requests.
    #[test]
    fn a_same_token_reconnect_between_a_resumes_validation_and_its_start_resumes() {
        use super::super::kept_video_disk::tests::stream_files;
        use super::super::token_file;
        let d = TmpDir::new("reconnect-mid-resume");
        let path = d.path().join("device-token");
        let videos = d.path().join("videos");
        let p = pref();
        *p.file.borrow_mut() = Some(path.clone());
        token_file::store(&path, "tokA").unwrap();
        run(async {
            let fetcher = FileFetcher::serving(&path, stream_files());
            {
                let before = disk_store(&videos, fetcher.clone());
                first_request_decided(&before, &fetcher, "A", "1").await;
                before.settled().await;
            }
            late_slot();
            let store = disk_store(&videos, fetcher.clone());
            store.settled().await;
            let Resumed::Opened(session_a) = resume_session(&FakeResumer::new(&path)).await else {
                panic!("A's token did not open A's session");
            };
            let Resumed::Opened(again) = resume_session(&FakeResumer::new(&path)).await else {
                panic!("A's token did not open A's second session");
            };
            session_replaced(&session_a);
            assert!(me_request(true, Some(session_a)).unwrap().answered("A"));
            let _ = super::super::platform::take_info_lines();
            AFTER_RESUME_VALIDATED.with(|h| {
                *h.borrow_mut() = Some(Box::new(move || session_replaced(&again)));
            });
            super::super::ports::register_video_store(store.clone()).unwrap();
            store.settled().await;
            super::super::ports::use_test_video_slot(None);
            assert!(
                !fetcher.sent().is_empty(),
                "a same-token reconnect lost A's owed resume"
            );
            let info = super::super::platform::take_info_lines();
            assert!(
                info.iter()
                    .any(|l| l.starts_with("pharos: video keep resume")
                        && l.contains("verdict=revalidated reason=same_session_credentials")),
                "{info:?}"
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
            // The app publishes B's session, as `connect` does.
            session_replaced(&session_b);
            // A's answer lands after the re-open, and is refused.
            assert!(!ask_a.answered("A"), "A's late answer was taken");
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
        /// How every drop ends, when set, instead of the file's answer.
        forget_ends: std::cell::Cell<Option<TokenDropped>>,
    }

    impl FakeResumer {
        fn new(file: &std::path::Path) -> Self {
            Self {
                file: file.to_owned(),
                users: vec![("tokA", "A"), ("tokB", "B")],
                sent: RefCell::default(),
                before_forget: RefCell::default(),
                forget_ends: std::cell::Cell::default(),
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
        fn forget_token(&self, refused: &str) -> TokenDropped {
            if let Some(f) = self.before_forget.borrow_mut().take() {
                f();
            }
            if let Some(end) = self.forget_ends.get() {
                return end;
            }
            match super::super::token_file::forget_if(&self.file, refused) {
                Ok(super::super::token_file::ForgetIf::Changed) => TokenDropped::Replaced,
                _ => TokenDropped::Gone,
            }
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
                    (session.api.as_str(), &session.token),
                    ("A", &TokenSeen(Some("tokA".to_owned())))
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

    /// CR 4640611359: a refused token that can be neither removed nor
    /// marked is its own outcome, not a plain refusal, and is never
    /// retried: the caller can warn that a restart may resume it.
    #[test]
    fn a_refused_token_that_will_not_go_is_its_own_outcome() {
        use super::super::token_file;
        let d = TmpDir::new("resume-unsafe");
        let path = d.path().join("device-token");
        let r = FakeResumer::new(&path);
        token_file::store(&path, "tokX").unwrap();
        r.forget_ends.set(Some(TokenDropped::Unsafe));
        assert!(matches!(
            run(resume_session(&r)),
            Resumed::RefusedNotForgotten
        ));
        assert_eq!(*r.sent.borrow(), ["tokX"], "an unsafe end was retried");
    }

    /// A refused token whose removal could not be made durable is
    /// its own outcome, not a plain refusal: the caller can warn that a
    /// restart may resume it. It is never retried.
    #[test]
    fn a_refused_token_removed_but_not_durably_is_its_own_outcome() {
        use super::super::token_file;
        let d = TmpDir::new("resume-not-durable");
        let path = d.path().join("device-token");
        let r = FakeResumer::new(&path);
        token_file::store(&path, "tokX").unwrap();
        r.forget_ends.set(Some(TokenDropped::GoneNotDurable));
        assert!(matches!(
            run(resume_session(&r)),
            Resumed::RefusedNotDurable
        ));
        assert_eq!(*r.sent.borrow(), ["tokX"], "a not-durable end was retried");
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
                (session.api.as_str(), &session.token),
                ("B", &TokenSeen(Some("tokB".to_owned())))
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
    /// A sign-out that lands after a cold start has read the state (Unknown)
    /// installs nobody: the install compares against the generation of that
    /// same snapshot, not one read after the sign-out. The preference write
    /// of the sign-out fails, so the files still name A.
    #[test]
    fn a_sign_out_after_the_cold_starts_state_read_installs_nobody() {
        let p = pref();
        *p.value.borrow_mut() = Some("A".to_owned());
        *p.token_user.borrow_mut() = Some("A".to_owned());
        p.fail_write.set(true);
        AFTER_STATE_READ.with(|h| *h.borrow_mut() = Some(Box::new(forget_user)));
        assert_eq!(
            last_user(),
            None,
            "the cold start installed A over the sign-out"
        );
        assert_eq!(identity(), Identity::SignedOut);
        assert!(stored_for(&Files, last_user().as_deref(), "/k").is_none());
    }

    /// A cold start that finds the user a pending `me` was asked about
    /// leaves that ask current: its answer still lands, and so does one
    /// naming another user (the server's answer is authoritative).
    #[test]
    fn a_cold_start_leaves_a_pending_me_current() {
        for answer in ["A", "B"] {
            let p = pref();
            *p.value.borrow_mut() = Some("A".to_owned());
            *p.token_user.borrow_mut() = Some("A".to_owned());
            let asked_at = identity_generation();
            assert_eq!(last_user().as_deref(), Some("A"));
            assert!(
                remember_user_if(asked_at, answer),
                "the cold start invalidated the pending me ({answer})"
            );
            assert_eq!(identity(), Identity::User(answer.to_owned()));
        }
    }

    /// A cold start that found nobody is not read again for the same
    /// session, but a new session is a new chance: its token may be paired
    /// with the remembered user.
    #[test]
    fn a_new_session_reads_the_cold_start_again() {
        let p = pref();
        *p.value.borrow_mut() = Some("A".to_owned());
        assert_eq!(last_user(), None, "no token paired yet");
        assert!(
            with_id(|s| s.cold_tried),
            "the empty read was not remembered"
        );
        *p.token_user.borrow_mut() = Some("A".to_owned());
        assert_eq!(last_user(), None, "the same session read again");
        session_replaced(&session(()));
        assert!(
            !with_id(|s| s.cold_tried),
            "a new session kept the empty read"
        );
        assert_eq!(last_user().as_deref(), Some("A"));
    }

    /// A token stored while the cold start read (a Quick Connect sign-in
    /// writes the token first, and moves no generation yet) is not installed
    /// under the user the old token named.
    #[test]
    fn a_cold_start_whose_token_changed_under_it_installs_nobody() {
        let p = pref();
        *p.value.borrow_mut() = Some("A".to_owned());
        *p.token_user.borrow_mut() = Some("A".to_owned());
        let p2 = p.clone();
        AFTER_COLD_READ.with(|h| {
            *h.borrow_mut() = Some(Box::new(move || {
                *p2.token_user.borrow_mut() = Some("B".to_owned());
            }));
        });
        assert_eq!(last_user(), None, "A was installed over B's new token");
        assert_eq!(identity(), Identity::Unknown);
        assert!(
            !with_id(|s| s.cold_tried),
            "the changed read was remembered as empty"
        );
    }

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
        with_id(|s| s.cold_tried = false);
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
            transferred: None,
            halt: None,
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
