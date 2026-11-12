//! Playback: the decision a client's profile and switches get for one item
//! (spec 017 US6). `/Items/{id}/PlaybackInfo` and the native
//! `Playback.negotiate` both decide here, so a client cannot be told
//! "direct play" over one API and "transcode" over the other.

use std::collections::BTreeSet;

use pharos_core::{
    MediaItem, MediaKind, SeriesMetadataStore as _, SubtitleTrack, UserDataStore as _,
};
use pharos_jellyfin_api::device_profile::{
    negotiate_explained, CodecCap, Decision, DeviceProfile, DirectPlayBlock, ServerCodecSupport,
    SourceMedia, SubtitleProfileDto,
};

use crate::api::jellyfin::dto::container_for;
use crate::api::jellyfin::media_source;
use crate::api::jellyfin::subtitle_delivery::{decide_subtitle_delivery, SubtitleDelivery};
use crate::api::jellyfin::subtitles::is_image_subtitle_codec;
use crate::state::AppState;

/// The named-config key holding one device's audio codec deny list (T130).
pub fn denied_audio_codecs_key(device_id: &str) -> String {
    format!("devaudionodirect:{device_id}")
}

/// At most this many codecs per device: the real set is a handful of
/// bitstream formats (ac3, eac3, truehd, dts), so more is a malformed request.
pub const MAX_DENIED_AUDIO_CODECS: usize = 16;

/// Why a deny list was refused, naming the offending value.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum DeniedCodecsError {
    #[error("{count} codecs given; at most {MAX_DENIED_AUDIO_CODECS} allowed")]
    TooMany { count: usize },
    #[error("codec {codec:?} is not a codec name (1-16 of a-z, 0-9, _ or -)")]
    BadName { codec: String },
    /// Every conversion path ends in AAC (the HLS segment encoder makes AAC
    /// whatever target the negotiator names), so denying it cannot be honoured.
    #[error("{CONVERSION_AUDIO_CODEC} is what turned-off codecs are converted to, so it cannot be turned off")]
    ConversionTarget,
}

/// The codec a denied one is converted to.
pub const CONVERSION_AUDIO_CODEC: &str = "aac";

/// A deny list as stored: lowercased, trimmed, deduplicated and sorted, so the
/// same set always reads back the same. Blank entries are dropped.
pub fn normalise_denied_audio_codecs(codecs: &[String]) -> Result<Vec<String>, DeniedCodecsError> {
    let set: BTreeSet<String> = codecs
        .iter()
        .map(|c| c.trim().to_ascii_lowercase())
        .filter(|c| !c.is_empty())
        .collect();
    if let Some(bad) = set.iter().find(|c| {
        c.len() > 16
            || !c
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
    }) {
        return Err(DeniedCodecsError::BadName { codec: bad.clone() });
    }
    if set.contains(CONVERSION_AUDIO_CODEC) {
        return Err(DeniedCodecsError::ConversionTarget);
    }
    if set.len() > MAX_DENIED_AUDIO_CODECS {
        return Err(DeniedCodecsError::TooMany { count: set.len() });
    }
    Ok(set.into_iter().collect())
}

/// The audio codecs `device_id` must not direct-play. Empty when none are set,
/// and also when the stored value cannot be read: the client's own claim then
/// stands, which is how it played before the setting existed.
///
/// A list saved before [`CONVERSION_AUDIO_CODEC`] was refused can still hold
/// it. It is dropped here, so the device list never shows it and the next
/// save, which sends the whole list back, is not refused for it.
pub async fn denied_audio_codecs(state: &AppState, device_id: &str) -> Vec<String> {
    match try_denied_audio_codecs(state, device_id).await {
        Ok(codecs) => codecs,
        Err(e) => {
            tracing::warn!(device.id = device_id, error = %e, "device audio deny list unreadable");
            Vec::new()
        }
    }
}

/// [`denied_audio_codecs`], but a store that cannot be read is an error, not
/// an empty list. A settings listing uses this: showing an empty list there
/// would let the next toggle, which saves the whole list, wipe the stored one.
pub async fn try_denied_audio_codecs(
    state: &AppState,
    device_id: &str,
) -> Result<Vec<String>, String> {
    use pharos_store_sqlx::ServerConfigStore as _;
    let key = denied_audio_codecs_key(device_id);
    let raw = match state.stores.load_named_config(&key).await {
        Ok(Some(raw)) => raw,
        Ok(None) => return Ok(Vec::new()),
        Err(e) => return Err(e.to_string()),
    };
    Ok(match serde_json::from_str::<Vec<String>>(&raw) {
        Ok(mut codecs) => {
            let before = codecs.len();
            codecs.retain(|c| !c.eq_ignore_ascii_case(CONVERSION_AUDIO_CODEC));
            if codecs.len() != before {
                tracing::warn!(
                    device.id = device_id,
                    codec = CONVERSION_AUDIO_CODEC,
                    "device audio deny list holds the conversion codec; ignoring it"
                );
            }
            codecs
        }
        Err(e) => {
            tracing::warn!(device.id = device_id, value = %raw, error = %e, "device audio deny list is not a JSON list");
            Vec::new()
        }
    })
}

/// [`denied_audio_codecs`] for the device `token` was issued to.
pub async fn denied_audio_codecs_for_token(state: &AppState, token: &str) -> Vec<String> {
    use pharos_core::TokenStore as _;
    match state.stores.device_for_token(token).await {
        Ok(Some(device)) => denied_audio_codecs(state, &device).await,
        Ok(None) => Vec::new(),
        Err(e) => {
            tracing::warn!(error = %e, "device lookup for the audio deny list failed");
            Vec::new()
        }
    }
}

/// Which delivery paths a PlaybackInfo request allows; everything is allowed
/// unless the body says `false`.
#[derive(Debug, Clone, Copy)]
pub struct PlaybackSwitches {
    pub direct_play: bool,
    pub direct_stream: bool,
    pub transcoding: bool,
    pub video_copy: bool,
    pub audio_copy: bool,
}

impl Default for PlaybackSwitches {
    fn default() -> Self {
        Self {
            direct_play: true,
            direct_stream: true,
            transcoding: true,
            video_copy: true,
            audio_copy: true,
        }
    }
}

impl PlaybackSwitches {
    /// Whether the request turned off any delivery path. Only then is an
    /// unplayable response the request's doing rather than the negotiator's.
    pub fn restricts(&self) -> bool {
        !(self.direct_play && self.direct_stream && self.transcoding)
    }
}

/// What a client asked for: its profile, track picks and switches, and the
/// facts about the client the wire layer derived from the request.
pub struct NegotiateRequest<'a> {
    pub profile: &'a DeviceProfile,
    pub switches: PlaybackSwitches,
    /// The client's explicit subtitle pick; `-1` is "off", `None` is "not
    /// said" (which falls back to the container's default track).
    pub explicit_subtitle_index: Option<i64>,
    /// The `StereoMode` query value (`mono`, `mono2d`, `preserve`).
    pub stereo_mode: Option<&'a str>,
    /// Whether the client reached the server over a non-private address.
    pub client_is_remote: bool,
    /// A browser (`Mozilla` user agent) rather than a native app.
    pub is_web_client: bool,
}

/// The negotiated outcome for one item, with the facts the wire layer needs
/// to build URLs, logs and its response.
#[derive(Debug)]
pub struct Negotiated {
    pub source: SourceMedia,
    pub container: String,
    pub is_video: bool,
    /// The capability set the transcode target was ranked from; the wire
    /// layer ranks its HLS renditions from the same one.
    pub server_codecs: ServerCodecSupport,
    /// The verdict after every override, switches included.
    pub decision: Decision,
    /// The verdict before the client's switches, which decide whether the raw
    /// `/stream` file is still on offer.
    pub pre_switch_decision: Decision,
    /// Why the client's profile did not direct-play, as the negotiator saw it.
    pub direct_play_block: DirectPlayBlock,
    /// The last post-negotiation override that replaced the negotiator's
    /// decision: `browser_matroska`, `stereo_mode_mono`, `subtitle_burn` or
    /// `client_switch`.
    pub downgrade: Option<&'static str>,
    /// The WAN bitrate cap applied to this client, if it is remote.
    pub remote_ceiling: Option<u64>,
    /// Subtitle streams this client cannot render itself and so must have
    /// burned in. Registered with the session: later segment URLs carry only
    /// a bare index.
    pub burn_text_indices: BTreeSet<u32>,
    /// The subtitle this playback acts on (explicit pick, else the
    /// container's default), when there is one.
    pub selected_subtitle: Option<SubtitleTrack>,
    pub explicit_mono: bool,
    pub explicit_preserve: bool,
    /// A `CodecProfile` condition rejected the source's 3D format.
    pub profile_rejected_3d: bool,
    /// The source range a `VideoRangeType` condition would have blocked, when
    /// enforcement is off and so playback went ahead.
    pub video_range_unenforced: Option<String>,
    /// A subtitle is burned into a packed 3D source: drawn into each eye, the
    /// layout kept (V234), unless the output is 2D.
    pub burning_sub_per_eye: bool,
    pub stereo_output: pharos_transcode::StereoOutput,
    pub direct_play: bool,
    /// Whether the raw `/stream` file plays as-is for this client.
    pub supports_direct_stream: bool,
    /// Whether the source is a WebM the client can play as served.
    pub webm_relabel_playable: bool,
}

impl Negotiated {
    /// The decision as a metric and log label.
    pub fn decision_kind(&self) -> &'static str {
        match &self.decision {
            Decision::DirectPlay => "direct_play",
            Decision::AudioRemux { .. } => "audio_remux",
            Decision::VideoRemux { .. } => "video_remux",
            Decision::Transcode { .. } => "transcode",
        }
    }

    /// Counts the verdict, so "how many sessions are transcoding, and for what
    /// reason" is a query rather than a log trawl. Label cardinality is
    /// bounded: 4 decisions × 10 block reasons × 5 downgrades (`none` plus the
    /// four in [`Negotiated::downgrade`]).
    pub fn record(&self) {
        metrics::counter!(
            "pharos_playback_decision_total",
            "decision" => self.decision_kind(),
            "direct_play_block" => self.direct_play_block.label(),
            "downgrade" => self.downgrade.unwrap_or("none"),
        )
        .increment(1);
        if let Some(range) = &self.video_range_unenforced {
            metrics::counter!(
                "pharos_video_range_unenforced_total",
                "range" => range_label(range),
            )
            .increment(1);
            tracing::info!(
                video_range = %range,
                "a VideoRangeType condition would have blocked direct play; \
                 enforce_video_range is off"
            );
        }
    }
}

/// The `range` label of `pharos_video_range_unenforced_total`, bounded to the
/// ranges the probe can report.
fn range_label(range: &str) -> &'static str {
    match range.parse::<pharos_core::VideoRangeType>() {
        Ok(r) if r.is_hdr() => r.label(),
        _ => "other",
    }
}

