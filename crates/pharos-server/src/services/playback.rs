//! Playback: the decision a client's profile and switches get for one item
//! (spec 017 US6). `/Items/{id}/PlaybackInfo` and the native
//! `Playback.negotiate` both decide here, so a client cannot be told
//! "direct play" over one API and "transcode" over the other.

use std::collections::BTreeSet;

use pharos_core::{MediaItem, MediaKind, SubtitleTrack};
use pharos_jellyfin_api::device_profile::{
    negotiate_explained, CodecCap, Decision, DeviceProfile, DirectPlayBlock, ServerCodecSupport,
    SourceMedia, SubtitleProfileDto,
};

use crate::api::jellyfin::dto::container_for;
use crate::api::jellyfin::subtitle_delivery::{decide_subtitle_delivery, SubtitleDelivery};
use crate::api::jellyfin::subtitles::is_image_subtitle_codec;
use crate::state::AppState;

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
    pub burning_sub_on_packed_3d: bool,
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
    /// bounded: 4 decisions × 8 block reasons × 5 downgrades (`none` plus the
    /// four in [`Negotiated::downgrade`]).
    pub fn record(&self) {
        metrics::counter!(
            "pharos_playback_decision_total",
            "decision" => self.decision_kind(),
            "direct_play_block" => self.direct_play_block.label(),
            "downgrade" => self.downgrade.unwrap_or("none"),
        )
        .increment(1);
    }
}

/// Decides how `item` plays for the client in `req`: negotiation against its
/// profile, the WAN clamp, the downgrades in the order they apply, the
/// client's own switches last, and the 3D output.
pub fn negotiate(state: &AppState, item: &MediaItem, req: &NegotiateRequest<'_>) -> Negotiated {
    let profile = req.profile;
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
    let (decision, direct_play_block) = if remote {
        let mut forced = profile.clone();
        forced.direct_play_profiles.clear();
        let (d, _) = negotiate_explained(&forced, &source, &server_codecs);
        (
            d,
            DirectPlayBlock::RemoteSource {
                locator: item.path.display().to_string(),
            },
        )
    } else {
        negotiate_explained(profile, &source, &server_codecs)
    };
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
    );
    // B80 (Lace incident, 2026-07-16) — a browser's <video>/MSE cannot demux a
    // raw Matroska container. When the negotiator returned DirectPlay for a
    // Matroska source to a browser that CANNOT play the `video/webm` relabel
    // (e.g. AV1 + AAC, or h264-in-mkv), advertising SupportsDirectStream with NO
    // TranscodingUrl stranded the client: jellyfin-web silently retried
    // PlaybackInfo for ~11 min (Supergirl S01E05, av1-in-mkv) before recovering.
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

    let burning_sub_on_packed_3d = is_video
        && probe
            .video_3d_format
            .is_some_and(|f| f.is_packed_sbs_or_tab())
        && selected_sub.is_some_and(|t| {
            is_image_subtitle_codec(t.codec.as_deref().unwrap_or(""))
                || burn_text_indices.contains(&t.stream_index)
        });
    let stereo_output = if !explicit_preserve
        && (explicit_mono || profile_rejected_3d || burning_sub_on_packed_3d)
    {
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
    let supports_direct_stream = switches.direct_stream
        && switches.video_copy
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
        burning_sub_on_packed_3d,
        stereo_output,
        direct_play,
        supports_direct_stream,
        webm_relabel_playable,
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
    let _ = state
        .transcode_sessions
        .insert(
            play_session_id.to_owned(),
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
        .await;
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
        // asks for more burns. Observed live 2026-08-16 on Black Panther:
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
/// 2026-08-29 (Code Geass: the container-default PGS track was a sparse
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
/// cannot follow without — B44's Avatar case, where the Na'vi sections played
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
        // B107, the exact live case: Cobra Kai S01E01, av1 + opus in a .mkv,
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

    /// B44's Avatar case: default AND forced, i.e. dialogue rather than a
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
        // Black Panther actually presents.
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
