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

fn identity() -> Identity {
    #[cfg(not(test))]
    {
        IDENTITY
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
    #[cfg(test)]
    {
        IDENTITY.with(|i| i.borrow().clone())
    }
}

fn set_identity(next: Identity) {
    #[cfg(not(test))]
    {
        *IDENTITY
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = next;
    }
    #[cfg(test)]
    {
        IDENTITY.with(|i| *i.borrow_mut() = next);
    }
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

/// The id of the user signed in, and so the one whose downloads may be
/// listed or served. The process's own knowledge wins; the stored
/// preference is read only while that is unknown (a cold start), and one
/// that cannot be read leaves it unknown: nobody.
pub fn last_user() -> Option<String> {
    match identity() {
        Identity::User(u) => Some(u),
        Identity::SignedOut => None,
        Identity::Unknown => match read_pref() {
            Ok(Some(u)) if !u.is_empty() => {
                super::platform::info(&format!(
                    "pharos: video user verdict=known reason=cold_start_from_preference user={u}"
                ));
                set_identity(Identity::User(u.clone()));
                Some(u)
            }
            Ok(_) => {
                set_identity(Identity::SignedOut);
                None
            }
            Err(e) => {
                super::platform::warn(&format!(
                    "pharos: video user verdict=none reason=preference_unreadable error={e}"
                ));
                None
            }
        },
    }
}

/// Notes the user who has signed in. Known in this process first; the
/// preference follows, and a failed write is logged, not served around.
pub fn remember_user(user: &str) {
    set_identity(Identity::User(user.to_owned()));
    super::platform::info(&format!(
        "pharos: video user verdict=known reason=signed_in user={user}"
    ));
    if matches!(read_pref(), Ok(Some(p)) if p == user) {
        return;
    }
    if let Err(e) = write_pref(user) {
        super::platform::warn(&format!(
            "pharos: video user not remembered (served for this run only) error={e}"
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
        super::platform::info(&format!(
            "pharos: video request verdict=not_served_offline reason=no_signed_in_user key_len={}",
            key.len()
        ));
        return None;
    };
    store.stored_file(user, key)
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

    /// The stored preference, replaceable: its reads and writes can fail.
    #[derive(Clone, Default)]
    pub(super) struct Pref {
        value: Rc<RefCell<Option<String>>>,
        fail_read: Rc<Cell<bool>>,
        fail_write: Rc<Cell<bool>>,
    }

    impl Pref {
        pub(super) fn read(&self) -> Result<Option<String>, String> {
            if self.fail_read.get() {
                return Err("unreadable".to_owned());
            }
            Ok(self.value.borrow().clone())
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

    /// A `remember_user` whose write fails still serves that user here.
    #[test]
    fn a_failed_remember_still_serves_the_right_user_in_this_process() {
        let p = pref();
        p.fail_write.set(true);
        remember_user("A");
        assert_eq!(last_user().as_deref(), Some("A"));
        assert!(serves(last_user()));
    }

    /// At a cold start the preference is where the user is learnt from; one
    /// that cannot be read leaves nobody served.
    #[test]
    fn a_cold_start_reads_the_preference_and_an_unreadable_one_serves_nobody() {
        let p = pref();
        *p.value.borrow_mut() = Some("A".to_owned());
        p.fail_read.set(true);
        assert_eq!(last_user(), None);
        assert!(!serves(last_user()));
        p.fail_read.set(false);
        assert_eq!(last_user().as_deref(), Some("A"));
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