/// Decides how `item` plays for the client in `req`: negotiation against its
/// profile, the WAN clamp, the downgrades in the order they apply, the
/// client's own switches last, and the 3D output.
pub fn negotiate(state: &AppState, item: &MediaItem, req: &NegotiateRequest<'_>) -> Negotiated {
    let stripped;
    let profile = if state.enforce_video_range {
        req.profile
    } else {
        stripped = req.profile.without_video_range_conditions();
        &stripped
    };
    let switches = req.switches;
    let explicit_subtitle_index = req.explicit_subtitle_index;
    let is_web_client = req.is_web_client;
    let is_video = matches!(item.kind, MediaKind::Movie | MediaKind::Episode);
    let probe = &item.probe;

    // Source-media shape pulled from the probe persisted by the scanner
    // (T29 follow-up). Container falls back to a kind-derived default
    // only when ffprobe never ran (mirrors `container_for` in dto.rs so
    // negotiator + DTO see the same value).
    let container = container_for(probe, &item.path, is_video);
    let source = SourceMedia {
        container: container.clone(),
        video_codec: probe.video_codec.clone(),
        audio_codec: probe.audio_codec.clone(),
        bitrate_bps: probe.bitrate_bps,
        is_video,
        video_level: probe.video_level,
        video_profile: probe.video_profile.clone(),
        audio_channels: probe.audio_channels,
        width: probe.width,
        height: probe.height,
        video_bit_depth: pharos_jellyfin_api::device_profile::bit_depth_from_pix_fmt(
            probe.pixel_format.as_deref(),
        ),
        video_3d_format: probe.video_3d_format,
        video_range_type: is_video.then(|| probe.video_range_type().label().to_owned()),
    };

    // Capability-aware negotiation: the transcode target is the best codec in
    // (client-decodable ∩ server-encodable), hardware-preferred — so Firefox
    // lands on hardware h264 (or a hardware VP9 where the GPU supports it)
    // instead of a forced software VP9 encode. Built from the boot-resolved
    // server encode capabilities (empty in tests → h264 fallback, matching the
    // old hardcode).
    let server_codecs = server_codec_support(&state.encode_capabilities);
    // 008 — a URL-backed item has no file, and every byte route that serves a
    // direct play or a remux (`/videos/{id}/stream`) is built on NamedFile +
    // tokio::fs. So direct play is impossible for it regardless of what the
    // client would accept, and a remux is too: both copy a bitstream out of a
    // file that is not there.
    //
    // Expressed by negotiating against a profile with NO DirectPlayProfiles
    // rather than by rewriting the verdict afterwards. That reuses the
    // negotiator's own tested fall-through — which picks a target container and
    // codecs from the client's TranscodingProfile — instead of inventing a
    // transcode target here, where nothing knows what the client can decode.
    //
    // The reason is then restated as RemoteSource, because the fall-through
    // would otherwise report `no_profile`: true of the profile this code just
    // constructed, but a lie about the client, and indistinguishable on the
    // dashboard from the real "clients are sending nothing" failure.
    let remote = item.origin().is_remote();
    // Spec 037 — a split film's file is only its first part, so it can no
    // more be direct played or remuxed from `/stream` than a URL can: it
    // plays as one film through HLS, which runs on into each part (V1155).
    let split_parts = (item.edition.is_none() && !probe.parts().is_empty()).then(|| {
        u32::try_from(probe.parts().len())
            .unwrap_or(u32::MAX)
            .saturating_add(1)
    });
    let mut video_range_unenforced = None;
    let (decision, direct_play_block) = if remote || split_parts.is_some() {
        let mut forced = profile.clone();
        forced.direct_play_profiles.clear();
        let (d, _) = negotiate_explained(&forced, &source, &server_codecs);
        let block = match split_parts {
            Some(parts) if !remote => DirectPlayBlock::SplitFilm { parts },
            _ => DirectPlayBlock::RemoteSource {
                locator: item.path.display().to_string(),
            },
        };
        (d, block)
    } else {
        let verdict = negotiate_explained(profile, &source, &server_codecs);
        if !state.enforce_video_range {
            let (would, block) = negotiate_explained(req.profile, &source, &server_codecs);
            if would != verdict.0 && matches!(block, DirectPlayBlock::VideoRange { .. }) {
                video_range_unenforced = source.video_range_type.clone();
            }
        }
        verdict
    };
    if !profile.denied_audio_codecs.is_empty() {
        tracing::info!(
            media.id = item.id,
            audio.codec = source.audio_codec.as_deref().unwrap_or("none"),
            denied = ?profile.denied_audio_codecs,
            applied = profile.denies_audio(source.audio_codec.as_deref()),
            direct_play_block = %direct_play_block,
            "device audio codec deny list consulted"
        );
    }
    // Which post-negotiation override (if any) replaced the negotiator's
    // decision. These fire AFTER `negotiate` and can turn a DirectPlay into a
    // transcode, so a log carrying only the negotiator's reason would name a
    // cause that no longer applies.
    let mut downgrade: Option<&'static str> = None;
    // Connection-aware transcode ceiling (Lace incident, 2026-07-16). A remote
    // client on jellyfin-web's "Auto" quality advertises an effectively-
    // unlimited MaxStreamingBitrate, so `negotiate` leaves the transcode
    // uncapped and the segment encoder targets the source bitrate up to its
    // 12 Mbps ceiling — smooth on a LAN, but a home uplink can't sustain it and
    // the viewer rebuffers. When the client is on a non-private address, clamp
    // the transcode's ceiling to the configured WAN default (min with any lower
    // explicit client pick). LAN clients — and `remote_default_bitrate_bps == 0`
    // — are untouched. The clamp lives on the negotiated Decision so it flows
    // through the registered session to every downstream surface: the HLS ladder
    // filter, the `main` rendition, and the VP9 segment path all read the
    // session's `max_video_bitrate_bps`.
    let remote_ceiling: Option<u64> = (state.remote_default_bitrate_bps > 0
        && req.client_is_remote)
        .then_some(state.remote_default_bitrate_bps);
    let decision = match remote_ceiling {
        Some(ceiling) => decision.clamp_video_bitrate(ceiling),
        None => decision,
    };

    // B75 — native direct-play now authenticates via a capability token (the
    // MediaSource ETag = PlaySessionId, which the SDK forwards as `?tag=`; the
    // stream route validates it against the registered session — see
    // `stream::authorize_media`). So we no longer force a transcode on native
    // clients: `negotiate()` already picked the correct per-DeviceProfile shape
    // (DirectPlay when the client can decode the source, transcode otherwise).
    // Forcing a transcode here (the old B73 workaround) was WRONG for every
    // device that can direct-play — it re-encoded needlessly and, on
    // memory-tight TVs, the extra transcode + HLS churn on seek got the app
    // OOM-killed. Trust the negotiator.

    // Whether the raw `/stream` file genuinely plays in a browser. Computed
    // HERE — ahead of the DirectPlay/force-webm gate — so the B80 hang fix
    // below can consult it; it also drives `SupportsDirectStream` further down.
    let webm_relabel_playable = source_plays_raw_in_browser(
        &source.container,
        source.video_codec.as_deref(),
        source.audio_codec.as_deref(),
        &profile.direct_play_profiles,
    ) && !profile.denies_audio(source.audio_codec.as_deref());
    // B80 (Lace incident, 2026-07-16) — a browser's <video>/MSE cannot demux a
    // raw Matroska container. When the negotiator returned DirectPlay for a
    // Matroska source to a browser that CANNOT play the `video/webm` relabel
    // (e.g. AV1 + AAC, or h264-in-mkv), advertising SupportsDirectStream with NO
    // TranscodingUrl stranded the client: jellyfin-web silently retried
    // PlaybackInfo for ~11 min (Show AI S01E05, av1-in-mkv) before recovering.
    // Downgrade to a transcode so a fallback URL is emitted and
    // SupportsDirectStream resolves false. The generic H.264/AAC target is
    // refined downstream — a Linux-Firefox that can't decode H.264 is redirected
    // onto the VP9 surface by the `force_webm` path below, which now (decision no
    // longer direct) fires. The connection-aware ceiling rides along so a remote
    // viewer keeps its WAN cap.
    // Gate on the Matroska/MKV token, NOT bare "webm". A genuine WebM container
    // (ffprobe "webm") is natively browser-playable and the client lists it
    // explicitly — trust it. The relabel hazard is specifically Matroska (.mkv,
    // ffprobe "matroska" / "matroska,webm"): pharos serves it verbatim relabelled
    // `video/webm`, which only survives when the streams are webm-legal
    // (`webm_relabel_playable`). Lace's av1-in-mkv probed as "matroska,webm".
    let source_is_matroska = {
        let cont = source.container.to_ascii_lowercase();
        cont.contains("matroska") || cont.contains("mkv")
    };
    let decision = if browser_matroska_direct_unplayable(
        is_web_client,
        is_video,
        decision.is_direct(),
        source_is_matroska,
        webm_relabel_playable,
    ) {
        downgrade = Some("browser_matroska");
        Decision::Transcode {
            target_container: "ts".into(),
            target_video_codec: Some("h264".into()),
            target_audio_codec: Some("aac".into()),
            max_video_bitrate_bps: remote_ceiling,
        }
    } else {
        decision
    };

    let qs_stereo_mode = req.stereo_mode;
    let explicit_mono = qs_stereo_mode
        .is_some_and(|m| m.eq_ignore_ascii_case("mono") || m.eq_ignore_ascii_case("mono2d"));
    let explicit_preserve = qs_stereo_mode.is_some_and(|m| m.eq_ignore_ascii_case("preserve"));
    let profile_rejected_3d = is_video
        && probe.video_3d_format.is_some()
        && !pharos_jellyfin_api::device_profile::codec_profile_3d_passes(
            &profile.codec_profiles,
            &source,
        );
    let decision = if is_video
        && probe.video_3d_format.is_some()
        && explicit_mono
        && !matches!(decision, Decision::Transcode { .. })
    {
        downgrade = Some("stereo_mode_mono");
        Decision::Transcode {
            target_container: "ts".into(),
            target_video_codec: Some("h264".into()),
            target_audio_codec: Some("aac".into()),
            max_video_bitrate_bps: remote_ceiling,
        }
    } else {
        decision
    };

    // Resolve which embedded TEXT subs this client can't render externally
    // (its SubtitleProfile lacks an External/Embed/Hls entry for that
    // format) — those must advertise Encode so the client requests the burn
    // transcode instead of trying to render the raw ASS itself (black bars
    // on Android/TV, which never declares ass External). Also incidentally
    // covers image subs (decide_subtitle_delivery always burns those), which
    // is harmless — `build_media_streams_with_subtitles` only consults this
    // set for TEXT tracks. Relocated here (was computed just before the DTO
    // build) so it's available to the downgrade below AND the URL-forward
    // predicate further down, not just the MediaStreams ctx.
    let mut burn_text_indices = std::collections::BTreeSet::new();
    for track in &probe.subtitle_tracks {
        if decide_subtitle_delivery(track.codec.as_deref(), &profile.subtitle_profiles)
            == SubtitleDelivery::Burn
        {
            burn_text_indices.insert(track.stream_index);
        }
    }

    // The subtitle track this playback will act on (explicit pick, else the
    // container's default), used by both the downgrade below and the
    // URL-forward predicate further down — one source of truth (Task 4/5).
    let selected_sub =
        resolve_selected_subtitle(explicit_subtitle_index, probe, &profile.subtitle_profiles);
    // Whether a client asked for "no subtitle" by sending -1 or by OMITTING
    // the field is invisible in the access log — PlaybackInfo carries it in
    // the body — and the two mean opposite things here: -1 resolves to no
    // subtitle, while an absent field falls back to the container's
    // default-disposition track. For an image subtitle that fallback is a
    // burn, which cannot then be turned off client-side, so a client that
    // omits the field instead of sending -1 can never remove its subtitles.
    // Log what actually arrived so that distinction is answerable from a
    // single reproduction.
    tracing::info!(
        media.id = item.id,
        subtitle.requested = ?explicit_subtitle_index,
        subtitle.resolved = ?selected_sub.map(|t| t.stream_index),
        subtitle.codec = ?selected_sub.and_then(|t| t.codec.as_deref()),
        subtitle.delivery = ?selected_sub.map(|t| decide_subtitle_delivery(
            t.codec.as_deref(),
            &profile.subtitle_profiles
        )),
        "playbackinfo subtitle selection"
    );

    // No-gap fix: a client that would DIRECT-PLAY the file but selects (or
    // defaults to) a subtitle it can't render externally has no video stream
    // to burn into. Mirror the Matroska downgrade above so a TranscodingUrl
    // is emitted (carrying the burn index — see the forward predicate below)
    // and SupportsDirectStream resolves false, instead of silently dropping
    // the subtitle (today's bug for a DirectPlay client selecting a
    // PGS/default-image sub).
    let decision =
        if subtitle_selection_forces_transcode(&decision, selected_sub, &profile.subtitle_profiles)
        {
            downgrade = Some("subtitle_burn");
            Decision::Transcode {
                target_container: "ts".into(),
                target_video_codec: Some("h264".into()),
                target_audio_codec: Some("aac".into()),
                max_video_bitrate_bps: remote_ceiling,
            }
        } else {
            decision
        };

    // The request's own switches (a retry after a direct-play failure) come
    // last so they override every negotiated outcome above.
    // Re-negotiated from the client's own profile with direct play cleared,
    // like the remote-source path, so the target is one it can decode.
    let pre_switch_decision = decision.clone();
    let decision = {
        let forced_transcode = || {
            let mut forced = profile.clone();
            forced.direct_play_profiles.clear();
            let (d, _) = negotiate_explained(&forced, &source, &server_codecs);
            match remote_ceiling {
                Some(ceiling) => d.clamp_video_bitrate(ceiling),
                None => d,
            }
        };
        let adjusted = apply_playback_switches(decision, &switches, is_video, forced_transcode);
        if adjusted != pre_switch_decision {
            downgrade = Some("client_switch");
        }
        adjusted
    };

    let burning_sub_per_eye = is_video
        && probe
            .video_3d_format
            .is_some_and(|f| f.is_packed_sbs_or_tab())
        && selected_sub.is_some_and(|t| {
            is_image_subtitle_codec(t.codec.as_deref().unwrap_or(""))
                || burn_text_indices.contains(&t.stream_index)
        });
    let stereo_output = if !explicit_preserve && (explicit_mono || profile_rejected_3d) {
        pharos_transcode::StereoOutput::Mono2D
    } else {
        pharos_transcode::StereoOutput::Preserve
    };

    let direct_play = decision.is_direct() && switches.direct_play;
    // B77 — `SupportsDirectStream` promises the raw `/stream` file is playable
    // AS-IS: pharos's `/stream` serves the source verbatim (no container remux,
    // no audio transcode — the only massaging is `deliver_stream`'s webm
    // re-label). So it's true ONLY when the file genuinely plays for THIS
    // client: a real DirectPlay, OR a webm-codec Matroska (VP8/9/AV1 + Opus/
    // Vorbis) that the re-label serves as `video/webm` to a client that decodes
    // VP9. A VideoRemux/AudioRemux of anything else — the h264+eac3/ac3/dts
    // Matroska that dominates the library — is NOT playable raw (browser rejects
    // the container/audio), so it must transcode via the TranscodingUrl below.
    // Native players that CAN play such files declare it in their profile →
    // DirectPlay, unaffected.
    // Judged on the pre-switch decision: a DirectPlay forced into a transcode
    // by EnableDirectPlay=false still has a raw stream to offer. A forbidden
    // video copy takes the raw stream away (Jellyfin: `!enableDirectStream ||
    // !allowVideoStreamCopy`).
    // A split film's raw file is its first part only (V1155).
    let supports_direct_stream = switches.direct_stream
        && switches.video_copy
        && split_parts.is_none()
        && (pre_switch_decision.is_direct()
            || (matches!(
                pre_switch_decision,
                Decision::VideoRemux { .. } | Decision::AudioRemux { .. }
            ) && webm_relabel_playable));

    Negotiated {
        source,
        container,
        is_video,
        server_codecs,
        decision,
        pre_switch_decision,
        direct_play_block,
        downgrade,
        remote_ceiling,
        burn_text_indices,
        selected_subtitle: selected_sub.cloned(),
        explicit_mono,
        explicit_preserve,
        profile_rejected_3d,
        video_range_unenforced,
        burning_sub_per_eye,
        stereo_output,
        direct_play,
        supports_direct_stream,
        webm_relabel_playable,
    }
}

/// Why a 3D source got the `stereo_output` it did, for the
/// `pharos_playback_3d_decision_total{reason}` label and its log line. A
/// bounded set: `passthrough`, `stereo_mode_query`, `profile_condition`,
/// `burn_per_eye`, else the downgrade or direct-play-block label.
pub fn playback_3d_reason(
    direct_play: bool,
    supports_direct_stream: bool,
    explicit_mono: bool,
    profile_rejected_3d: bool,
    burning_sub_per_eye: bool,
    downgrade: Option<&'static str>,
    direct_play_block: &'static str,
) -> &'static str {
    if direct_play || supports_direct_stream {
        "passthrough"
    } else if explicit_mono {
        "stereo_mode_query"
    } else if profile_rejected_3d {
        "profile_condition"
    } else if burning_sub_per_eye {
        "burn_per_eye"
    } else {
        downgrade.unwrap_or(direct_play_block)
    }
}

/// Registers the negotiated decision under `play_session_id`. For a
/// transcode the HLS segment generator reads it back to honour the target
/// codec / container / bitrate cap. For DIRECT PLAY we still register it
/// (B75): the session doubles as the stream capability token — the ETag we
/// hand back equals this PlaySessionId, and `stream::authorize_media`
/// authorizes the tokenless native `/stream?tag=…` request by resolving the
/// session and matching its media_id, then checking that `holder`'s token
/// is live and that they may still see the item (B256). The registry
/// self-expires (5 min in-memory / 6 h durable), so no-op direct-play entries
/// don't accumulate.
pub async fn register(
    state: &AppState,
    play_session_id: &str,
    media_id: u64,
    negotiated: &Negotiated,
    probe: &pharos_core::MediaProbe,
    holder: pharos_core::SessionHolder,
) {
    match state
        .transcode_sessions
        .insert(
            pharos_core::PlaySessionId::from(play_session_id),
            crate::transcode_sessions::TranscodeSession {
                media_id,
                decision: negotiated.decision.clone(),
                source_probe: probe.clone(),
                // The ONLY request on which this client's SubtitleProfiles are
                // visible. Every segment URL it fetches later carries a bare
                // SubtitleStreamIndex, so without carrying the verdict forward
                // the segment handler must assume each one needs burning.
                burn_subtitle_indices: negotiated.burn_text_indices.clone(),
                holder,
            },
        )
        .await
    {
        Ok(evicted) => {
            // Stop what the cap ended the way a client's Stopped report does,
            // minus the final UserData write: its prefetch and its record go.
            for id in &evicted {
                state.prefetch_tasks.cancel(id.as_str());
                if let Err(e) = state
                    .sessions
                    .apply(crate::sessions::SessionEvent::Stopped {
                        session_id: id.clone(),
                    })
                    .await
                {
                    tracing::warn!(
                        error = %e,
                        "play session cap: ending the evicted session's record failed"
                    );
                }
            }
        }
        Err(e) => tracing::warn!(
            error = %e,
            "transcode session insert failed; this playback has no stream capability"
        ),
    }
}

/// Fold the request's switches into the negotiated decision. A remux that
/// copies a stream the client forbids copying becomes a full transcode, and a
/// DirectPlay with direct play disabled becomes one too, so a TranscodingUrl
/// exists whenever direct play is off. `AudioRemux` copies the VIDEO and
/// re-encodes the audio for a video source, so it is gated on `video_copy`;
/// only an audio-only item is gated on `audio_copy`. `forced_transcode`
/// supplies the target (re-negotiated from the client's own profile). When
/// transcoding is also disabled the decision is left alone: the caller then
/// strips the TranscodingUrl and reports the source as unplayable.
fn apply_playback_switches(
    decision: Decision,
    sw: &PlaybackSwitches,
    is_video: bool,
    forced_transcode: impl Fn() -> Decision,
) -> Decision {
    if !sw.transcoding {
        return decision;
    }
    match decision {
        Decision::VideoRemux { .. } if !sw.video_copy => forced_transcode(),
        Decision::AudioRemux { .. } if is_video && !sw.video_copy => forced_transcode(),
        Decision::AudioRemux { .. } if !is_video && !sw.audio_copy => forced_transcode(),
        Decision::DirectPlay if !sw.direct_play => forced_transcode(),
        other => other,
    }
}

/// B80 (Lace incident, 2026-07-16) — true when the negotiator picked DirectPlay
/// for a Matroska-container source destined for a BROWSER that cannot actually
/// play the re-labelled `video/webm` stream (`webm_relabel_playable` is false).
/// A browser's `<video>`/MSE has no Matroska demuxer, so pharos would advertise
/// `SupportsDirectStream` with NO `TranscodingUrl` fallback and strand the
/// client (av1-in-mkv → ~11 min PlaybackInfo-retry hang). When this holds,
/// `playback_info` downgrades the decision to a transcode so a fallback URL is
/// emitted. Native players declare real Matroska support in their profile →
/// their DirectPlay is genuine and `is_web_client` is false, so they are
/// unaffected.
fn browser_matroska_direct_unplayable(
    is_web_client: bool,
    is_video: bool,
    decision_is_direct: bool,
    source_is_matroska: bool,
    webm_relabel_playable: bool,
) -> bool {
    is_web_client && is_video && decision_is_direct && source_is_matroska && !webm_relabel_playable
}

/// Whether a browser can play pharos's `/stream` response for this source
/// AS-IS — the file served verbatim, no remux.
///
/// A browser demuxes the CONTAINER before it ever reaches a codec, so this is
/// a container test first. Only WebM qualifies: Firefox ships no Matroska
/// demuxer, and reads the EBML DocType to decide — `matroska` is refused
/// outright however WebM-legal the streams inside are.
///
/// This was previously computed as "Matroska whose codecs are WebM-legal,
/// relabelled `video/webm`" (B107). That premise inverted the check it fed:
/// an av1+opus .mkv satisfied it, so [`browser_matroska_direct_unplayable`]
/// concluded the file was fine and skipped the downgrade — leaving the browser
/// with a Matroska it could not open and no `TranscodingUrl` to fall back to.
///
/// The codec tests remain: a genuine WebM can still carry streams a given
/// client does not decode.
fn source_plays_raw_in_browser(
    container: &str,
    video_codec: Option<&str>,
    audio_codec: Option<&str>,
    direct_play_profiles: &[pharos_jellyfin_api::device_profile::DirectPlayProfile],
) -> bool {
    if !container.eq_ignore_ascii_case("webm") {
        return false;
    }
    // The client must list a webm DirectPlayProfile covering the source's video
    // codec — a WebM container it cannot decode is no more playable than a
    // Matroska.
    let client_plays_webm_video = video_codec.is_some_and(|sv| {
        direct_play_profiles.iter().any(|p| {
            (p.kind.is_empty() || p.kind.eq_ignore_ascii_case("Video"))
                && p.container
                    .split(',')
                    .any(|c| c.trim().eq_ignore_ascii_case("webm"))
                && p.video_codec
                    .split(',')
                    .any(|c| c.trim().eq_ignore_ascii_case(sv))
        })
    });
    let v = video_codec.map(|c| c.to_ascii_lowercase());
    let a = audio_codec.map(|c| c.to_ascii_lowercase());
    client_plays_webm_video
        && matches!(
            v.as_deref(),
            Some("vp9" | "vp09" | "vp8" | "vp08" | "av1" | "av01")
        )
        // Audio must be webm-legal too, else the browser plays video but
        // cannot decode the audio track.
        && matches!(a.as_deref(), Some("opus" | "vorbis") | None)
}

/// The subtitle track this playback will act on: the client's explicit pick
/// (query `SubtitleStreamIndex` or body), else the default-disposition
/// track, else `None`. `-1` (off) → `None`. The single source of truth for
/// "which subtitle applies", shared by the burn-forward predicate and the
/// DirectPlay-downgrade predicate below.
pub(crate) fn resolve_selected_subtitle<'a>(
    explicit_index: Option<i64>,
    probe: &'a pharos_core::MediaProbe,
    client_profiles: &[SubtitleProfileDto],
) -> Option<&'a pharos_core::SubtitleTrack> {
    match explicit_index {
        Some(n) if n >= 0 => probe
            .subtitle_tracks
            .iter()
            .find(|t| i64::from(t.stream_index) == n),
        Some(_) => None, // explicit "off" (-1, or any other negative)
        // NOBODY ASKED FOR THIS TRACK, so it may not commit the server to
        // re-encoding every frame of the film (T110).
        //
        // The container's default-disposition track used to be taken
        // unconditionally. When that track is PGS or VOBSUB there is no
        // external delivery — the only way to show it is to burn it into the
        // video — so an unasked-for default silently turned a cheap stream
        // into a full overlay encode. Measured 2026-08-05: burned segments
        // take p50 5380 ms / p95 10995 ms to produce 6000 ms of video, i.e.
        // ABOVE REALTIME at the p95. The encoder cannot stay ahead of the
        // player, the buffer starves, the client restarts, and each restart
        // asks for more burns. Observed live 2026-08-16 on Film S:
        // `subtitle.requested: None, subtitle.resolved: Some(2),
        // subtitle.codec: hdmv_pgs_subtitle, subtitle.delivery: Burn`, with
        // one segment re-fetched 118 times and 22 play sessions in 15
        // minutes.
        //
        // An EXPLICIT pick is still honoured exactly as before: burning a
        // subtitle someone actually chose is the correct answer to an
        // expensive request, and this deliberately does not second-guess it.
        // What changes is only the DEFAULT, where the cost is invisible to
        // the person paying it — and where, as the selection log has noted
        // since B104, a client that omits the field rather than sending -1
        // cannot turn the resulting burn off again.
        None => probe
            .subtitle_tracks
            .iter()
            .find(|t| t.is_default)
            .and_then(|default| accept_or_decline_uninvited_burn(default, probe, client_profiles)),
    }
}

/// Veto an AUTO-PICKED (never explicitly requested) subtitle candidate that
/// would silently commit the session to an above-realtime full-frame burn
/// (T110) — substituting a cheap externally-renderable track when one exists,
/// declining outright otherwise. Applies regardless of which ladder produced
/// the candidate: `resolve_selected_subtitle`'s container-default lookup and
/// `stream_select::default_subtitle_stream_index`'s mode/language-aware ladder
/// (P12) both hand it a track nobody asked for, and the cost is the same
/// either way. The two ladders disagreeing about the ANSWER is fine — the P12
/// ladder can legitimately prefer a language-matched track the container
/// never flagged default — but disagreeing about whether THAT answer is
/// affordable is what let `DefaultSubtitleStreamIndex` point at a track the
/// burn path had separately and silently declined to ever deliver, live
/// 2026-08-29 (Show U: the container-default PGS track was a sparse
/// signs/songs-only track; jellyfin-web pre-selected it as "English" and
/// nothing rendered during dialogue).
///
/// Narrowed twice, on purpose.
///
/// By CODEC: an unrenderable ASS default is still burned (B104 — converting it
/// to WebVTT drops positioning and fonts, which the Android app rendered as
/// black bars), because a text overlay costs a fraction of a bitmap one.
/// PGS/VOBSUB is the measured expensive case and is what this rule is about.
///
/// By FORCED: a forced track is not a convenience, it is dialogue the viewer
/// cannot follow without — B44's Film BA case, where the alien-language sections played
/// unsubtitled until the default+forced PGS was baked in. Its cost is the
/// price of an intelligible film, and it is bounded: forced tracks cover a
/// fraction of the runtime. What this declines is the UNFORCED image
/// candidate — a full-frame burn for the whole film that nobody asked for.
pub(crate) fn accept_or_decline_uninvited_burn<'a>(
    candidate: &'a pharos_core::SubtitleTrack,
    probe: &'a pharos_core::MediaProbe,
    client_profiles: &[SubtitleProfileDto],
) -> Option<&'a pharos_core::SubtitleTrack> {
    let burns = |t: &pharos_core::SubtitleTrack| {
        decide_subtitle_delivery(t.codec.as_deref(), client_profiles) == SubtitleDelivery::Burn
    };
    let image_candidate = candidate
        .codec
        .as_deref()
        .is_some_and(is_image_subtitle_codec);
    if !(image_candidate && !candidate.is_forced && burns(candidate)) {
        // "accepted", not "default": the P12 ladder can hand this a
        // language/mode-preferred candidate the container never flagged
        // default (Smart mode, an English-preferred track over a
        // non-preferred one), and this rule doesn't care which ladder
        // produced it — only whether the pick is affordable as-is.
        metrics::counter!(
            "pharos_subtitle_autoselect_total",
            "outcome" => "accepted",
        )
        .increment(1);
        return Some(candidate);
    }
    // The candidate would cost a full-frame re-encode nobody asked for.
    // Prefer any track this client can render itself: the viewer still gets
    // subtitles, at no encoding cost at all.
    if let Some(cheap) = probe.subtitle_tracks.iter().find(|t| !burns(t)) {
        metrics::counter!(
            "pharos_subtitle_autoselect_total",
            "outcome" => "swapped_to_external",
        )
        .increment(1);
        tracing::info!(
            subtitle.declined = candidate.stream_index,
            subtitle.declined_codec = ?candidate.codec,
            subtitle.chosen = cheap.stream_index,
            subtitle.chosen_codec = ?cheap.codec,
            "the auto-picked subtitle could only be delivered by burning it in; \
             auto-selected an externally-renderable track instead"
        );
        return Some(cheap);
    }
    metrics::counter!(
        "pharos_subtitle_autoselect_total",
        "outcome" => "declined_burn",
    )
    .increment(1);
    tracing::info!(
        subtitle.index = candidate.stream_index,
        subtitle.codec = ?candidate.codec,
        "declining to auto-select an image subtitle nobody requested: \
         its only delivery is a full-frame burn, measured above \
         realtime, and the client cannot turn it off"
    );
    None
}

/// A DirectPlay/DirectStream decision must downgrade to Transcode when the
/// selected subtitle will be BURNED (no external stream to render it): a
/// text/ASS sub the client has no external SubtitleProfile for, or any image
/// sub (PGS/VOBSUB — never externally renderable). Mirrors the B80/V40
/// Matroska downgrade so a `TranscodingUrl` is emitted and
/// `SupportsDirectStream` resolves false instead of silently dropping the
/// subtitle.
fn subtitle_selection_forces_transcode(
    decision: &Decision,
    selected: Option<&pharos_core::SubtitleTrack>,
    client_profiles: &[SubtitleProfileDto],
) -> bool {
    decision.is_direct()
        && selected.is_some_and(|t| {
            decide_subtitle_delivery(t.codec.as_deref(), client_profiles) == SubtitleDelivery::Burn
        })
}

/// What the transcoding URL is built from, beyond the negotiated outcome.
pub struct DeliveryRequest<'a> {
    pub profile: &'a DeviceProfile,
    pub probe: &'a pharos_core::MediaProbe,
    /// The item (or edition) as the client names it on the wire.
    pub item_wire_id: &'a str,
    /// `&MediaSourceId=…` when one edition was asked for, else empty.
    pub source_param: &'a str,
    /// The URL's credential: the transcode routes accept it in place of a
    /// token (V1050), so no URL carries one.
    pub play_session_id: &'a str,
    pub audio_stream_index: Option<String>,
    pub explicit_subtitle_index: Option<i64>,
    pub is_web_client: bool,
    pub resume_ticks: u64,
    /// The client's `EnableTranscoding` switch.
    pub transcoding: bool,
    /// The client's `MaxWidth` / `MaxHeight`, baked into the master URL so the
    /// cold-start prewarm and the segment requests plan the same output size.
    pub size_limits: pharos_transcode::SizeLimits,
}

/// The segmented delivery of a negotiated playback.
pub struct Delivery {
    /// The codecs the HLS master advertises, best first.
    pub renditions: Vec<String>,
    /// `None` when the playback is direct, or transcoding is switched off.
    pub transcoding_url: Option<String>,
}

/// Builds the URL a non-direct playback streams from: the unified HLS
/// master for video, the universal endpoint for audio.
pub fn delivery(n: &Negotiated, r: &DeliveryRequest<'_>) -> Delivery {
    let renditions = renditions(n, r);
    let (id, psid, source) = (r.item_wire_id, r.play_session_id, r.source_param);
    let transcoding_url = match (&n.decision, n.is_video) {
        (Decision::DirectPlay, _) => None,
        // Every video transcode or remux drives the unified master. It
        // advertises each rendition the client can decode and lets the
        // client's own isTypeSupported pick; the PlaySessionId lets the
        // segment handlers reuse the registered decision (P9, B77).
        (Decision::VideoRemux { .. }, _) | (_, true) => Some(format!(
            "/videos/{id}/master.m3u8?PlaySessionId={psid}&renditions={}{}{}{}{source}",
            renditions.join(","),
            stream_selection(n, r),
            video_bitrate_param(n),
            r.size_limits.query_suffix(),
        )),
        (_, false) => Some(format!(
            "/audio/{id}/universal?PlaySessionId={psid}{source}"
        )),
    }
    .filter(|_| r.transcoding)
    // B74 — native players replay the URL verbatim and do not client-seek a
    // transcode, so a resume rides it as StartTimeTicks. The web client
    // seeks in the full VOD playlist itself.
    .map(|u| {
        if !r.is_web_client && r.resume_ticks > 0 {
            format!("{u}&StartTimeTicks={}", r.resume_ticks)
        } else {
            u
        }
    });
    Delivery {
        renditions,
        transcoding_url,
    }
}

/// Every codec the client's profile decodes and this server encodes, ranked
/// hardware-first, limited to the codecs with a segment surface (h264 on
/// mpegts, vp9 on fMP4). A browser filters them by isTypeSupported; a native
/// player takes the first rung, so it gets only the negotiated target.
fn renditions(n: &Negotiated, r: &DeliveryRequest<'_>) -> Vec<String> {
    let client_video_csv = r
        .profile
        .transcoding_profiles
        .iter()
        .filter(|t| t.kind.is_empty() || t.kind.eq_ignore_ascii_case("Video"))
        .map(|t| t.video_codec.as_str())
        .collect::<Vec<_>>()
        .join(",");
    let mut renditions: Vec<String> =
        pharos_jellyfin_api::device_profile::rank_transcode_video_codecs(
            &client_video_csv,
            &n.server_codecs,
        )
        .into_iter()
        .filter(|c| matches!(c.as_str(), "h264" | "vp9"))
        .collect();
    if renditions.is_empty() {
        renditions.push("h264".to_string());
    }
    if !r.is_web_client {
        let one = match &n.decision {
            Decision::Transcode {
                target_video_codec: Some(c),
                ..
            } if c.to_ascii_lowercase().starts_with("vp") => "vp9".to_string(),
            _ => renditions.swap_remove(0),
        };
        renditions = vec![one];
    }
    renditions
}

/// The track picks the segment handler needs: the audio stream, the subtitle
/// to burn, and the 3D output.
///
/// A subtitle rides the URL only when it must burn: an image subtitle, or a
/// text one this client cannot render (`burn_text_indices`). Burning a text
/// subtitle the client renders itself is redundant and slow: a burned segment
/// seeks by decoding from the start. `-1` ("off") always rides, since it
/// clears an earlier burn.
fn stream_selection(n: &Negotiated, r: &DeliveryRequest<'_>) -> String {
    let mut s = String::new();
    let mut push = |key: &str, val: &dyn std::fmt::Display| {
        s.push_str(&format!("&{key}={val}"));
    };
    if let Some(v) = &r.audio_stream_index {
        push("AudioStreamIndex", v);
    }
    if r.explicit_subtitle_index == Some(-1) {
        push("SubtitleStreamIndex", &-1);
    } else if let Some(t) = &n.selected_subtitle {
        if is_image_subtitle_codec(t.codec.as_deref().unwrap_or(""))
            || n.burn_text_indices.contains(&t.stream_index)
        {
            push("SubtitleStreamIndex", &t.stream_index);
        }
    }
    if n.is_video && r.probe.video_3d_format.is_some() {
        if n.stereo_output == pharos_transcode::StereoOutput::Mono2D {
            push("StereoMode", &"mono");
        } else if n.explicit_preserve {
            push("StereoMode", &"preserve");
        }
    }
    s
}

/// The negotiated video bitrate ceiling as `&VideoBitrate=` (Lace incident):
/// it rides master → variant → segment, so the cap holds on a cold segment
/// whose session was collected, and on the remux paths, whose decision
/// carries no cap. A remote remux re-encodes video too (B45), so it carries
/// the connection ceiling.
fn video_bitrate_param(n: &Negotiated) -> String {
    match &n.decision {
        Decision::Transcode {
            max_video_bitrate_bps: Some(bps),
            ..
        } => Some(*bps),
        _ => n.remote_ceiling,
    }
    .map(|b| format!("&VideoBitrate={b}"))
    .unwrap_or_default()
}

/// The audio track a playback starts on, and the user's track preferences
/// and play state it was picked from.
pub struct AudioPick {
    pub index: Option<u32>,
    pub language: Option<String>,
    pub prefs: crate::api::jellyfin::user_prefs::TrackPreference,
    pub remembered: pharos_core::UserItemData,
}

/// Picks the audio track `user` starts `item` on: `explicit` (a track the
/// client named on this request), else the one they played it with last,
/// else their language rules.
pub async fn pick_audio(
    state: &AppState,
    user: &pharos_core::User,
    item: &MediaItem,
    streams: &[pharos_jellyfin_api::dto::MediaStreamDto],
    explicit: Option<u32>,
) -> AudioPick {
    // Which tracks THIS user should start on. Until now this was "the first
    // audio stream in the container", which is how Film AL — three Ukrainian
    // tracks ahead of its English DTS-HD MA — played in Ukrainian every time.
    // The preferences have been stored per user since `/Users/{id}/Configuration`
    // existed; this is the first thing to read them.
    let track_prefs = crate::api::jellyfin::user_prefs::track_preference(state, user.id).await;
    let audio_facts = crate::api::jellyfin::stream_select::audio_facts(streams);
    // An `OriginalLanguage` preference ranks by the language THIS title was
    // made in — Japanese for anime, English for Hollywood, from one setting.
    // Unenriched items record none and degrade to the container's own order
    // rather than to a wrong language.
    //
    // An EPISODE never carries one: providers report the original language on
    // the series record, because it is a property of the show. Measured on the
    // live library mid-backfill — 9 of 32 refreshed movies had one, 0 of 14
    // episodes — so resolving TV through its series is not an optimisation,
    // it is the difference between the preference working for anime and not
    // working at all.
    let original_language = match item.metadata.original_language.clone() {
        Some(l) => Some(l),
        None if track_prefs.audio_original_language => match &item.series {
            Some(series) => state
                .stores
                .series_metadata_by_keys(&[series.series_key().to_string()])
                .await
                .ok()
                .and_then(|m| {
                    m.get(series.series_key())
                        .and_then(|s| s.original_language.clone())
                }),
            None => None,
        },
        None => None,
    };
    let audio_languages = track_prefs.audio_languages_for(original_language.as_deref());
    // What this user last played THIS item with. An explicit choice they made
    // outranks any language rule — it is the same person, saying the same
    // thing, one session later.
    let mut remembered = state
        .stores
        .get_user_data(user.id, item.id)
        .await
        .unwrap_or_default();
    // A remembered index names a stream of the primary file; in a version
    // (another file, another stream layout) it can name another track.
    if item.edition.is_some() {
        remembered.audio_stream_index = None;
        remembered.subtitle_stream_index = None;
    }
    let remembered_audio = track_prefs
        .remember_audio
        .then_some(remembered.audio_stream_index)
        .flatten()
        .and_then(|i| u32::try_from(i).ok())
        .filter(|idx| audio_facts.iter().any(|s| s.index == *idx));
    let explicit_audio_ok = explicit.filter(|idx| audio_facts.iter().any(|s| s.index == *idx));
    let rule_audio_index = crate::api::jellyfin::stream_select::default_audio_stream_index(
        &audio_facts,
        &audio_languages,
        track_prefs.prefer_default_audio_track,
    );
    let (default_audio_stream_index, audio_pick_source) =
        resolve_audio_pick(explicit_audio_ok, remembered_audio, rule_audio_index);
    // The language of the track actually chosen — Smart subtitles turn on
    // precisely when this is NOT a language the user reads.
    let chosen_audio_language: Option<String> = default_audio_stream_index.and_then(|idx| {
        streams
            .iter()
            .find(|s| s.kind == "Audio" && s.index == idx)
            .and_then(|s| s.language.clone())
    });
    // ODD: the audio pick was invisible — diagnosing a wrong-language start
    // (A viewer's Show U playing English, 2026-07-26) took a hand DB read.
    // Record the decision: the resolved preference inputs, EVERY candidate with
    // its language + default flag (the flag is what beats a language preference
    // when prefer_default is on — the Film AL / Show U trap), and the chosen
    // index/language/source. A surprising pick is now one query away from its
    // reason. `chosen_index` uses -1 for "no audio" so the field stays numeric.
    {
        let chosen_is_default = default_audio_stream_index
            .and_then(|idx| audio_facts.iter().find(|s| s.index == idx))
            .is_some_and(|s| s.is_default);
        let candidates = audio_facts
            .iter()
            .map(|s| {
                format!(
                    "{}:{}{}",
                    s.index,
                    s.language.as_deref().unwrap_or("und"),
                    if s.is_default { ":default" } else { "" }
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        tracing::info!(
            user.id = %user.id,
            item.id = %item.id,
            pick_source = audio_pick_source,
            prefer_default_track = track_prefs.prefer_default_audio_track,
            original_language_pref = track_prefs.audio_original_language,
            original_language = original_language.as_deref().unwrap_or(""),
            preferred_languages = %audio_languages.join(","),
            candidates = %candidates,
            chosen_index = default_audio_stream_index.map(i64::from).unwrap_or(-1),
            chosen_language = chosen_audio_language.as_deref().unwrap_or(""),
            chosen_is_default,
            "playback: audio track selected"
        );
    }
    AudioPick {
        index: default_audio_stream_index,
        language: chosen_audio_language,
        prefs: track_prefs,
        remembered,
    }
}

/// The audio track a player starts a file on: the default-flagged one, else
/// the first.
fn first_audio(streams: &[pharos_jellyfin_api::dto::MediaStreamDto]) -> Option<u32> {
    let mut audio = streams.iter().filter(|s| s.kind == "Audio");
    audio
        .clone()
        .find(|s| s.is_default)
        .or_else(|| audio.next())
        .map(|s| s.index)
}

/// Picks the subtitle `item` starts with for the audio in `audio`; `None` is
/// no subtitle. `explicit` is the client's pick on this request (`-1` off).
pub fn pick_subtitle(
    audio: &AudioPick,
    streams: &[pharos_jellyfin_api::dto::MediaStreamDto],
    probe: &pharos_core::MediaProbe,
    subtitle_profiles: &[SubtitleProfileDto],
    explicit: Option<i64>,
) -> Option<u32> {
    // P12 — `DefaultSubtitleStreamIndex` resolution priority:
    //   0. What the CLIENT asked for on THIS request, if it asked at all.
    //   1. Any subtitle track flagged `is_default` (probed from the
    //      container's disposition bits).
    //   2. The first English-language track.
    //   3. The first subtitle track of any kind.
    // None when no subtitle tracks exist — client renders no default.
    //
    // Step 0 is what makes "subtitles off" STICK. This field is not a
    // description of the file, it is pharos telling the client what to select
    // for this playback — so computing it from the container alone answered a
    // client that had just sent `SubtitleStreamIndex=-1` with "the default
    // subtitle is 2", and the client obediently selected it. Measured live
    // 2026-07-26, one second apart:
    //
    //   04:06:29  requested Some(-1) → resolved None
    //   04:06:30  requested Some(2)  → resolved Some(2), delivery Burn
    //
    // The re-selection was the client obeying this field, which is why turning
    // subtitles off appeared to do nothing on Google TV. An explicit pick is
    // echoed back; explicit off answers None; only an absent field falls
    // through to the container's own preference (so B44's forced-alien-language track
    // and B104's default ASS are untouched).
    match explicit {
        // The client stated a choice on THIS request — echo it back, including
        // "off" (-1 → None). Filtered against the advertised streams so a stale
        // index cannot name a track that isn't there.
        Some(n) => u32::try_from(n).ok().filter(|idx| {
            streams
                .iter()
                .any(|s| s.kind == "Subtitle" && s.index == *idx)
        }),
        // No choice stated on this request — a remembered one still counts,
        // including a remembered -1 (subtitles OFF), which is a choice like
        // any other and must survive a resume.
        None if audio.prefs.remember_subtitle
            && audio.remembered.subtitle_stream_index.is_some() =>
        {
            audio
                .remembered
                .subtitle_stream_index
                .filter(|i| *i >= 0)
                .and_then(|i| u32::try_from(i).ok())
                .filter(|idx| {
                    streams
                        .iter()
                        .any(|s| s.kind == "Subtitle" && s.index == *idx)
                })
        }
        // Nothing chosen, ever — the user's SubtitleMode decides.
        //
        // This replaces a hand-rolled ladder (default flag → first English →
        // ANY subtitle track). Its last rung is the reason subtitles appeared
        // on titles nobody asked them for: a container with one non-default,
        // non-forced track always selected it. Jellyfin's Default mode picks
        // only external/default/forced, so B44's forced track and B104's
        // default ASS still select — and still burn — while an incidental
        // track no longer switches itself on.
        // The ladder above is a faithful port of upstream's mode/language
        // selection (stream_select.rs) and knows nothing of burn cost — it
        // picked the container-default PGS "Signs & Songs" track for Code
        // Geass live 2026-08-29 while the burn path separately declined to
        // ever deliver it, so jellyfin-web pre-selected a subtitle that
        // rendered nothing during dialogue. Route its answer through the same
        // veto `resolve_selected_subtitle` applies to the container default,
        // so `DefaultSubtitleStreamIndex` never points at a track this
        // session will not actually burn.
        None => crate::api::jellyfin::stream_select::default_subtitle_stream_index(
            &crate::api::jellyfin::stream_select::subtitle_facts(streams),
            &audio.prefs.subtitle_languages,
            audio.prefs.subtitle_mode,
            audio.language.as_deref(),
        )
        .and_then(|idx| probe.subtitle_tracks.iter().find(|t| t.stream_index == idx))
        .and_then(|candidate| accept_or_decline_uninvited_burn(candidate, probe, subtitle_profiles))
        .map(|t| t.stream_index),
    }
}

/// Append `&AudioStreamIndex={idx}` to a transcode URL that doesn't already
/// carry one, so the server's auto-selected audio track (the same absolute
/// index that becomes `DefaultAudioStreamIndex`) rides the initial URL. Absent
/// URL or absent selection → unchanged; an already-present index (the client
/// named a track on this request) → unchanged, so it is idempotent with a
/// manual switch. The check is a plain substring: the param name never appears
/// elsewhere in these URLs.
pub fn ensure_audio_index_param(url: Option<String>, idx: Option<u32>) -> Option<String> {
    match (url, idx) {
        (Some(u), Some(i)) if !u.contains("AudioStreamIndex") => {
            Some(format!("{u}&AudioStreamIndex={i}"))
        }
        (other, _) => other,
    }
}

/// Resolve the audio track to start on and NAME why, in priority order: a track
/// the client explicitly requested, then one it played before (remembered),
/// then the language/default-flag rule. The `&'static str` source is the
/// decision's reason for the ODD selection log — a wrong-language start is
/// otherwise indistinguishable between "the rule chose it" and "a remembered
/// pick did" without reading the DB by hand.
pub(crate) fn resolve_audio_pick(
    explicit_ok: Option<u32>,
    remembered: Option<u32>,
    rule: Option<u32>,
) -> (Option<u32>, &'static str) {
    if let Some(i) = explicit_ok {
        (Some(i), "explicit")
    } else if let Some(i) = remembered {
        (Some(i), "remembered")
    } else {
        (rule, "rule")
    }
}

/// What a native player says it can play (`Playback.negotiate`).
#[derive(Debug, Default, Clone)]
pub struct Capabilities {
    pub video_codecs: Vec<String>,
    pub audio_codecs: Vec<String>,
    pub containers: Vec<String>,
    /// Bits per second; 0 is "no limit".
    pub max_bitrate: u64,
    /// Whether the display shows HDR. False adds a `VideoRangeType` condition
    /// that admits SDR only, so an HDR source is never copied to the player.
    pub hdr: bool,
    /// Image subtitle codecs the player draws from extracted PNGs (spec 019).
    pub image_subtitles: Vec<String>,
}

impl Capabilities {
    /// The capabilities as the profile the negotiator reads: one direct-play
    /// and one transcoding profile per media type. The transcoding profiles
    /// name the HLS surfaces pharos serves: video on the master, audio on the
    /// universal endpoint. The declared image subtitles render externally only
    /// when `draws_images` (this server extracts them); otherwise they burn.
    pub fn profile(&self, draws_images: bool) -> DeviceProfile {
        use pharos_jellyfin_api::device_profile::{
            CodecProfileDto, DirectPlayProfile, ProfileCondition, SubtitleProfileDto,
            TranscodingProfile,
        };
        let csv = |v: &[String]| v.join(",");
        let (containers, video, audio) = (
            csv(&self.containers),
            csv(&self.video_codecs),
            csv(&self.audio_codecs),
        );
        let codec_profiles = if self.hdr {
            Vec::new()
        } else {
            vec![CodecProfileDto {
                kind: "Video".into(),
                codec: String::new(),
                conditions: vec![ProfileCondition {
                    condition: "Equals".into(),
                    property: "VideoRangeType".into(),
                    value: "SDR".into(),
                    is_required: true,
                }],
            }]
        };
        DeviceProfile {
            codec_profiles,
            direct_play_profiles: vec![
                DirectPlayProfile {
                    container: containers.clone(),
                    video_codec: video.clone(),
                    audio_codec: audio.clone(),
                    kind: "Video".into(),
                },
                DirectPlayProfile {
                    container: containers,
                    video_codec: String::new(),
                    audio_codec: audio.clone(),
                    kind: "Audio".into(),
                },
            ],
            transcoding_profiles: vec![
                TranscodingProfile {
                    container: "ts".into(),
                    video_codec: video,
                    audio_codec: audio.clone(),
                    protocol: "hls".into(),
                    kind: "Video".into(),
                },
                TranscodingProfile {
                    container: "ts".into(),
                    video_codec: String::new(),
                    audio_codec: audio,
                    protocol: "hls".into(),
                    kind: "Audio".into(),
                },
            ],
            max_streaming_bitrate: (self.max_bitrate > 0).then_some(self.max_bitrate),
            subtitle_profiles: if draws_images {
                // A non-empty list switches off the empty-profile default that
                // keeps every text subtitle External, so name the WebVTT and
                // ASS the native player is served (ASS converted) explicitly.
                ["vtt", "ass"]
                    .into_iter()
                    .map(str::to_owned)
                    .chain(self.image_subtitles.iter().cloned())
                    .map(|format| SubtitleProfileDto {
                        format,
                        method: "External".into(),
                        ..SubtitleProfileDto::default()
                    })
                    .collect()
            } else {
                Vec::new()
            },
            ..DeviceProfile::default()
        }
    }
}

/// A native player's request to play one item.
pub struct GrantRequest<'a> {
    pub capabilities: &'a Capabilities,
    /// `None` takes the source's default audio track.
    pub audio_stream_index: Option<u32>,
    /// `-1` is "off", `None` the container's default.
    pub subtitle_stream_index: Option<i64>,
    /// The session's device token: the playback ends with it (B256).
    pub token: &'a str,
    pub client_is_remote: bool,
    /// A version's id (`AlternateMediaSource::id`); empty plays the primary.
    pub version: &'a str,
    /// The device the session's token was issued to. With it, a negotiate may
    /// adopt a grant that device released without a stop (B1397).
    pub device: Option<&'a pharos_core::ClientDeviceId>,
}

/// A playback a native player may start: the stream capability and the
/// decision behind it.
pub struct Grant {
    /// The numeric item id and the version as named: with the device, what a
    /// released grant is matched on.
    pub item_id: u64,
    pub version: String,
    /// What makes this stream the same stream as another negotiation's.
    pub shape: crate::pending_release::StreamShape,
    /// Adopted from a released grant: the session, its transcode and its
    /// `/Sessions` entry already exist, and nothing was prewarmed.
    pub adopted: bool,
    /// The position of the last report on an adopted grant.
    pub reported_ms: Option<u64>,
    pub play_session_id: String,
    /// The item as the player names it in progress reports.
    pub item_wire_id: String,
    pub negotiated: Negotiated,
    /// The raw file for a direct play, else the transcoding URL.
    pub stream_url: String,
    /// The audio track the playback starts on, when the item has one.
    pub audio_stream_index: Option<u32>,
    /// The subtitle it starts with; `None` is off.
    pub subtitle_stream_index: Option<u32>,
    /// Whether the tracks this playback reports may be remembered for the
    /// item: only the primary file's, whose stream indexes they name.
    pub remembers_tracks: bool,
}

#[derive(Debug)]
pub enum GrantError {
    /// The item is hidden, missing, or could not be read.
    Item(crate::services::library::DetailError),
    /// A book is read, not streamed (004-books).
    NotPlayable,
    Version(VersionError),
}

/// Why a native player cannot have the version it named.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VersionError {
    /// Not one of the item's editions or copies.
    NoSuchVersion,
    /// Stored before editions kept their own probe (ADR-0030), so it cannot
    /// be served until a rescan.
    Unprobed,
}

/// `item` as the file `version` names: the primary for an empty id, else a
/// grouped edition or copy, its path and probe swapped in by
/// [`media_source::select`] under the item's own id. Unlike a Jellyfin
/// client's `MediaSourceId`, an edition that cannot be served is refused
/// rather than replaced by the primary: a player that asked for a 3D file
/// would draw a 2D one as 3D.
pub(crate) fn select_version(
    route: media_source::Route,
    item: MediaItem,
    version: &str,
) -> Result<MediaItem, VersionError> {
    if version.is_empty() {
        return Ok(item);
    }
    let raw = format!(
        "{}-{version}",
        pharos_jellyfin_api::dto::wire_item_id(item.id)
    );
    if media_source::classify(&item, Some(&raw)).0 != media_source::Requested::Alternate {
        tracing::info!(
            media.id = item.id,
            route = route.label(),
            version,
            "asked for a version the item does not have"
        );
        return Err(VersionError::NoSuchVersion);
    }
    let item = media_source::select(route, item, Some(&raw));
    if item.edition.is_none() {
        return Err(VersionError::Unprobed);
    }
    Ok(item)
}

/// The item `id` a native player may start, with the user's play state.
async fn playable_item(
    state: &AppState,
    user: &pharos_core::User,
    id: u64,
) -> Result<(MediaItem, pharos_core::UserItemData), GrantError> {
    let (item, user_data) = crate::services::library::detail(state, user, id)
        .await
        .map_err(GrantError::Item)?;
    if matches!(item.kind, MediaKind::Book) {
        return Err(GrantError::NotPlayable);
    }
    Ok((item, user_data))
}

/// The tracks a playback starts on, as [`pick_tracks`] chose them.
pub struct TrackPick {
    pub audio: AudioPick,
    /// The subtitle the negotiation asks for, after the styled-track mapping.
    pub requested_subtitle: Option<i64>,
    /// The subtitle picked from the source's tracks; `None` is off.
    pub subtitle: Option<u32>,
    /// The styled track's own index, when the playback starts on one.
    pub styled_pick: Option<u32>,
}

/// Picks the tracks `item` starts on: the explicit pick on the request, else
/// the user's remembered pick for this item, else their language rules.
/// `None` names the default. Shared by `grant` and `start_tracks`, so the
/// tracks a title page shows are the ones the playback starts on.
async fn pick_tracks(
    state: &AppState,
    user: &pharos_core::User,
    item: &MediaItem,
    streams: &[pharos_jellyfin_api::dto::MediaStreamDto],
    subtitle_profiles: &[SubtitleProfileDto],
    audio_explicit: Option<u32>,
    subtitle_explicit: Option<i64>,
) -> TrackPick {
    let audio = pick_audio(state, user, item, streams, audio_explicit).await;
    // A remembered pick counts as the player's own: the grant echoes the
    // styled index, so that is what the player reports and gets remembered.
    let stated_subtitle = subtitle_explicit.or_else(|| {
        audio
            .prefs
            .remember_subtitle
            .then_some(audio.remembered.subtitle_stream_index)
            .flatten()
            .map(i64::from)
    });
    let (requested_subtitle, styled_pick) =
        crate::styled_track::negotiation_choice(&state.stores, item, stated_subtitle).await;
    let subtitle = pick_subtitle(
        &audio,
        streams,
        &item.probe,
        subtitle_profiles,
        requested_subtitle,
    );
    let styled_pick = match (styled_pick, stated_subtitle, subtitle) {
        (None, None, Some(source)) => {
            crate::styled_track::default_pick(&state.stores, item, source).await
        }
        (picked, _, _) => picked,
    };
    TrackPick {
        audio,
        requested_subtitle,
        subtitle,
        styled_pick,
    }
}

/// The audio and subtitle `grant` would start `id` on for a default
/// negotiation with `capabilities`. Opens no session.
pub async fn start_tracks(
    state: &AppState,
    user: &pharos_core::User,
    id: u64,
    capabilities: &Capabilities,
) -> Result<(Option<u32>, Option<u32>), GrantError> {
    let (item, _) = playable_item(state, user, id).await?;
    let profile = capabilities.profile(crate::services::watch::extracts_image_subtitles(state));
    let streams = crate::services::watch::streams(&item).await;
    let picked = pick_tracks(
        state,
        user,
        &item,
        &streams,
        &profile.subtitle_profiles,
        None,
        None,
    )
    .await;
    Ok((picked.audio.index, picked.styled_pick.or(picked.subtitle)))
}

/// Negotiates `id` for a native player and registers the session that
/// authorizes its stream, as `/Items/{id}/PlaybackInfo` does for a
/// Jellyfin client: the same access check, decision, metric and session.
pub async fn grant(
    state: &actix_web::web::Data<AppState>,
    user: &pharos_core::User,
    id: u64,
    r: &GrantRequest<'_>,
) -> Result<Grant, GrantError> {
    let (item, user_data) = playable_item(state, user, id).await?;
    let item = select_version(media_source::Route::Negotiate, item, r.version)
        .map_err(GrantError::Version)?;
    let item_wire_id = pharos_jellyfin_api::dto::wire_item_id(id);
    // The `{item}-{edition}` id PlaybackInfo hands Jellyfin clients, so the
    // stream and playlist routes read the version's file.
    let source_param = item
        .edition
        .as_deref()
        .map(|e| format!("&MediaSourceId={item_wire_id}-{e}"))
        .unwrap_or_default();
    let mut profile = r
        .capabilities
        .profile(crate::services::watch::extracts_image_subtitles(state));
    profile.denied_audio_codecs = denied_audio_codecs_for_token(state, r.token).await;
    let streams = crate::services::watch::streams(&item).await;
    let release_key = r.device.map(|device| crate::pending_release::ReleaseKey {
        user: user.id,
        device: device.clone(),
        item: id,
        version: r.version.to_owned(),
    });
    // A player that reconnects under a live stream names no tracks, and would
    // resolve to a pick it switched to mid-playback (which is remembered): the
    // stream it is still pulling is on the tracks it was negotiated on, so a
    // default pick follows the held release.
    let held = release_key
        .as_ref()
        .and_then(|key| state.pending_releases.held_picks(key));
    let TrackPick {
        audio,
        requested_subtitle,
        subtitle,
        styled_pick,
    } = pick_tracks(
        state,
        user,
        &item,
        &streams,
        &profile.subtitle_profiles,
        r.audio_stream_index.or(held.and_then(|(audio, _)| audio)),
        r.subtitle_stream_index
            .or(held.and_then(|(_, subtitle)| subtitle.map(i64::from))),
    )
    .await;
    // The decision names these tracks, so the stream must carry them: the
    // picked subtitle (or none) is what gets burned, and an audio track other
    // than the one a player starts on cannot direct play.
    let switches = PlaybackSwitches {
        direct_play: audio.index.is_none() || audio.index == first_audio(&streams),
        ..PlaybackSwitches::default()
    };
    let negotiated = negotiate(
        state,
        &item,
        &NegotiateRequest {
            profile: &profile,
            switches,
            explicit_subtitle_index: Some(subtitle.map_or(-1, i64::from)),
            stereo_mode: None,
            client_is_remote: r.client_is_remote,
            is_web_client: false,
        },
    );
    let shape = crate::pending_release::StreamShape {
        decision: negotiated.decision.clone(),
        burn_subtitle_indices: negotiated.burn_text_indices.clone(),
        audio: audio.index,
        burned_subtitle: negotiated
            .selected_subtitle
            .as_ref()
            .filter(|t| {
                is_image_subtitle_codec(t.codec.as_deref().unwrap_or(""))
                    || negotiated.burn_text_indices.contains(&t.stream_index)
            })
            .map(|t| t.stream_index),
    };
    if let Some(adopted) = match &release_key {
        Some(key) => adopt_released(state, user, id, r, key, &shape).await,
        None => None,
    } {
        return Ok(Grant {
            play_session_id: adopted.play_session_id,
            item_wire_id: adopted.item_wire_id,
            negotiated,
            stream_url: adopted.stream_url,
            audio_stream_index: adopted.audio,
            subtitle_stream_index: adopted.subtitle,
            remembers_tracks: adopted.remembers_tracks,
            item_id: id,
            version: r.version.to_owned(),
            shape,
            adopted: true,
            reported_ms: adopted.reported_ms,
        });
    }
    let play_session_id = uuid::Uuid::new_v4().simple().to_string();
    let resume_ticks = if user_data.played {
        0
    } else {
        user_data.last_played_position_ticks
    };
    let mut delivery_renditions: Vec<String> = Vec::new();
    let stream_url = match negotiated.decision {
        Decision::DirectPlay => format!(
            "/{}/{item_wire_id}/stream?static=true&tag={play_session_id}{source_param}",
            if negotiated.is_video {
                "videos"
            } else {
                "audio"
            },
        ),
        _ => {
            let d = delivery(
                &negotiated,
                &DeliveryRequest {
                    profile: &profile,
                    probe: &item.probe,
                    item_wire_id: &item_wire_id,
                    source_param: &source_param,
                    play_session_id: &play_session_id,
                    audio_stream_index: audio.index.map(|i| i.to_string()),
                    explicit_subtitle_index: requested_subtitle,
                    is_web_client: false,
                    resume_ticks,
                    transcoding: true,
                    size_limits: pharos_transcode::SizeLimits::default(),
                },
            );
            delivery_renditions = d.renditions;
            d.transcoding_url
                .and_then(|url| ensure_audio_index_param(Some(url), audio.index))
                .unwrap_or_default()
        }
    };
    let holder = pharos_core::SessionHolder {
        user_id: user.id,
        token_id: pharos_core::TokenStore::token_id(&state.stores, r.token),
    };
    register(
        state,
        &play_session_id,
        id,
        &negotiated,
        &item.probe,
        holder.clone(),
    )
    .await;
    // The client will not ask for segment 0 until its player has loaded the
    // master, so warm the opening segments now, off its critical path. The
    // URL is the one handed back, so the prewarm derives the keys the same
    // way the client's requests will.
    let transcoding_url =
        (!matches!(negotiated.decision, Decision::DirectPlay)).then_some(stream_url.as_str());
    let renditions = delivery_renditions;
    crate::api::jellyfin::hls::prewarm_negotiated(
        state,
        crate::api::jellyfin::hls::NegotiatedPrewarm {
            route: crate::api::jellyfin::hls::PrewarmRoute::Native,
            media_id: id,
            negotiated: &negotiated,
            transcoding_url,
            renditions: &renditions,
            probe: &item.probe,
            holder,
            play_session_id: &play_session_id,
            resume_ticks,
            // Native clients play through hls.js in a browser engine, so their
            // media requests send a Mozilla user agent and `master_playlist`
            // serves them the demuxed h264 CMAF master.
            fetches_as_browser: true,
            is_audio: !negotiated.is_video,
            media_source_id: item.edition.is_some().then(|| {
                format!(
                    "{item_wire_id}-{}",
                    item.edition.as_deref().unwrap_or_default()
                )
            }),
        },
    );
    negotiated.record();
    tracing::info!(
        media.id = id,
        user.name = %user.name,
        play_session_id = %pharos_core::secret_prefix(&play_session_id),
        decision = negotiated.decision_kind(),
        direct_play_block = negotiated.direct_play_block.label(),
        downgrade = negotiated.downgrade.unwrap_or("none"),
        client_is_remote = r.client_is_remote,
        audio_index = audio.index.map(i64::from).unwrap_or(-1),
        subtitle_index = subtitle.map(i64::from).unwrap_or(-1),
        edition = item.edition.as_deref().unwrap_or("primary"),
        stereo = item.probe.video_3d_format.map_or("none", |f| f.wire_name()),
        "native playback negotiated"
    );
    Ok(Grant {
        play_session_id,
        item_wire_id,
        negotiated,
        stream_url,
        audio_stream_index: audio.index,
        subtitle_stream_index: styled_pick.or(subtitle),
        remembers_tracks: item.edition.is_none(),
        item_id: id,
        version: r.version.to_owned(),
        shape,
        adopted: false,
        reported_ms: None,
    })
}

/// Takes over the grant `r`'s device released without a stop for the same
/// item and version, when the stream negotiated is the same one: the player
/// is still pulling segments under its session id, and a new id would 410
/// them (B1397). A held grant that is not that stream ends at once.
async fn adopt_released(
    state: &AppState,
    user: &pharos_core::User,
    item: u64,
    r: &GrantRequest<'_>,
    key: &crate::pending_release::ReleaseKey,
    shape: &crate::pending_release::StreamShape,
) -> Option<crate::pending_release::Released> {
    let claim = state.pending_releases.claim(key)?;
    if let Some(why) = claim.mismatch(shape) {
        claim.reject(why);
        return None;
    }
    let psid = claim.released()?.play_session_id.clone();
    // The transcode session is the stream's capability: adopt only a live one
    // this user holds for this item.
    let live = state
        .transcode_sessions
        .get(pharos_core::PlaySessionId::from(psid.as_str()))
        .await;
    match live {
        Ok(Some(s)) if s.holder.user_id == user.id && s.media_id == item => {
            // The stream now answers to the adopting token: revoking it ends
            // the playback, and revoking the old connection's does not.
            let holder = pharos_core::SessionHolder {
                user_id: user.id,
                token_id: pharos_core::TokenStore::token_id(&state.stores, r.token),
            };
            match state
                .transcode_sessions
                .rebind_holder(&pharos_core::PlaySessionId::from(psid.as_str()), holder)
                .await
            {
                Ok(true) => claim.adopt(),
                Ok(false) => {
                    claim.reject("session: the transcode session is gone".into());
                    None
                }
                Err(e) => {
                    claim.reject(format!("session: rebinding its holder failed: {e}"));
                    None
                }
            }
        }
        Ok(other) => {
            claim.reject(format!(
                "session: the transcode session is {}",
                if other.is_some() {
                    "held by another user or item"
                } else {
                    "gone"
                }
            ));
            None
        }
        Err(e) => {
            claim.reject(format!("session: transcode session lookup failed: {e}"));
            None
        }
    }
}

/// Convert the boot-resolved server encode capabilities into the negotiator's
/// codec-support DTO (canonical codec names, hw flag, cost tier). An empty
/// capability set (tests / not-yet-wired) yields an empty support list, so the
/// picker falls back to h264 — matching the old hardcoded target.
fn server_codec_support(caps: &pharos_transcode::ServerEncodeCapabilities) -> ServerCodecSupport {
    fn codec_name(c: pharos_transcode::VideoCodec) -> Option<&'static str> {
        use pharos_transcode::VideoCodec::*;
        match c {
            H264 => Some("h264"),
            H265 => Some("h265"),
            Vp9 => Some("vp9"),
            Av1 => Some("av1"),
            Copy => None,
        }
    }
    fn cost(c: pharos_transcode::RelCost) -> u8 {
        use pharos_transcode::RelCost::*;
        match c {
            Cheap => 0,
            Moderate => 1,
            Expensive => 2,
            Glacial => 3,
        }
    }
    let encodable: Vec<CodecCap> = caps
        .video
        .iter()
        .filter_map(|c| {
            codec_name(c.codec).map(|name| CodecCap {
                name: name.to_string(),
                hw: c.accel.is_hardware(),
                cost: cost(c.cost),
            })
        })
        .collect();
    // An empty set means capabilities weren't resolved (tests, or an ffmpeg
    // that failed `-encoders`) — NOT "can encode nothing". Fall back to the
    // permissive software assumption so a vp9-only client still gets vp9, etc.
    if encodable.is_empty() {
        ServerCodecSupport::default()
    } else {
        ServerCodecSupport { encodable }
    }
}

#[cfg(test)]
mod playback_switch_tests {
    use super::{apply_playback_switches, PlaybackSwitches};
    use pharos_jellyfin_api::device_profile::Decision;

    fn video_remux() -> Decision {
        Decision::VideoRemux {
            target_container: "mp4".into(),
            target_audio_codec: None,
        }
    }

    fn audio_remux() -> Decision {
        Decision::AudioRemux {
            target_audio_codec: "aac".into(),
        }
    }

    fn forced() -> Decision {
        Decision::Transcode {
            target_container: "mp4".into(),
            target_video_codec: Some("hevc".into()),
            target_audio_codec: None,
            max_video_bitrate_bps: None,
        }
    }

    fn apply(d: Decision, sw: &PlaybackSwitches, is_video: bool) -> Decision {
        apply_playback_switches(d, sw, is_video, forced)
    }

    #[test]
    fn forbidding_video_copy_transcodes_both_video_remuxes() {
        let sw = PlaybackSwitches {
            video_copy: false,
            ..Default::default()
        };
        assert_eq!(apply(video_remux(), &sw, true), forced());
        // AudioRemux on a video source copies the VIDEO and re-encodes audio.
        assert_eq!(apply(audio_remux(), &sw, true), forced());
        // An audio-only item has no video to copy.
        assert_eq!(apply(audio_remux(), &sw, false), audio_remux());
    }

    #[test]
    fn forbidding_audio_copy_only_touches_audio_only_remux() {
        let sw = PlaybackSwitches {
            audio_copy: false,
            ..Default::default()
        };
        assert_eq!(apply(audio_remux(), &sw, false), forced());
        assert_eq!(apply(audio_remux(), &sw, true), audio_remux());
        assert_eq!(apply(video_remux(), &sw, true), video_remux());
    }

    #[test]
    fn disabling_direct_play_forces_the_renegotiated_transcode() {
        let sw = PlaybackSwitches {
            direct_play: false,
            ..Default::default()
        };
        assert_eq!(apply(Decision::DirectPlay, &sw, true), forced());
    }

    #[test]
    fn default_switches_change_nothing() {
        for d in [Decision::DirectPlay, video_remux(), audio_remux()] {
            assert_eq!(apply(d.clone(), &PlaybackSwitches::default(), true), d);
        }
    }
}

#[cfg(test)]
mod browser_matroska_fallback_tests {
    use super::browser_matroska_direct_unplayable;

    #[test]
    fn downgrades_only_an_unplayable_browser_matroska_directplay() {
        // The Lace case (B80): browser + video + DirectPlay verdict + Matroska
        // source + NOT webm-relabel-playable (av1 + non-webm audio) → downgrade.
        assert!(browser_matroska_direct_unplayable(
            true, true, true, true, false
        ));
        // Raw-playable source (a genuine WebM the client decodes) → keep direct.
        assert!(!browser_matroska_direct_unplayable(
            true, true, true, true, true
        ));
        // Native app that declared real Matroska DirectPlay → keep (not a web
        // client; its `<video>`-less player demuxes Matroska for real).
        assert!(!browser_matroska_direct_unplayable(
            false, true, true, true, false
        ));
        // Non-Matroska container (mp4/av1 a browser plays natively) → keep.
        assert!(!browser_matroska_direct_unplayable(
            true, true, true, false, false
        ));
        // Already transcoding (not a DirectPlay verdict) → nothing to downgrade.
        assert!(!browser_matroska_direct_unplayable(
            true, true, false, true, false
        ));
        // Audio-only source → unaffected.
        assert!(!browser_matroska_direct_unplayable(
            true, false, true, true, false
        ));
    }
}

#[cfg(test)]
mod source_plays_raw_in_browser_tests {
    use super::source_plays_raw_in_browser;
    use pharos_jellyfin_api::device_profile::DirectPlayProfile;

    /// jellyfin-web's browser profile: WebM carrying VP9/AV1 + Opus. Firefox
    /// declares no `mkv` entry — it has no Matroska demuxer to declare.
    fn browser_profiles() -> Vec<DirectPlayProfile> {
        vec![DirectPlayProfile {
            container: "webm".into(),
            video_codec: "vp8,vp9,av1".into(),
            audio_codec: "vorbis,opus".into(),
            kind: "Video".into(),
        }]
    }

    #[test]
    fn a_matroska_never_plays_raw_however_webm_legal_its_codecs() {
        // B107, the exact live case: Show AE S01E01, av1 + opus in a .mkv,
        // to Firefox. Every codec here is WebM-legal and the client lists them
        // — which is precisely why the old codec-based check said "playable"
        // and disarmed the downgrade. The container is what decides, and
        // Firefox refuses EBML DocType `matroska` outright.
        assert!(!source_plays_raw_in_browser(
            "mkv",
            Some("av1"),
            Some("opus"),
            &browser_profiles()
        ));
    }

    #[test]
    fn a_genuine_webm_plays_raw() {
        // Same codecs, same client, real WebM container → still direct.
        // Fixing B107 must not force needless transcodes on true WebM files.
        assert!(source_plays_raw_in_browser(
            "webm",
            Some("av1"),
            Some("opus"),
            &browser_profiles()
        ));
    }

    #[test]
    fn a_webm_the_client_does_not_decode_does_not_play_raw() {
        // Container alone is not sufficient: a client with no matching video
        // codec in its webm profile cannot play it.
        let profiles = vec![DirectPlayProfile {
            container: "webm".into(),
            video_codec: "vp8".into(),
            audio_codec: "opus".into(),
            kind: "Video".into(),
        }];
        assert!(!source_plays_raw_in_browser(
            "webm",
            Some("av1"),
            Some("opus"),
            &profiles
        ));
    }

    #[test]
    fn a_webm_with_non_webm_audio_does_not_play_raw() {
        // Video would decode, audio would not — the file is not playable AS-IS.
        assert!(!source_plays_raw_in_browser(
            "webm",
            Some("vp9"),
            Some("ac3"),
            &browser_profiles()
        ));
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod resolve_selected_subtitle_tests {
    use super::resolve_selected_subtitle;
    use pharos_core::{MediaProbe, SubtitleTrack};

    /// A client that renders text subs externally but, like every browser,
    /// cannot render bitmap ones.
    fn text_capable_profiles() -> Vec<pharos_jellyfin_api::device_profile::SubtitleProfileDto> {
        vec![pharos_jellyfin_api::device_profile::SubtitleProfileDto {
            format: "subrip".into(),
            method: "External".into(),
            ..Default::default()
        }]
    }

    fn image_track(idx: u32, is_default: bool) -> SubtitleTrack {
        SubtitleTrack {
            stream_index: idx,
            codec: Some("hdmv_pgs_subtitle".into()),
            is_default,
            ..Default::default()
        }
    }

    /// B44's Film BA case: default AND forced, i.e. dialogue rather than a
    /// convenience.
    fn forced_image_track(idx: u32) -> SubtitleTrack {
        SubtitleTrack {
            stream_index: idx,
            codec: Some("hdmv_pgs_subtitle".into()),
            is_default: true,
            is_forced: true,
            ..Default::default()
        }
    }

    /// A FORCED image default is still burned. Without it the foreign-language
    /// sections play unsubtitled (B44), and its cost is bounded — forced
    /// tracks cover a fraction of the runtime, unlike the full-film burn this
    /// rule declines.
    #[test]
    fn a_forced_image_default_is_still_selected() {
        let probe = probe_with(vec![forced_image_track(5)]);
        assert_eq!(
            resolve_selected_subtitle(None, &probe, &text_capable_profiles())
                .map(|t| t.stream_index),
            Some(5),
            "a forced track is dialogue, not a convenience"
        );
    }

    fn probe_with(tracks: Vec<SubtitleTrack>) -> MediaProbe {
        MediaProbe {
            subtitle_tracks: tracks,
            ..Default::default()
        }
    }

    fn track(idx: u32, is_default: bool) -> SubtitleTrack {
        SubtitleTrack {
            stream_index: idx,
            codec: Some("subrip".into()),
            is_default,
            ..Default::default()
        }
    }

    #[test]
    fn explicit_positive_index_resolves_that_track() {
        let probe = probe_with(vec![track(2, false), track(4, true)]);
        let sel = resolve_selected_subtitle(Some(2), &probe, &text_capable_profiles());
        assert_eq!(sel.map(|t| t.stream_index), Some(2));
    }

    #[test]
    fn explicit_minus_one_is_off() {
        let probe = probe_with(vec![track(2, false), track(4, true)]);
        assert!(resolve_selected_subtitle(Some(-1), &probe, &text_capable_profiles()).is_none());
    }

    #[test]
    fn no_explicit_pick_falls_back_to_default_track() {
        let probe = probe_with(vec![track(2, false), track(4, true)]);
        let sel = resolve_selected_subtitle(None, &probe, &text_capable_profiles());
        assert_eq!(sel.map(|t| t.stream_index), Some(4));
    }

    #[test]
    fn no_explicit_pick_and_no_default_is_none() {
        let probe = probe_with(vec![track(2, false), track(4, false)]);
        assert!(resolve_selected_subtitle(None, &probe, &text_capable_profiles()).is_none());
    }

    /// T110 — an auto-selected subtitle must never commit the server to
    /// burning every frame.
    ///
    /// Live on 2026-08-16 the server logged `subtitle.requested: None,
    /// subtitle.resolved: Some(2), subtitle.codec: hdmv_pgs_subtitle,
    /// subtitle.delivery: Burn` — nobody asked for that track, and burning it
    /// costs p95 10995 ms to produce 6000 ms of video, i.e. above realtime.
    /// The buffer starves, the player restarts, and each restart asks for more
    /// burns: one segment was re-fetched 118 times.
    #[test]
    fn an_unrequested_image_default_is_not_auto_selected() {
        // Only image tracks, so there is nothing cheap to swap to — the case
        // Film S actually presents.
        let probe = probe_with(vec![image_track(2, false), image_track(4, true)]);
        assert!(
            resolve_selected_subtitle(None, &probe, &text_capable_profiles()).is_none(),
            "a PGS default nobody asked for must not be selected: its only \
             delivery is a full-frame burn, and the client cannot turn it off"
        );
    }

    /// …but an EXPLICIT pick is still honoured. Burning a subtitle someone
    /// actually chose is the correct answer to an expensive request; this rule
    /// is about the cost nobody opted into.
    #[test]
    fn an_explicitly_requested_image_track_is_still_selected() {
        let probe = probe_with(vec![track(2, false), image_track(4, true)]);
        let sel = resolve_selected_subtitle(Some(4), &probe, &text_capable_profiles());
        assert_eq!(
            sel.map(|t| t.stream_index),
            Some(4),
            "an explicit request for a burned subtitle must still be served"
        );
    }

    /// And a text default is unaffected — it delivers externally for nothing,
    /// which is the case this must not regress.
    #[test]
    fn a_text_default_is_still_auto_selected() {
        let probe = probe_with(vec![image_track(2, false), track(4, true)]);
        let sel = resolve_selected_subtitle(None, &probe, &text_capable_profiles());
        assert_eq!(sel.map(|t| t.stream_index), Some(4));
    }

    /// An unrenderable ASS default is STILL burned. B104 is why: converting
    /// it to WebVTT drops positioning and fonts, which the Android app
    /// rendered as black bars — and a text overlay costs a fraction of a
    /// bitmap one. This rule is about image subs, not about burning per se.
    #[test]
    fn an_unrenderable_ass_default_is_still_selected_and_burned() {
        let ass = SubtitleTrack {
            stream_index: 4,
            codec: Some("ass".into()),
            is_default: true,
            ..Default::default()
        };
        let probe = probe_with(vec![ass]);
        assert_eq!(
            resolve_selected_subtitle(None, &probe, &text_capable_profiles())
                .map(|t| t.stream_index),
            Some(4),
            "B104's ASS burn must survive the T110 rule"
        );
    }

    /// Better than declining: when the image default would burn but the file
    /// also carries a track this client renders itself, take that one. The
    /// viewer keeps subtitles and the server encodes nothing.
    #[test]
    fn an_image_default_is_swapped_for_a_renderable_text_track() {
        let probe = probe_with(vec![image_track(4, true), track(2, false)]);
        assert_eq!(
            resolve_selected_subtitle(None, &probe, &text_capable_profiles())
                .map(|t| t.stream_index),
            Some(2),
            "prefer the free text track over a full-frame burn nobody asked for"
        );
    }

    #[test]
    fn explicit_index_with_no_matching_track_is_none() {
        let probe = probe_with(vec![track(2, false)]);
        assert!(resolve_selected_subtitle(Some(99), &probe, &text_capable_profiles()).is_none());
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod subtitle_selection_forces_transcode_tests {
    use super::subtitle_selection_forces_transcode;
    use pharos_core::SubtitleTrack;
    use pharos_jellyfin_api::device_profile::{Decision, SubtitleProfileDto};

    fn ass_track(idx: u32) -> SubtitleTrack {
        SubtitleTrack {
            stream_index: idx,
            codec: Some("ass".into()),
            ..Default::default()
        }
    }

    fn pgs_track(idx: u32) -> SubtitleTrack {
        SubtitleTrack {
            stream_index: idx,
            codec: Some("hdmv_pgs_subtitle".into()),
            ..Default::default()
        }
    }

    fn subrip_external_only() -> Vec<SubtitleProfileDto> {
        vec![SubtitleProfileDto {
            format: "subrip".into(),
            method: "External".into(),
            ..Default::default()
        }]
    }

    fn ass_external() -> Vec<SubtitleProfileDto> {
        vec![SubtitleProfileDto {
            format: "ass".into(),
            method: "External".into(),
            ..Default::default()
        }]
    }

    #[test]
    fn directplay_with_burned_ass_forces_transcode() {
        let sel = ass_track(4);
        let profiles = subrip_external_only();
        assert!(subtitle_selection_forces_transcode(
            &Decision::DirectPlay,
            Some(&sel),
            &profiles
        ));
    }

    #[test]
    fn directplay_with_externally_rendered_ass_does_not_transcode() {
        let sel = ass_track(4);
        let profiles = ass_external();
        assert!(!subtitle_selection_forces_transcode(
            &Decision::DirectPlay,
            Some(&sel),
            &profiles
        ));
    }

    #[test]
    fn directplay_with_image_sub_forces_transcode() {
        let sel = pgs_track(7);
        assert!(subtitle_selection_forces_transcode(
            &Decision::DirectPlay,
            Some(&sel),
            &[]
        ));
    }

    #[test]
    fn no_subtitle_selected_keeps_directplay() {
        assert!(!subtitle_selection_forces_transcode(
            &Decision::DirectPlay,
            None,
            &[]
        ));
    }

    #[test]
    fn already_transcoding_is_noop() {
        let sel = ass_track(4);
        assert!(!subtitle_selection_forces_transcode(
            &Decision::Transcode {
                target_container: "ts".into(),
                target_video_codec: Some("h264".into()),
                target_audio_codec: Some("aac".into()),
                max_video_bitrate_bps: None,
            },
            Some(&sel),
            &[]
        ));
    }
}

/// V234 — a subtitle burned into a packed 3D source keeps the layout; only an
/// explicit 2D request or a profile that rejects 3D downmixes it.
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod burn_on_packed_3d_tests {
    use super::*;
    use crate::state::Stores;
    use pharos_core::{MediaKind, MediaProbe, SubtitleTrack, Video3DFormat};
    use pharos_transcode::StereoOutput;

    const PACKED: [Video3DFormat; 4] = [
        Video3DFormat::HalfSideBySide,
        Video3DFormat::FullSideBySide,
        Video3DFormat::HalfTopAndBottom,
        Video3DFormat::FullTopAndBottom,
    ];

    fn item(format: Video3DFormat, subtitle_codec: &str) -> MediaItem {
        MediaItem {
            id: 1,
            path: "/m/3d.mkv".into(),
            kind: MediaKind::Movie,
            probe: MediaProbe {
                video_codec: Some("h264".into()),
                audio_codec: Some("aac".into()),
                video_3d_format: Some(format),
                subtitle_tracks: vec![SubtitleTrack {
                    stream_index: 3,
                    codec: Some(subtitle_codec.into()),
                    ..Default::default()
                }],
                ..Default::default()
            },
            ..Default::default()
        }
    }

    async fn negotiated(item: &MediaItem, stereo_mode: Option<&str>) -> Negotiated {
        let stores = Stores::connect("sqlite::memory:").await.unwrap();
        let state = AppState::new(stores, "t".into());
        let profile = DeviceProfile::default();
        negotiate(
            &state,
            item,
            &NegotiateRequest {
                profile: &profile,
                switches: PlaybackSwitches::default(),
                explicit_subtitle_index: Some(3),
                stereo_mode,
                client_is_remote: false,
                is_web_client: false,
            },
        )
    }

    #[actix_web::test]
    async fn burning_an_image_subtitle_keeps_every_packed_layout() {
        for format in PACKED {
            let n = negotiated(&item(format, "hdmv_pgs_subtitle"), None).await;
            assert!(n.burning_sub_per_eye, "{format:?}");
            assert_eq!(n.stereo_output, StereoOutput::Preserve, "{format:?}");
            let n = negotiated(&item(format, "hdmv_pgs_subtitle"), Some("preserve")).await;
            assert_eq!(n.stereo_output, StereoOutput::Preserve, "{format:?}");
        }
    }

    #[actix_web::test]
    async fn asking_for_2d_still_downmixes_a_burned_packed_source() {
        for format in PACKED {
            for mode in ["mono", "mono2d"] {
                let n = negotiated(&item(format, "hdmv_pgs_subtitle"), Some(mode)).await;
                assert_eq!(n.stereo_output, StereoOutput::Mono2D, "{format:?} {mode}");
            }
        }
    }

    #[actix_web::test]
    async fn mvc_burn_is_not_a_per_eye_burn() {
        let n = negotiated(&item(Video3DFormat::Mvc, "hdmv_pgs_subtitle"), None).await;
        assert!(!n.burning_sub_per_eye);
        assert_eq!(n.stereo_output, StereoOutput::Preserve);
    }

    #[test]
    fn the_3d_decision_reason_names_a_per_eye_burn_and_stays_distinct() {
        let block = "no_direct_play_profile";
        let reasons = [
            playback_3d_reason(true, false, false, false, false, None, block),
            playback_3d_reason(false, false, true, false, false, None, block),
            playback_3d_reason(false, false, false, true, false, None, block),
            playback_3d_reason(false, false, false, false, true, None, block),
            playback_3d_reason(
                false,
                false,
                false,
                false,
                false,
                Some("subtitle_burn"),
                block,
            ),
            playback_3d_reason(false, false, false, false, false, None, block),
        ];
        assert_eq!(
            reasons,
            [
                "passthrough",
                "stereo_mode_query",
                "profile_condition",
                "burn_per_eye",
                "subtitle_burn",
                block
            ]
        );
        let distinct: std::collections::BTreeSet<_> = reasons.iter().collect();
        assert_eq!(distinct.len(), reasons.len(), "{reasons:?}");
        // A 2D request outranks the burn: the plan is then not per-eye.
        assert_eq!(
            playback_3d_reason(false, false, true, false, true, None, block),
            "stereo_mode_query"
        );
    }
}

#[cfg(test)]
mod native_max_bitrate_tests {
    use super::Capabilities;
    use pharos_jellyfin_api::device_profile::{
        negotiate, Decision, ServerCodecSupport, SourceMedia,
    };

    fn source(bitrate_bps: u64) -> SourceMedia {
        SourceMedia {
            container: "mp4".into(),
            video_codec: Some("h264".into()),
            audio_codec: Some("eac3".into()),
            bitrate_bps: Some(bitrate_bps),
            is_video: true,
            video_level: None,
            video_profile: None,
            audio_channels: None,
            width: None,
            height: None,
            video_bit_depth: None,
            video_3d_format: None,
            video_range_type: None,
        }
    }

    fn caps(max_bitrate: u64) -> Capabilities {
        Capabilities {
            video_codecs: vec!["h264".into()],
            audio_codecs: vec!["aac".into()],
            containers: vec!["mp4".into()],
            max_bitrate,
            hdr: true,
            image_subtitles: vec![],
        }
    }

    fn decide(max_bitrate: u64, source_bps: u64) -> Decision {
        negotiate(
            &caps(max_bitrate).profile(false),
            &source(source_bps),
            &ServerCodecSupport::default(),
        )
    }

    /// The remote viewer's case: an audio-remux source at 4.7 Mbit/s must not
    /// be copied once the viewer capped the stream at 2 Mbit/s.
    #[test]
    fn a_source_above_the_viewers_max_is_transcoded_at_that_max() {
        match decide(2_000_000, 4_700_000) {
            Decision::Transcode {
                max_video_bitrate_bps,
                ..
            } => assert_eq!(max_video_bitrate_bps, Some(2_000_000)),
            other => panic!("expected a capped transcode, got {other:?}"),
        }
    }

    #[test]
    fn auto_and_a_source_under_the_max_keep_the_copy() {
        assert!(matches!(decide(0, 4_700_000), Decision::AudioRemux { .. }));
        assert!(matches!(
            decide(8_000_000, 4_700_000),
            Decision::AudioRemux { .. }
        ));
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod split_film_tests {
    use super::*;
    use crate::state::Stores;
    use pharos_core::{AlternateMediaSource, MediaKind, MediaProbe};
    use pharos_jellyfin_api::device_profile::DirectPlayProfile;

    fn film(parts: usize) -> MediaItem {
        let probe = MediaProbe {
            container: Some("matroska".into()),
            video_codec: Some("h264".into()),
            audio_codec: Some("aac".into()),
            ..Default::default()
        };
        MediaItem {
            id: 1,
            path: "/m/G/G (2003) cd1.mkv".into(),
            kind: MediaKind::Movie,
            probe: MediaProbe {
                alternate_sources: (2..=parts)
                    .map(|n| AlternateMediaSource {
                        id: format!("part-{n}"),
                        path: format!("/m/G/G (2003) cd{n}.mkv").into(),
                        part: u32::try_from(n).ok(),
                        probe: Some(Box::new(probe.clone())),
                        ..Default::default()
                    })
                    .collect(),
                ..probe
            },
            ..Default::default()
        }
    }

    async fn negotiated(item: &MediaItem) -> Negotiated {
        let stores = Stores::connect("sqlite::memory:").await.unwrap();
        let state = AppState::new(stores, "t".into());
        let profile = DeviceProfile {
            direct_play_profiles: vec![DirectPlayProfile {
                container: "mkv,matroska".into(),
                video_codec: "h264".into(),
                audio_codec: "aac".into(),
                kind: "Video".into(),
            }],
            ..Default::default()
        };
        negotiate(
            &state,
            item,
            &NegotiateRequest {
                profile: &profile,
                switches: PlaybackSwitches::default(),
                explicit_subtitle_index: None,
                stereo_mode: None,
                client_is_remote: false,
                is_web_client: false,
            },
        )
    }

    /// V1155 — a client that would direct play the first part's file streams
    /// a split film through HLS instead, and is told why, with the count.
    #[actix_web::test]
    async fn a_split_film_never_direct_plays() {
        let whole = negotiated(&film(1)).await;
        assert!(
            whole.direct_play,
            "control: the same file alone direct plays"
        );

        let split = negotiated(&film(3)).await;
        assert!(!split.direct_play);
        assert!(!split.supports_direct_stream);
        assert_eq!(
            split.direct_play_block,
            DirectPlayBlock::SplitFilm { parts: 3 }
        );
        assert!(split.direct_play_block.to_string().contains('3'));
    }
}
