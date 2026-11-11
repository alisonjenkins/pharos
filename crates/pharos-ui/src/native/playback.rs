//! What the watch page decides without touching the browser: which codecs to
//! offer the server, which engine plays the stream, and when to report
//! progress. The browser answers come in as plain values, so all of it is
//! tested on the host.

use pharos_client::api::{
    Capabilities, ClientEvent, ClientEventKind, ClientLevel, Font, Segment, SegmentKind, Track,
    Trickplay,
};

/// Something that turns a stream URL into playback on the page's `<video>`
/// (009 Boundaries: each external dependency sits behind an adapter). The
/// engine is dropped when the player unmounts; dropping detaches it.
pub trait PlaybackEngine {
    /// Start `url` on the `<video>` with id `video_id`, `start_ms` in.
    /// `on_fatal` gets the reason once playback has failed for good.
    fn attach(
        &mut self,
        video_id: &str,
        url: &str,
        start_ms: u64,
        on_fatal: OnFatal,
        on_fragment: OnFragment,
    ) -> Result<(), String>;
}

/// Told why playback stopped for good.
pub type OnFatal = std::rc::Rc<dyn Fn(Fatal)>;

/// What kind of failure stopped a video, which decides the hint its panel
/// gives (038 FR-403, V1219). Each names who can fix it: the connection, the
/// server, the browser or device, or the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    /// The connection: a fetch that failed or timed out, a socket that went.
    Network,
    /// The server answered and refused, or failed: an `Error`, an HTTP 4xx/5xx.
    Server,
    /// This browser or device cannot play what it was sent: a codec it lacks,
    /// an engine that will not attach.
    Unplayable,
    /// The bytes arrived and would not decode: the file is likely damaged.
    Damaged,
}

impl FailureKind {
    /// Stable label for log lines and the panel's `data-kind`.
    pub fn label(self) -> &'static str {
        match self {
            Self::Network => "network",
            Self::Server => "server",
            Self::Unplayable => "unplayable",
            Self::Damaged => "damaged",
        }
    }

    /// An HTTP status that ended a fetch: the server's (4xx, 5xx), else the
    /// network's.
    pub fn of_status(status: u16) -> Self {
        if (400..600).contains(&status) {
            Self::Server
        } else {
            Self::Network
        }
    }

    /// A media error the Rust engine gave up on, by its hls.js-shaped
    /// `details`: bytes that would not decode are damage, a codec the
    /// browser refused is unplayable, and anything else (a stall with data
    /// buffered, a SourceBuffer that would not remove) is neither, so it is
    /// not offered a skip (V1219).
    pub fn of_media_error(details: &str) -> Self {
        match details {
            "bufferAppendError"
            | "bufferAppendingError"
            | "fragParsingError"
            | "videoDecodeError" => Self::Damaged,
            "bufferAddCodecError"
            | "manifestIncompatibleCodecsError"
            | "bufferIncompatibleCodecsError" => Self::Unplayable,
            _ => Self::Network,
        }
    }
}

/// Why playback stopped for good, and what kind of failure it was.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fatal {
    pub reason: String,
    pub kind: FailureKind,
}

impl Fatal {
    /// The media could not be decoded: the file is likely damaged there,
    /// so skipping ahead may get past it (B1160).
    pub fn media(reason: impl Into<String>) -> Self {
        Self::of(FailureKind::Damaged, reason)
    }

    /// The network, or anything not known to be otherwise.
    pub fn other(reason: impl Into<String>) -> Self {
        Self::of(FailureKind::Network, reason)
    }

    pub fn of(kind: FailureKind, reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
            kind,
        }
    }

    /// Whether the media itself broke.
    pub fn is_media(&self) -> bool {
        self.kind == FailureKind::Damaged
    }

    /// hls.js reports media it could not parse or append as a `mediaError`
    /// with these details; a stall, a full buffer or a codec the browser
    /// lacks is a `mediaError` too, but not damage. A codec the browser
    /// lacks is this browser's limit.
    pub fn from_hls(kind: &str, details: &str) -> Self {
        let reason = format!("the stream failed ({kind}: {details})");
        let damage = matches!(
            details,
            "bufferAppendError" | "bufferAppendingError" | "fragParsingError"
        );
        let unplayable = matches!(
            details,
            "manifestIncompatibleCodecsError" | "bufferIncompatibleCodecsError"
        );
        if kind == "mediaError" && damage {
            Self::media(reason)
        } else if unplayable {
            Self::of(FailureKind::Unplayable, reason)
        } else {
            Self::other(reason)
        }
    }
}

/// One video fragment the engine downloaded, for measuring the connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FragmentLoad {
    pub bytes: u64,
    /// From the first byte to the last, so the server's time to start the
    /// response is not mistaken for a slow link.
    pub load_ms: u64,
    /// How much video the fragment holds.
    pub media_ms: u64,
}

/// Told of each video fragment downloaded; engines that cannot see
/// downloads (the browser's own player) never call it.
pub type OnFragment = std::rc::Rc<dyn Fn(FragmentLoad)>;

/// Which engine plays a stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Engine {
    /// A file the `<video>` element plays from its URL.
    Direct,
    /// An HLS playlist the `<video>` element plays itself (Safari).
    NativeHls,
    /// An HLS playlist played through Media Source Extensions by hls.js.
    HlsJs,
    /// An HLS playlist played through Media Source Extensions by Pharos's
    /// own engine (spec 031).
    RustHls,
    /// HLS with neither native support nor MSE.
    Unsupported,
}

impl Engine {
    /// Whether the engine sees each fragment it downloads, which auto quality
    /// needs to judge the connection.
    pub fn reports_fragments(self) -> bool {
        matches!(self, Engine::HlsJs | Engine::RustHls)
    }

    /// Whether the engine picks the quality itself (spec 031 slice 4), so the
    /// watch page's auto quality must stay out of the way.
    pub fn adapts_quality(self) -> bool {
        matches!(self, Engine::RustHls)
    }
}

/// The engine for a stream: native HLS where the browser has it, else the
/// device's chosen MSE engine where Media Source Extensions exist.
pub fn choose_engine(is_hls: bool, native_hls: bool, mse: bool, mse_engine: MseEngine) -> Engine {
    match (is_hls, native_hls, mse) {
        (false, _, _) => Engine::Direct,
        (true, true, _) => Engine::NativeHls,
        (true, false, true) => match mse_engine {
            MseEngine::HlsJs => Engine::HlsJs,
            MseEngine::Rust => Engine::RustHls,
        },
        (true, false, false) => Engine::Unsupported,
    }
}

/// Which engine plays HLS through Media Source Extensions on this device.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MseEngine {
    /// The vendored hls.js (ADR-0035), the default until spec 031 reaches
    /// parity.
    #[default]
    HlsJs,
    /// Pharos's own engine (ADR-0071), opt-in.
    Rust,
}

impl MseEngine {
    pub const ALL: [MseEngine; 2] = [MseEngine::HlsJs, MseEngine::Rust];

    /// The stored and logged name.
    pub fn as_str(self) -> &'static str {
        match self {
            MseEngine::HlsJs => "hlsjs",
            MseEngine::Rust => "rust",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            MseEngine::HlsJs => "hls.js",
            MseEngine::Rust => "Pharos (experimental)",
        }
    }

    pub fn parse(stored: &str) -> Option<MseEngine> {
        MseEngine::ALL
            .into_iter()
            .find(|e| e.as_str() == stored.trim())
    }
}

/// Where this device's [`MseEngine`] choice is kept.
pub const MSE_ENGINE_KEY: &str = "pharos.player.engine";

/// Why an [`MseEngine`] was picked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineReason {
    /// `?engine=` on the page URL, for this page load only.
    Override,
    /// The device's Playback setting.
    Setting,
    /// Nothing usable chosen.
    Default,
}

impl EngineReason {
    pub fn label(self) -> &'static str {
        match self {
            EngineReason::Override => "override",
            EngineReason::Setting => "setting",
            EngineReason::Default => "default",
        }
    }
}

/// The MSE engine for this page load: a valid `engine=` in the page's query
/// `search` wins, then the `stored` setting, then the default (V630).
pub fn pick_mse_engine(search: &str, stored: Option<&str>) -> (MseEngine, EngineReason) {
    let asked = search
        .trim_start_matches('?')
        .split('&')
        .filter_map(|pair| pair.strip_prefix("engine="))
        .find_map(MseEngine::parse);
    if let Some(engine) = asked {
        return (engine, EngineReason::Override);
    }
    match stored.and_then(MseEngine::parse) {
        Some(engine) => (engine, EngineReason::Setting),
        None => (MseEngine::default(), EngineReason::Default),
    }
}

/// Whether [`Engine::RustHls`] can play yet (spec 031 slice 2).
const RUST_HLS_BUILT: bool = true;

/// The HLS engine a play actually uses once `picked` for `reason`: the
/// Rust engine hands over to hls.js until it is built.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EngineVerdict {
    pub engine: Engine,
    pub picked: Engine,
    pub reason: EngineReason,
    /// Why `engine` differs from `picked`, when it does.
    pub fallback: Option<&'static str>,
}

pub fn engine_verdict(picked: Engine, reason: EngineReason) -> EngineVerdict {
    engine_verdict_with(picked, reason, RUST_HLS_BUILT)
}

pub fn engine_verdict_with(
    picked: Engine,
    reason: EngineReason,
    rust_built: bool,
) -> EngineVerdict {
    let (engine, fallback) = match picked {
        Engine::RustHls if !rust_built => (Engine::HlsJs, Some("not_built")),
        other => (other, None),
    };
    EngineVerdict {
        engine,
        picked,
        reason,
        fallback,
    }
}

/// Stable fallback cause when [`Engine::RustHls`] hands a stream to `hls.js`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HlsFallbackCause {
    UnsupportedTag,
    NotFmp4,
    Live,
    Codec,
    NoVariant,
}

impl HlsFallbackCause {
    pub const ALL: [Self; 5] = [
        Self::UnsupportedTag,
        Self::NotFmp4,
        Self::Live,
        Self::Codec,
        Self::NoVariant,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::UnsupportedTag => "unsupported_tag",
            Self::NotFmp4 => "not_fmp4",
            Self::Live => "live",
            Self::Codec => "codec",
            Self::NoVariant => "no_variant",
        }
    }
}

/// Validated playlist and SourceBuffer MIME plan for [`Engine::RustHls`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub video_uri: String,
    pub audio_uri: Option<String>,
    pub video_mime: String,
    pub audio_mime: Option<String>,
}

const VIDEO_CODEC_PREFIXES: &[&str] = &[
    "avc1", "avc3", "hvc1", "hev1", "dvh1", "dvhe", "dva1", "dvav", "vp09", "av01", "mp4v",
];

/// Splits a variant's `CODECS` list into `(video_codecs, audio_codecs)`.
pub fn split_codecs(codecs: &[String]) -> (Vec<String>, Vec<String>) {
    let mut video = Vec::new();
    let mut audio = Vec::new();
    for codec in codecs {
        if VIDEO_CODEC_PREFIXES.iter().any(|p| codec.starts_with(p)) {
            video.push(codec.clone());
        } else {
            audio.push(codec.clone());
        }
    }
    (video, audio)
}

/// Picks the audio rendition URI for `variant` from `master`: the default
/// rendition in `variant.audio_group` with a URI, else the first in that group
/// with a URI.
pub fn pick_audio_uri(
    master: &pharos_hls::playlist::Master,
    variant: &pharos_hls::playlist::Variant,
) -> Option<String> {
    let group_id = variant.audio_group.as_deref()?;
    master
        .audio
        .iter()
        .find(|r| r.group_id == group_id && r.default)
        .and_then(|r| r.uri.clone())
        .or_else(|| {
            master
                .audio
                .iter()
                .find(|r| r.group_id == group_id && r.uri.is_some())
                .and_then(|r| r.uri.clone())
        })
}

fn check_unsupported_tags(
    unsupported: &[pharos_hls::playlist::Unsupported],
) -> Result<(), (HlsFallbackCause, String)> {
    for &item in unsupported {
        if item != pharos_hls::playlist::Unsupported::Live {
            return Err((HlsFallbackCause::UnsupportedTag, item.label().to_owned()));
        }
    }
    Ok(())
}

fn check_media_playlist(
    media: &pharos_hls::playlist::MediaPlaylist,
) -> Result<(), (HlsFallbackCause, String)> {
    check_unsupported_tags(&media.unsupported)?;
    if (!media.vod && !media.ended)
        || media
            .unsupported
            .contains(&pharos_hls::playlist::Unsupported::Live)
    {
        return Err((HlsFallbackCause::Live, "live".to_owned()));
    }
    if media.init_uri.is_none() {
        return Err((HlsFallbackCause::NotFmp4, "none".to_owned()));
    }
    Ok(())
}

/// The master-level part of [`plan_hls`], checked before any media playlist is fetched.
pub fn check_master(
    master: &pharos_hls::playlist::Master,
) -> Result<(), (HlsFallbackCause, String)> {
    check_unsupported_tags(&master.unsupported)?;
    if master
        .unsupported
        .contains(&pharos_hls::playlist::Unsupported::Live)
    {
        return Err((HlsFallbackCause::Live, "live".to_owned()));
    }
    Ok(())
}

/// Validates parsed master and media playlists plus browser codec support for [`Engine::RustHls`].
pub fn plan_hls(
    master: &pharos_hls::playlist::Master,
    video: &pharos_hls::playlist::MediaPlaylist,
    audio: Option<&pharos_hls::playlist::MediaPlaylist>,
    is_type_supported: impl Fn(&str) -> bool,
) -> Result<Plan, (HlsFallbackCause, String)> {
    check_master(master)?;
    let Some(variant) = master.variants.first() else {
        return Err((HlsFallbackCause::NoVariant, "none".to_owned()));
    };

    check_media_playlist(video)?;
    if let Some(audio_pl) = audio {
        check_media_playlist(audio_pl)?;
    }

    let audio_uri = pick_audio_uri(master, variant);
    let (video_mime, audio_mime) = if audio_uri.is_some() {
        let (video_codecs, audio_codecs) = split_codecs(&variant.codecs);
        (
            format!(r#"video/mp4; codecs="{}""#, video_codecs.join(",")),
            Some(format!(r#"audio/mp4; codecs="{}""#, audio_codecs.join(","))),
        )
    } else {
        (
            format!(r#"video/mp4; codecs="{}""#, variant.codecs.join(",")),
            None,
        )
    };

    if !is_type_supported(&video_mime) {
        return Err((HlsFallbackCause::Codec, video_mime));
    }
    if let Some(ref a_mime) = audio_mime {
        if !is_type_supported(a_mime) {
            return Err((HlsFallbackCause::Codec, a_mime.clone()));
        }
    }

    Ok(Plan {
        video_uri: variant.uri.clone(),
        audio_uri,
        video_mime,
        audio_mime,
    })
}

/// Converts `(start_s, end_s)` `TimeRanges` pairs into sorted millisecond ranges.
pub fn ranges_to_ms(ranges_s: &[(f64, f64)]) -> Vec<(u64, u64)> {
    let mut out = Vec::with_capacity(ranges_s.len());
    for &(start_s, end_s) in ranges_s {
        if !start_s.is_finite() || !end_s.is_finite() || end_s <= 0.0 || end_s <= start_s {
            continue;
        }
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let start_ms = (start_s.max(0.0) * 1000.0).round() as u64;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let end_ms = (end_s * 1000.0).round() as u64;
        if start_ms < end_ms {
            out.push((start_ms, end_ms));
        }
    }
    out.sort_unstable_by_key(|&(start, _)| start);
    out
}

/// The master URL the Rust engine asks for: with `abr=1` the server lists the
/// H.264 fMP4 quality ladder (V644), which no other client is sent.
pub fn with_abr(url: &str) -> String {
    let sep = if url.contains('?') { '&' } else { '?' };
    format!("{url}{sep}abr=1")
}

/// One rung the Rust engine can switch to: its media playlist and its place in
/// the ladder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LadderEntry {
    pub uri: String,
    pub rung: pharos_hls::abr::Rung,
}

/// The rungs a switch can move between: the variants carrying the first
/// variant's codecs and audio group, lowest bandwidth first, the order
/// [`pharos_hls::abr::Abr`] indexes them in. A variant on other codecs would
/// need `changeType`, so it is left out.
pub fn abr_ladder(master: &pharos_hls::playlist::Master) -> Vec<LadderEntry> {
    let Some(first) = master.variants.first() else {
        return Vec::new();
    };
    let mut out: Vec<LadderEntry> = master
        .variants
        .iter()
        .filter(|v| v.codecs == first.codecs && v.audio_group == first.audio_group)
        .map(|v| LadderEntry {
            uri: v.uri.clone(),
            rung: pharos_hls::abr::Rung {
                bandwidth_bps: v.bandwidth,
                height: v.resolution.map(|(_, h)| h),
            },
        })
        .collect();
    out.sort_by_key(|e| e.rung.bandwidth_bps);
    out.dedup_by(|a, b| a.uri == b.uri);
    out
}

/// A rung as the switch log names it: its height, else its bandwidth.
pub fn rung_label(rung: &pharos_hls::abr::Rung) -> String {
    match rung.height {
        Some(h) => format!("{h}p"),
        None => format!("{}k", rung.bandwidth_bps / 1000),
    }
}

/// How much media is buffered ahead of `play_ms`: the end of the range that
/// holds it, less `play_ms`; 0 when the play head is in a gap.
pub fn forward_buffer_ms(ranges: &[(u64, u64)], play_ms: u64) -> u64 {
    ranges
        .iter()
        .find(|&&(start, end)| start <= play_ms && play_ms < end)
        .map_or(0, |&(_, end)| end - play_ms)
}

/// One stall verdict of the Rust engine's watchdog, with the play head's
/// surroundings (V702): which tracks had a request in flight, and where each
/// track's buffered data ends past the play head.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StallReport {
    pub verdict: pharos_hls::stall::Verdict,
    pub video_ahead_ms: u64,
    pub audio_ahead_ms: u64,
    pub in_flight: &'static str,
}

impl StallReport {
    /// `(at_ms, to_ms, gap_ms, attempt, stalled_ms)`; `None` where the
    /// verdict has no such value.
    fn fields(&self) -> (u64, Option<u64>, Option<u64>, u32, Option<u64>) {
        use pharos_hls::stall::Verdict as V;
        match self.verdict {
            V::Jump {
                from_ms,
                to_ms,
                gap_ms,
                attempt,
            } => (from_ms, Some(to_ms), Some(gap_ms), attempt, None),
            V::Nudge {
                from_ms,
                to_ms,
                attempt,
                ..
            } => (from_ms, Some(to_ms), None, attempt, None),
            V::Starved { at_ms, stalled_ms } => (at_ms, None, None, 0, Some(stalled_ms)),
            V::GiveUp {
                at_ms,
                stalled_ms,
                attempts,
            }
            | V::Recovered {
                at_ms,
                stalled_ms,
                attempts,
            } => (at_ms, None, None, attempts, Some(stalled_ms)),
        }
    }

    pub fn line(&self) -> String {
        let (at, to, gap, attempt, stalled) = self.fields();
        let opt = |v: Option<u64>| v.map_or_else(|| "none".to_owned(), |v| v.to_string());
        format!(
            "pharos: playback stall engine=rust action={} at_ms={at} to_ms={} gap_ms={} attempt={attempt} stalled_ms={} video_ahead_ms={} audio_ahead_ms={} in_flight={}",
            self.verdict.label(),
            opt(to),
            opt(gap),
            opt(stalled),
            self.video_ahead_ms,
            self.audio_ahead_ms,
            self.in_flight,
        )
    }

    pub fn event(&self, route: String) -> ClientEvent {
        use pharos_hls::stall::Verdict as V;
        let (at, to, gap, attempt, stalled) = self.fields();
        let opt = |v: Option<u64>| v.map_or_else(|| "none".to_owned(), |v| v.to_string());
        ClientEvent {
            kind: ClientEventKind::PlaybackStall,
            level: match self.verdict {
                V::GiveUp { .. } => ClientLevel::Error,
                V::Recovered { .. } => ClientLevel::Info,
                V::Jump { .. } | V::Nudge { .. } | V::Starved { .. } => ClientLevel::Warn,
            },
            route,
            fields: vec![
                ("engine", "rust".to_owned()),
                ("action", self.verdict.label().to_owned()),
                ("at_ms", at.to_string()),
                ("to_ms", opt(to)),
                ("gap_ms", opt(gap)),
                ("attempt", attempt.to_string()),
                ("stalled_ms", opt(stalled)),
                ("video_ahead_ms", self.video_ahead_ms.to_string()),
                ("audio_ahead_ms", self.audio_ahead_ms.to_string()),
                ("in_flight", self.in_flight.to_owned()),
            ],
        }
    }
}

/// One quality switch the Rust engine took, with what it decided on (V649).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AbrSwitchReport {
    pub from: String,
    pub to: String,
    pub reason: &'static str,
    pub estimate_bps: Option<u64>,
    pub buffer_ms: u64,
    pub cold_ms: u64,
    /// The planner took the switch; `false` is a switch it refused.
    pub applied: bool,
}

impl AbrSwitchReport {
    pub fn line(&self) -> String {
        let est = self
            .estimate_bps
            .map_or_else(|| "none".to_owned(), |b| (b / 1000).to_string());
        format!(
            "pharos: abr switch from={} to={} reason={} est_kbps={est} buffer_ms={} cold_ms={} applied={}",
            self.from, self.to, self.reason, self.buffer_ms, self.cold_ms, self.applied
        )
    }

    pub fn event(&self, route: String) -> ClientEvent {
        let est = self
            .estimate_bps
            .map_or_else(|| "none".to_owned(), |b| (b / 1000).to_string());
        ClientEvent {
            kind: ClientEventKind::QualitySwitch,
            level: if self.applied {
                ClientLevel::Info
            } else {
                ClientLevel::Warn
            },
            route,
            fields: vec![
                ("engine", "rust".to_owned()),
                ("from", self.from.clone()),
                ("to", self.to.clone()),
                ("reason", self.reason.to_owned()),
                ("est_kbps", est),
                ("buffer_ms", self.buffer_ms.to_string()),
                ("cold_ms", self.cold_ms.to_string()),
                ("applied", self.applied.to_string()),
            ],
        }
    }
}

/// Segment deadlines for the Rust engine on `device` (FR-005). Provisional,
/// not measured: hls.js's ten-second first byte on a desktop, longer on a
/// TV or phone whose Wi-Fi and CPU answer slower; the slice 0 baseline
/// (T031-03) retunes them.
pub fn segment_deadlines(device: DeviceClass) -> pharos_hls::retry::Deadlines {
    use pharos_hls::retry::Deadlines;
    match device {
        DeviceClass::Desktop => Deadlines {
            first_byte_ms: 10_000,
            load_ms: 60_000,
        },
        DeviceClass::Tv | DeviceClass::Phone => Deadlines {
            first_byte_ms: 15_000,
            load_ms: 90_000,
        },
    }
}

/// What the Rust engine does about one failed segment request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegmentRecovery {
    /// Fetch it again after `after_ms`; `attempt` failures so far.
    Retry { attempt: u32, after_ms: u64 },
    /// It missed its load deadline: fetch it again at once from a lower rung,
    /// without counting towards the retry limit.
    StepDown,
    /// Give up: playback fails with a `networkError`.
    Fail {
        attempt: u32,
        why: pharos_hls::retry::GiveUp,
    },
}

impl From<pharos_hls::retry::Verdict> for SegmentRecovery {
    fn from(verdict: pharos_hls::retry::Verdict) -> Self {
        use pharos_hls::retry::Verdict;
        match verdict {
            Verdict::Retry { attempt, after_ms } => Self::Retry { attempt, after_ms },
            Verdict::Fail { attempt, why } => Self::Fail { attempt, why },
        }
    }
}

impl SegmentRecovery {
    fn action(self) -> &'static str {
        match self {
            Self::Retry { .. } => "retry",
            Self::StepDown => "step_down",
            Self::Fail { .. } => "fail",
        }
    }

    fn attempt(self) -> u32 {
        match self {
            Self::Retry { attempt, .. } | Self::Fail { attempt, .. } => attempt,
            Self::StepDown => 0,
        }
    }

    fn after_ms(self) -> String {
        match self {
            Self::Retry { after_ms, .. } => after_ms.to_string(),
            Self::StepDown => "0".to_owned(),
            Self::Fail { .. } => "none".to_owned(),
        }
    }

    fn give_up(self) -> &'static str {
        match self {
            Self::Fail { why, .. } => why.label(),
            Self::Retry { .. } | Self::StepDown => "none",
        }
    }
}

/// One failed segment request of the Rust engine and what it led to (V637).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SegmentFailureReport {
    pub track: &'static str,
    /// The segment index, or `init`.
    pub index: String,
    pub cause: pharos_hls::retry::Cause,
    /// The deadline that passed, for a timeout.
    pub deadline_ms: Option<u64>,
    pub recovery: SegmentRecovery,
    /// The quality rung the request was for, `none` without a ladder.
    pub rung: String,
    pub reason: String,
    pub url: String,
}

impl SegmentFailureReport {
    pub fn fatal(&self) -> bool {
        matches!(self.recovery, SegmentRecovery::Fail { .. })
    }

    fn status(&self) -> String {
        match self.cause {
            pharos_hls::retry::Cause::Http(status) => status.to_string(),
            _ => "none".to_owned(),
        }
    }

    fn deadline(&self) -> String {
        self.deadline_ms
            .map_or_else(|| "none".to_owned(), |d| d.to_string())
    }

    pub fn line(&self) -> String {
        let r = self.recovery;
        format!(
            "pharos: playback error engine=rust type=network track={} index={} cause={} status={} deadline_ms={} rung={} action={} attempt={} after_ms={} give_up={} reason={} url={}",
            self.track,
            self.index,
            self.cause.label(),
            self.status(),
            self.deadline(),
            self.rung,
            r.action(),
            r.attempt(),
            r.after_ms(),
            r.give_up(),
            self.reason,
            self.url,
        )
    }

    /// The client event, in the `networkError` vocabulary hls.js reports in (FR-006).
    pub fn event(&self, route: String) -> ClientEvent {
        let r = self.recovery;
        ClientEvent {
            kind: ClientEventKind::PlaybackError,
            level: if self.fatal() {
                ClientLevel::Error
            } else {
                ClientLevel::Warn
            },
            route,
            fields: vec![
                ("engine", "rust".to_owned()),
                ("type", "networkError".to_owned()),
                ("details", self.cause.label().to_owned()),
                ("track", self.track.to_owned()),
                ("index", self.index.clone()),
                ("status", self.status()),
                ("deadline_ms", self.deadline()),
                ("rung", self.rung.clone()),
                ("reason", self.reason.clone()),
                ("action", r.action().to_owned()),
                ("attempt", r.attempt().to_string()),
                ("after_ms", r.after_ms()),
                ("give_up", r.give_up().to_owned()),
            ],
        }
    }
}

impl EngineVerdict {
    /// The watch page's log line for this verdict (V630).
    pub fn line(&self) -> String {
        match self.fallback {
            Some(cause) => format!(
                "pharos: playback engine={} reason=fallback cause={cause} asked={} asked_by={}",
                engine_label(self.engine),
                engine_label(self.picked),
                self.reason.label()
            ),
            None => format!(
                "pharos: playback engine={} reason={}",
                engine_label(self.engine),
                self.reason.label()
            ),
        }
    }

    /// The `rendererMode` client event naming the HLS engine a play uses
    /// and why (V630).
    pub fn report(&self) -> ClientEvent {
        let mut fields = vec![
            ("renderer", "hls".to_owned()),
            ("mode", engine_label(self.engine).to_owned()),
            ("asked", engine_label(self.picked).to_owned()),
            ("asked_by", self.reason.label().to_owned()),
        ];
        match self.fallback {
            Some(cause) => {
                fields.push(("reason", "fallback".to_owned()));
                fields.push(("cause", cause.to_owned()));
            }
            None => fields.push(("reason", self.reason.label().to_owned())),
        }
        ClientEvent {
            kind: ClientEventKind::RendererMode,
            level: ClientLevel::Info,
            route: String::new(),
            fields,
        }
    }
}

/// The logged name of `engine`.
pub fn engine_label(engine: Engine) -> &'static str {
    match engine {
        Engine::Direct => "direct",
        Engine::NativeHls => "native",
        Engine::HlsJs => "hlsjs",
        Engine::RustHls => "rust",
        Engine::Unsupported => "unsupported",
    }
}

/// Server codec name → the MIME type a browser is asked about. Codecs are
/// probed in fragmented MP4, the packaging pharos's HLS segments use, so the
/// answer holds for direct play and HLS alike.
const VIDEO_CODECS: &[(&str, &str)] = &[
    ("h264", r#"video/mp4; codecs="avc1.640028""#),
    ("hevc", r#"video/mp4; codecs="hvc1.1.6.L120.90""#),
    ("vp9", r#"video/mp4; codecs="vp09.00.40.08""#),
    ("av1", r#"video/mp4; codecs="av01.0.08M.08""#),
];

const AUDIO_CODECS: &[(&str, &str)] = &[
    ("aac", r#"audio/mp4; codecs="mp4a.40.2""#),
    ("opus", r#"audio/mp4; codecs="opus""#),
    ("flac", r#"audio/mp4; codecs="flac""#),
    ("mp3", "audio/mpeg"),
    ("ac3", r#"audio/mp4; codecs="ac-3""#),
    ("eac3", r#"audio/mp4; codecs="ec-3""#),
];

/// Containers a `<video>` element opens directly.
const CONTAINERS: &[(&str, &str)] = &[
    ("mp4", "video/mp4"),
    ("m4v", "video/mp4"),
    ("mov", "video/quicktime"),
    ("webm", "video/webm"),
    ("mkv", "video/x-matroska"),
];

/// The capabilities to negotiate with, from the browser's answers: `codec`
/// for a codec MIME type, `container` for a container one.
pub fn capabilities(
    codec: impl Fn(&str) -> bool,
    container: impl Fn(&str) -> bool,
) -> Capabilities {
    let pick = |table: &[(&str, &str)], ok: &dyn Fn(&str) -> bool| -> Vec<String> {
        table
            .iter()
            .filter(|(_, mime)| ok(mime))
            .map(|(name, _)| (*name).to_owned())
            .collect()
    };
    Capabilities {
        video_codecs: pick(VIDEO_CODECS, &codec),
        audio_codecs: pick(AUDIO_CODECS, &codec),
        containers: pick(CONTAINERS, &container),
        max_bitrate: 0,
        hdr: false,
        image_subtitles: IMAGE_SUBTITLES.iter().map(|c| (*c).to_owned()).collect(),
    }
}

/// The image subtitle codecs the player draws (spec 019): the server sends
/// each event as a PNG, which every browser shows.
pub const IMAGE_SUBTITLES: &[&str] = &["dvd_subtitle", "hdmv_pgs_subtitle", "dvb_subtitle"];

/// An image subtitle track as the server's manifest lists it: every event
/// decoded so far, on a `width` x `height` canvas.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Deserialize)]
pub struct ImageCues {
    pub width: u32,
    pub height: u32,
    /// The whole track is listed; until then the player polls.
    pub complete: bool,
    pub events: Vec<ImageCue>,
}

/// One subtitle image and where it sits on the canvas.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct ImageCue {
    pub start_ms: u64,
    pub end_ms: u64,
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    pub url: String,
}

/// The images on screen at `ms`.
pub fn cues_at(cues: &ImageCues, ms: u64) -> impl Iterator<Item = &ImageCue> {
    cues.events
        .iter()
        .filter(move |c| c.start_ms <= ms && ms < c.end_ms)
}

/// The font size in px pharos.css gives a cue in a player `width_px` wide
/// in a viewport `viewport_h_px` tall at size `scale` (V455): `scale x
/// max(14px, min(2.8cqw, 5vh))`, without the 14px floor in the `mini` player.
pub fn cue_font_px(width_px: f64, viewport_h_px: f64, scale: f64, mini: bool) -> f64 {
    let fluid = (width_px * 0.028).min(viewport_h_px * 0.05);
    scale * if mini { fluid } else { fluid.max(14.0) }
}

/// The share of the picture's width, in percent, a horizontal cue's text may
/// span: its `size`, narrowed by its `position` (`None` for `auto`) and
/// `position_align`, which place the box so it never crosses the picture's
/// edge (WebVTT: a centred cue at 10% spans at most 20%). `align` decides
/// `auto` for both.
pub fn cue_width_share(size: f64, position: Option<f64>, position_align: &str, align: &str) -> f64 {
    let anchor = match position_align {
        "line-left" => "line-left",
        "line-right" => "line-right",
        "center" => "center",
        _ => match align {
            "left" | "start" => "line-left",
            "right" | "end" => "line-right",
            _ => "center",
        },
    };
    // WebVTT's computed position: only `left` and `right` move `auto` off
    // the middle; `start` and `end` anchor a box that starts at 50%.
    let position = position.unwrap_or(match align {
        "left" => 0.0,
        "right" => 100.0,
        _ => 50.0,
    });
    let position = position.clamp(0.0, 100.0);
    let room = match anchor {
        "line-left" => 100.0 - position,
        "line-right" => position,
        _ => 2.0 * position.min(100.0 - position),
    };
    size.clamp(0.0, 100.0).min(room)
}

/// About how many characters fit on one rendered cue line in a cue box
/// `width_px` wide (the picture's width times the cue's `size`) at `font_px`,
/// erring low: the browser does not say where
/// it wraps, and a cue lifted a line too far is better than one on the edge.
/// A cue box spans at most the picture, less its padding, and 0.6em is wider
/// than the average subtitle glyph.
pub fn cue_chars_per_line(width_px: f64, font_px: f64) -> usize {
    if font_px <= 0.0 || width_px <= 0.0 {
        return usize::MAX;
    }
    ((width_px * 0.9) / (font_px * 0.6)).floor().max(1.0) as usize
}

/// The `line` a horizontal cue whose `line` is `auto` is moved to (V455):
/// one line clear of the picture's bottom edge. A snap-to-lines cue with a
/// negative `line` puts its first line that many lines from the bottom, so a
/// cue of `n` rendered lines needs `-(n + 1)` to end one line up rather than
/// on the edge. Each of the cue's own lines counts as the rendered lines it
/// wraps to at `chars_per_line` (`cue_chars_per_line`), its markup tags not
/// counted (B545).
pub fn lifted_line(cue_text: &str, chars_per_line: usize) -> f64 {
    let per_line = chars_per_line.max(1);
    let lines: usize = cue_text
        .lines()
        .map(|line| wrapped_lines(&visible_text(line), per_line))
        .sum();
    -(lines.max(1) as f64 + 1.0)
}

/// The rendered lines `text` wraps to at `per_line` characters, wrapped as
/// a browser does, at spaces: a word that does not fit on the current line
/// starts the next, and a word longer than a line breaks across lines. A
/// fullwidth (CJK, Hangul) character counts as two (`cell_width`).
fn wrapped_lines(text: &str, per_line: usize) -> usize {
    let mut lines = 0;
    let mut used = 0;
    for word in text.split_whitespace() {
        let len: usize = word.chars().map(cell_width).sum();
        if used > 0 && used + 1 + len <= per_line {
            used += 1 + len;
        } else {
            lines += len.div_ceil(per_line).max(1);
            used = match len % per_line {
                0 => per_line,
                rest => rest,
            };
        }
    }
    lines
}

/// The character cells `c` takes in `wrapped_lines`: a fullwidth glyph (CJK,
/// kana, Hangul, fullwidth forms) is about 1em wide, against the 0.6em
/// `cue_chars_per_line` allows a cell, so it counts as two; 2 cells (1.2em)
/// still errs towards more lines.
fn cell_width(c: char) -> usize {
    match c as u32 {
        0x1100..=0x115F
        | 0x2E80..=0xA4CF
        | 0xAC00..=0xD7A3
        | 0xF900..=0xFAFF
        | 0xFE30..=0xFE6F
        | 0xFF00..=0xFF60
        | 0xFFE0..=0xFFE6
        | 0x20000..=0x3FFFD => 2,
        _ => 1,
    }
}

/// A cue text line without its `<...>` tags.
fn visible_text(line: &str) -> String {
    let mut in_tag = false;
    line.chars()
        .filter(|&c| match c {
            '<' => {
                in_tag = true;
                false
            }
            '>' if in_tag => {
                in_tag = false;
                false
            }
            _ => !in_tag,
        })
        .collect()
}

/// `part` of `whole` in percent, for placing a cue on the canvas.
pub fn share(part: u32, whole: u32) -> f64 {
    match whole {
        0 => 0.0,
        w => f64::from(part) * 100.0 / f64::from(w),
    }
}

/// The `<video>` as the controls show it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MediaState {
    pub position_ms: u64,
    /// 0 until the metadata has loaded.
    pub duration_ms: u64,
    pub paused: bool,
    pub volume: f64,
    pub muted: bool,
    pub loading: bool,
    /// The picture's display size; 0 until the metadata has loaded.
    pub video_width: u32,
    pub video_height: u32,
}

impl Default for MediaState {
    fn default() -> Self {
        Self {
            position_ms: 0,
            duration_ms: 0,
            paused: true,
            volume: 1.0,
            muted: false,
            loading: false,
            video_width: 0,
            video_height: 0,
        }
    }
}

/// `HTMLMediaElement.HAVE_FUTURE_DATA`: enough data for the current frame and
/// at least a little ahead.
const HAVE_FUTURE_DATA: u16 = 3;

/// Whether the `<video>` is waiting on data: while seeking (paused or not), or
/// while unpaused without future data to play.
pub fn is_loading(seeking: bool, ready_state: u16, paused: bool, ended: bool) -> bool {
    seeking || (ready_state < HAVE_FUTURE_DATA && !ended && (!paused || seeking))
}

impl MediaState {
    /// A new player's state: at the viewer's saved volume and mute, so its
    /// controls never draw the fresh element's full volume first.
    pub fn at_volume(saved: Option<(f64, bool)>) -> Self {
        match saved {
            Some((volume, muted)) => Self {
                volume,
                muted,
                ..Self::default()
            },
            None => Self::default(),
        }
    }
}

/// Where the viewer's volume and mute are kept, so the next player (each
/// track and each renegotiated stream mounts a new media element) starts
/// where they left it rather than at full volume.
pub const VOLUME_KEY: &str = "pharos.volume";

/// `volume` and `muted` as stored under [`VOLUME_KEY`].
pub fn volume_setting(volume: f64, muted: bool) -> String {
    let muted = if muted { " muted" } else { "" };
    format!("{:.3}{muted}", volume.clamp(0.0, 1.0))
}

/// The volume and mute a [`volume_setting`] holds; `None` for anything else.
pub fn parse_volume_setting(stored: &str) -> Option<(f64, bool)> {
    let (level, muted) = match stored.strip_suffix(" muted") {
        Some(level) => (level, true),
        None => (stored, false),
    };
    let volume = level
        .parse::<f64>()
        .ok()
        .filter(|v| (0.0..=1.0).contains(v))?;
    Some((volume, muted))
}

/// Where the listener's crossfade length is kept, in whole seconds; absent
/// or `0` is off, the default (opt-in).
pub const CROSSFADE_KEY: &str = "pharos.player.crossfade";

/// The crossfade lengths offered, in seconds; `0` is off.
pub const CROSSFADE_CHOICES: [u32; 6] = [0, 2, 4, 6, 8, 12];

/// The crossfade length a stored [`CROSSFADE_KEY`] value holds, in seconds;
/// anything that is not one of [`CROSSFADE_CHOICES`] is off.
pub fn parse_crossfade(stored: &str) -> u32 {
    let stored = stored.trim();
    // `u32::from_str` takes a leading `+`, which this app never writes.
    if !stored.bytes().all(|b| b.is_ascii_digit()) {
        return 0;
    }
    stored
        .parse::<u32>()
        .ok()
        .filter(|s| CROSSFADE_CHOICES.contains(s))
        .unwrap_or(0)
}

/// Where the viewer's maximum video bitrate is kept, in bits per second;
/// absent or `0` is Auto (no limit), the default.
pub const MAX_BITRATE_KEY: &str = "pharos.player.max_bitrate";

/// The maximum qualities offered, as (bits per second, label); `0` is Auto.
pub const MAX_BITRATE_CHOICES: [(u64, &str); 5] = [
    (0, "Auto"),
    (8_000_000, "8 Mbps"),
    (4_000_000, "4 Mbps"),
    (2_000_000, "2 Mbps"),
    (1_000_000, "1 Mbps"),
];

/// The limit a stored [`MAX_BITRATE_KEY`] value holds; anything that is not
/// one of [`MAX_BITRATE_CHOICES`] is Auto.
pub fn parse_max_bitrate(stored: &str) -> u64 {
    let stored = stored.trim();
    if !stored.bytes().all(|b| b.is_ascii_digit()) {
        return 0;
    }
    stored
        .parse::<u64>()
        .ok()
        .filter(|bps| MAX_BITRATE_CHOICES.iter().any(|(c, _)| c == bps))
        .unwrap_or(0)
}

/// The `playback max_bitrate=` log line's value.
pub fn max_bitrate_label(bps: u64) -> String {
    if bps == 0 {
        "auto".to_owned()
    } else {
        bps.to_string()
    }
}

/// Where the viewer's subtitle size choice is kept; absent or unknown is
/// [`SubtitleSize::Medium`].
pub const SUBTITLE_SIZE_KEY: &str = "pharos.subtitles.size";

/// How large browser-native subtitles are drawn on this device.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum SubtitleSize {
    Small,
    #[default]
    Medium,
    Large,
    ExtraLarge,
}

impl SubtitleSize {
    /// Every size offered, smallest to largest.
    pub const ALL: [SubtitleSize; 4] = [
        SubtitleSize::Small,
        SubtitleSize::Medium,
        SubtitleSize::Large,
        SubtitleSize::ExtraLarge,
    ];

    /// The size a stored [`SUBTITLE_SIZE_KEY`] value holds; anything unknown
    /// is [`SubtitleSize::Medium`].
    pub fn parse(stored: &str) -> SubtitleSize {
        match stored.trim() {
            "small" => SubtitleSize::Small,
            "large" => SubtitleSize::Large,
            "extra-large" => SubtitleSize::ExtraLarge,
            _ => SubtitleSize::Medium,
        }
    }

    /// Both the stored value and the `data-subtitle-size` attribute on `<html>`.
    pub fn as_str(self) -> &'static str {
        match self {
            SubtitleSize::Small => "small",
            SubtitleSize::Medium => "medium",
            SubtitleSize::Large => "large",
            SubtitleSize::ExtraLarge => "extra-large",
        }
    }

    /// How much of the base text height the Rust subtitle renderer draws
    /// (spec 026 US1: 75/100/125/150%).
    pub fn scale(self) -> f32 {
        match self {
            SubtitleSize::Small => 0.75,
            SubtitleSize::Medium => 1.0,
            SubtitleSize::Large => 1.25,
            SubtitleSize::ExtraLarge => 1.5,
        }
    }

    /// [`scale`](Self::scale) in whole percent: the `?size=` the generated
    /// styled track is compiled at (spec 028 FR-022).
    pub fn percent(self) -> u16 {
        (self.scale() * 100.0).round() as u16
    }

    /// The option text shown in the Appearance picker.
    pub fn label(self) -> &'static str {
        match self {
            SubtitleSize::Small => "Small",
            SubtitleSize::Medium => "Medium",
            SubtitleSize::Large => "Large",
            SubtitleSize::ExtraLarge => "Extra large",
        }
    }
}

/// Where the viewer's subtitle effects choice is kept; absent or unknown is
/// [`EffectsLevel::Subtle`].
pub const EFFECTS_LEVEL_KEY: &str = "pharos.subtitles.effects";

/// Where the viewer's speaker colours switch is kept: `on`, else off.
pub const SPEAKER_COLOURS_KEY: &str = "pharos.subtitles.speakers";

/// Where the viewer's subtitle effects detail is kept; absent or unknown is
/// [`EffectsDetail::Auto`].
pub const EFFECTS_DETAIL_KEY: &str = "pharos.subtitles.detail";

/// How rich a tier a TV or phone asks for. Auto takes the device's tier;
/// Rich asks for the desktop tier everywhere and leaves the frame-drop watch
/// (T028-21) to step it down where the device cannot keep up.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum EffectsDetail {
    #[default]
    Auto,
    Rich,
}

impl EffectsDetail {
    /// Every choice offered, the default first.
    pub const ALL: [EffectsDetail; 2] = [EffectsDetail::Auto, EffectsDetail::Rich];

    /// The choice a stored [`EFFECTS_DETAIL_KEY`] value holds; anything
    /// unknown is [`EffectsDetail::Auto`].
    pub fn parse(stored: &str) -> EffectsDetail {
        match stored.trim() {
            "rich" => EffectsDetail::Rich,
            _ => EffectsDetail::Auto,
        }
    }

    /// The stored value.
    pub fn as_str(self) -> &'static str {
        match self {
            EffectsDetail::Auto => "auto",
            EffectsDetail::Rich => "rich",
        }
    }

    /// The setting's label.
    pub fn label(self) -> &'static str {
        match self {
            EffectsDetail::Auto => "Auto",
            EffectsDetail::Rich => "Rich",
        }
    }
}

/// How much motion the styled subtitle track may carry on this device
/// (spec 028 FR-018).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum EffectsLevel {
    Off,
    #[default]
    Subtle,
    Full,
}

impl EffectsLevel {
    /// Every level offered, calmest first.
    pub const ALL: [EffectsLevel; 3] =
        [EffectsLevel::Off, EffectsLevel::Subtle, EffectsLevel::Full];

    /// The level a stored [`EFFECTS_LEVEL_KEY`] value holds; anything unknown
    /// is [`EffectsLevel::Subtle`].
    pub fn parse(stored: &str) -> EffectsLevel {
        match stored.trim() {
            "off" => EffectsLevel::Off,
            "full" => EffectsLevel::Full,
            _ => EffectsLevel::Subtle,
        }
    }

    /// The stored value.
    pub fn as_str(self) -> &'static str {
        match self {
            EffectsLevel::Off => "off",
            EffectsLevel::Subtle => "subtle",
            EffectsLevel::Full => "full",
        }
    }

    /// The option text shown in the Appearance picker.
    pub fn label(self) -> &'static str {
        match self {
            EffectsLevel::Off => "Off",
            EffectsLevel::Subtle => "Subtle",
            EffectsLevel::Full => "Full",
        }
    }
}

/// How much animation a styled subtitle track may carry (spec 028 FR-006),
/// cheapest first so a cap is `min`. `label` is the server's `?tier=` value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Tier {
    Low,
    Tv,
    Desktop,
}

impl Tier {
    /// Every tier, cheapest first.
    pub const ALL: [Tier; 3] = [Tier::Low, Tier::Tv, Tier::Desktop];

    pub fn label(self) -> &'static str {
        match self {
            Tier::Low => "low",
            Tier::Tv => "tv",
            Tier::Desktop => "desktop",
        }
    }

    /// The tier a stored [`TIER_DROP_KEY`] value names; anything unknown is
    /// `None`, as if nothing had been dropped to.
    pub fn parse(stored: &str) -> Option<Tier> {
        Tier::ALL.into_iter().find(|t| t.label() == stored.trim())
    }
}

/// Where the tier this device last dropped to is kept; absent or unknown
/// means no cap. Versioned: a cap under an older key came from a detector that
/// charged every dropped video frame to the subtitles (B666, v1) or judged
/// windows too short to tell (B1357, v2), so it is not trusted.
pub const TIER_DROP_KEY: &str = "pharos.subtitles.tier_drop.v3";

/// What kind of screen the page runs on, for choosing a [`Tier`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceClass {
    Tv,
    Phone,
    Desktop,
}

impl DeviceClass {
    pub fn label(self) -> &'static str {
        match self {
            DeviceClass::Tv => "tv",
            DeviceClass::Phone => "phone",
            DeviceClass::Desktop => "desktop",
        }
    }
}

/// User agent tokens that mark a TV browser (spec 028 plan, Client tier choice).
const TV_AGENT_TOKENS: [&str; 6] = [
    "Android TV",
    "GoogleTV",
    "AFT",
    "BRAVIA",
    "SMART-TV",
    "Tizen",
];

/// The device class: a TV user agent or the Android shell is a TV, a coarse
/// pointer on a narrow screen is a phone, anything else a desktop.
pub fn device_class(
    user_agent: &str,
    coarse_pointer: bool,
    narrow_screen: bool,
    android_shell: bool,
) -> DeviceClass {
    if android_shell
        || TV_AGENT_TOKENS
            .iter()
            .any(|t| has_word_start(user_agent, t))
    {
        DeviceClass::Tv
    } else if coarse_pointer && narrow_screen {
        DeviceClass::Phone
    } else {
        DeviceClass::Desktop
    }
}

/// Whether `token` occurs in `text` at the start of a word, so the short
/// `AFT` (Fire TV models: AFTMM, AFTS) does not match inside another word.
fn has_word_start(text: &str, token: &str) -> bool {
    text.match_indices(token).any(|(at, _)| {
        text[..at]
            .chars()
            .next_back()
            .map_or(true, |c| !c.is_ascii_alphanumeric())
    })
}

/// The tier a styled track is first fetched at: Low under reduced motion,
/// else Desktop for desktops and for any device set to Rich, else Tv, capped
/// at the tier this device last dropped to.
pub fn starting_tier(
    device: DeviceClass,
    reduced_motion: bool,
    remembered: Option<Tier>,
    detail: EffectsDetail,
) -> Tier {
    let by_device = if reduced_motion {
        Tier::Low
    } else {
        match (device, detail) {
            (DeviceClass::Desktop, _) | (_, EffectsDetail::Rich) => Tier::Desktop,
            (DeviceClass::Tv | DeviceClass::Phone, EffectsDetail::Auto) => Tier::Tv,
        }
    };
    remembered.map_or(by_device, |cap| by_device.min(cap))
}

/// The log line that records why a starting tier was chosen.
pub fn starting_tier_line(
    tier: Tier,
    device: DeviceClass,
    reduced_motion: bool,
    remembered: Option<Tier>,
    detail: EffectsDetail,
) -> String {
    format!(
        "pharos: subtitle tier={} device={} reduced_motion={reduced_motion} remembered={} detail={}",
        tier.label(),
        device.label(),
        remembered.map_or("none", Tier::label),
        detail.as_str(),
    )
}

/// A frame-drop measurement window (spec 028 FR-019).
const TIER_DROP_WINDOW_MS: u64 = 10_000;
/// The fewest ms between two drops, so a drop gets time to show its effect.
const TIER_DROP_COOLDOWN_MS: u64 = 30_000;
/// Dropped frames over presented frames, in percent, past which a window is over budget.
const TIER_DROP_BUDGET_PERCENT: u128 = 5;
/// The fewest video frames a window needs before its drops count: half a
/// 24 fps window, so a stall or restart's few frames cannot trip a drop.
const TIER_DROP_MIN_FRAMES: u64 = 120;

/// One tier lower and why: the measured window behind it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TierDropEvent {
    pub from: Tier,
    pub to: Tier,
    pub dropped: u64,
    pub presented: u64,
    pub window_ms: u64,
    /// The subtitle renderer's own skipped and drawn frames over the window,
    /// when it counts them.
    pub renderer: Option<RendererFrames>,
}

/// A subtitle renderer's running frame counters: frames it skipped because
/// it was still drawing the last one, and frames it drew.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RendererFrames {
    pub skipped: u64,
    pub drawn: u64,
}

/// What one quality sample decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TierVerdict {
    /// Nothing to do: within budget, mid-window or cooling down.
    Hold,
    /// The video dropped frames over budget but the subtitle renderer kept
    /// up, so the cost is the video's own and the tier stays (B667).
    Spared(TierDropEvent),
    /// One tier lower.
    Drop(TierDropEvent),
}

/// Watches `getVideoPlaybackQuality` counters and says when the styled track
/// should fall one tier (spec 028 FR-019): over budget across a full 10 s
/// window, at most once per 30 s, never below Low. When the renderer counts
/// its own frames, a window only drops if the renderer was also skipping
/// frames over budget: a video that drops frames on its own (a weak decoder)
/// keeps its effects (B667, V617).
#[derive(Debug)]
pub struct TierDrop {
    tier: Tier,
    samples: std::collections::VecDeque<QualitySample>,
    last_drop_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy)]
struct QualitySample {
    at_ms: u64,
    dropped: u64,
    presented: u64,
    renderer: Option<RendererFrames>,
}

/// Whether `dropped` frames out of `total` is over the drop budget.
fn over_budget(dropped: u64, total: u64) -> bool {
    u128::from(dropped).saturating_mul(100)
        > u128::from(total).saturating_mul(TIER_DROP_BUDGET_PERCENT)
}

impl TierDrop {
    pub fn new(tier: Tier) -> Self {
        TierDrop {
            tier,
            samples: std::collections::VecDeque::new(),
            last_drop_ms: None,
        }
    }

    /// The tier the styled track is on now.
    pub fn tier(&self) -> Tier {
        self.tier
    }

    /// Feed the video's running counters; a drop comes back when one is due.
    /// `counting` is false while the page is hidden or the video is paused or
    /// stalled: such a sample says nothing about the renderer, so it only
    /// restarts the window. With no renderer counters every video drop is
    /// charged to the subtitles.
    pub fn sample(
        &mut self,
        now_ms: u64,
        dropped_total: u64,
        presented_total: u64,
        counting: bool,
    ) -> Option<TierDropEvent> {
        match self.judge(now_ms, dropped_total, presented_total, None, counting) {
            TierVerdict::Drop(e) => Some(e),
            TierVerdict::Hold | TierVerdict::Spared(_) => None,
        }
    }

    /// [`TierDrop::sample`] with the subtitle renderer's own counters, and
    /// the verdict in full.
    pub fn judge(
        &mut self,
        now_ms: u64,
        dropped_total: u64,
        presented_total: u64,
        renderer: Option<RendererFrames>,
        counting: bool,
    ) -> TierVerdict {
        if !counting {
            self.samples.clear();
            return TierVerdict::Hold;
        }
        let sample = QualitySample {
            at_ms: now_ms,
            dropped: dropped_total,
            presented: presented_total,
            renderer,
        };
        if self.samples.back().is_some_and(|last| {
            now_ms < last.at_ms
                || dropped_total < last.dropped
                || presented_total < last.presented
                || match (last.renderer, renderer) {
                    (Some(a), Some(b)) => b.skipped < a.skipped || b.drawn < a.drawn,
                    (None, None) => false,
                    // A renderer that starts or stops counting: start over.
                    _ => true,
                }
        }) {
            self.samples.clear();
        }
        self.samples.push_back(sample);
        // Keep one sample at or before the window's start as its baseline.
        let window_start = now_ms.saturating_sub(TIER_DROP_WINDOW_MS);
        while self
            .samples
            .get(1)
            .is_some_and(|second| second.at_ms <= window_start)
        {
            self.samples.pop_front();
        }
        let Some(&oldest) = self.samples.front() else {
            return TierVerdict::Hold;
        };
        let window_ms = now_ms.saturating_sub(oldest.at_ms);
        let dropped = dropped_total.saturating_sub(oldest.dropped);
        let presented = presented_total.saturating_sub(oldest.presented);
        let in_window = match (oldest.renderer, renderer) {
            (Some(a), Some(b)) => Some(RendererFrames {
                skipped: b.skipped.saturating_sub(a.skipped),
                drawn: b.drawn.saturating_sub(a.drawn),
            }),
            _ => None,
        };
        let video_over = window_ms >= TIER_DROP_WINDOW_MS
            && presented.max(dropped) >= TIER_DROP_MIN_FRAMES
            && over_budget(dropped, presented);
        let cooled = self.last_drop_ms.map_or(true, |at| {
            now_ms.saturating_sub(at) >= TIER_DROP_COOLDOWN_MS
        });
        let to = match self.tier {
            Tier::Desktop => Tier::Tv,
            Tier::Tv | Tier::Low => Tier::Low,
        };
        if !video_over || !cooled || to == self.tier {
            return TierVerdict::Hold;
        }
        let event = TierDropEvent {
            from: self.tier,
            to,
            dropped,
            presented,
            window_ms,
            renderer: in_window,
        };
        // Each verdict ends the window and starts the cooldown, so a spared
        // window is logged at most once per cooldown too.
        self.last_drop_ms = Some(now_ms);
        self.samples.clear();
        // A renderer that counted no frames at all in the window tells us
        // nothing (a stalled worker reads the same as an idle one), so the
        // drop is charged to the video as if uncounted.
        if let Some(r) = in_window.filter(|r| r.skipped.saturating_add(r.drawn) > 0) {
            if !over_budget(r.skipped, r.skipped.saturating_add(r.drawn)) {
                return TierVerdict::Spared(event);
            }
        }
        self.tier = to;
        TierVerdict::Drop(event)
    }
}

/// The log line for a tier drop and the numbers behind it.
pub fn tier_drop_line(drop: &TierDropEvent) -> String {
    format!(
        "pharos: subtitle tier drop from={} to={} reason=dropped_frames dropped={} presented={} window_ms={}{}",
        drop.from.label(),
        drop.to.label(),
        drop.dropped,
        drop.presented,
        drop.window_ms,
        renderer_fields(drop.renderer),
    )
}

/// The log line for a window whose dropped frames were the video's own, so
/// the tier stayed (B667).
pub fn tier_spared_line(spared: &TierDropEvent) -> String {
    format!(
        "pharos: subtitle tier kept tier={} reason=renderer_kept_up dropped={} presented={} window_ms={}{}",
        spared.from.label(),
        spared.dropped,
        spared.presented,
        spared.window_ms,
        renderer_fields(spared.renderer),
    )
}

fn renderer_fields(r: Option<RendererFrames>) -> String {
    r.map_or(" renderer=uncounted".to_owned(), |r| {
        format!(" renderer_skipped={} renderer_drawn={}", r.skipped, r.drawn)
    })
}

/// The `rendererMode` client event for a tier drop, drawn by `renderer`.
pub fn tier_drop_report(renderer: &str, drop: &TierDropEvent) -> ClientEvent {
    ClientEvent {
        kind: ClientEventKind::RendererMode,
        level: ClientLevel::Info,
        route: String::new(),
        fields: vec![
            ("renderer", renderer.to_owned()),
            ("from", drop.from.label().to_owned()),
            ("to", drop.to.label().to_owned()),
            ("reason", "dropped_frames".to_owned()),
            ("dropped", drop.dropped.to_string()),
            ("presented", drop.presented.to_string()),
            ("window_ms", drop.window_ms.to_string()),
        ],
    }
}

/// How long a remembered tier cap lasts. It only spares a weak device the
/// first 10 s of over-budget frames on its next episode, so a day is enough;
/// one bad window used to cap a device for two weeks (B666).
pub const TIER_DROP_TTL_MS: u64 = 24 * 60 * 60 * 1000;

/// The stored form of a cap: the tier and when it was set, `tv@<unix_ms>`.
pub fn format_tier_drop(tier: Tier, set_at_ms: u64) -> String {
    format!("{}@{set_at_ms}", tier.label())
}

/// The cap a stored [`TIER_DROP_KEY`] value holds at `now_ms`: none once it
/// is older than [`TIER_DROP_TTL_MS`] or unreadable. A bare tier, from before
/// caps carried a time, still counts.
pub fn parse_tier_drop(stored: &str, now_ms: u64) -> Option<Tier> {
    let Some((tier, set_at)) = stored.split_once('@') else {
        return Tier::parse(stored);
    };
    let set_at: u64 = set_at.trim().parse().ok()?;
    // A cap stamped after now came from a clock since corrected: it says
    // nothing about how long ago the device dropped, so it is not trusted.
    if set_at > now_ms || now_ms - set_at > TIER_DROP_TTL_MS {
        return None;
    }
    Tier::parse(tier)
}

/// Where the viewer's library card transition is kept; absent or unknown is
/// [`LibraryTransition::Fade`].
pub const LIBRARY_TRANSITION_KEY: &str = "pharos.library.transition";

/// How a library's cards arrive when the viewer opens or switches to it
/// (V476). Each is a CSS animation of `transform` and `opacity` only, so the
/// compositor runs it on a TV or headset GPU without repainting the posters.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum LibraryTransition {
    None,
    #[default]
    Fade,
    Pop,
    Flip,
    Warp,
    Matrix,
}

impl LibraryTransition {
    /// Every transition offered, in the Appearance picker's order.
    pub const ALL: [LibraryTransition; 6] = [
        LibraryTransition::None,
        LibraryTransition::Fade,
        LibraryTransition::Pop,
        LibraryTransition::Flip,
        LibraryTransition::Warp,
        LibraryTransition::Matrix,
    ];

    /// The transition a stored [`LIBRARY_TRANSITION_KEY`] value holds;
    /// anything unknown is [`LibraryTransition::Fade`].
    pub fn parse(stored: &str) -> LibraryTransition {
        match stored.trim() {
            "none" => LibraryTransition::None,
            "pop" => LibraryTransition::Pop,
            "flip" => LibraryTransition::Flip,
            "warp" => LibraryTransition::Warp,
            "matrix" => LibraryTransition::Matrix,
            _ => LibraryTransition::Fade,
        }
    }

    /// Both the stored value and the `data-library-transition` attribute on
    /// `<html>`.
    pub fn as_str(self) -> &'static str {
        match self {
            LibraryTransition::None => "none",
            LibraryTransition::Fade => "fade",
            LibraryTransition::Pop => "pop",
            LibraryTransition::Flip => "flip",
            LibraryTransition::Warp => "warp",
            LibraryTransition::Matrix => "matrix",
        }
    }

    /// The option text shown in the Appearance picker.
    pub fn label(self) -> &'static str {
        match self {
            LibraryTransition::None => "None",
            LibraryTransition::Fade => "Fade",
            LibraryTransition::Pop => "Pop",
            LibraryTransition::Flip => "Flip",
            LibraryTransition::Warp => "Warp",
            LibraryTransition::Matrix => "Matrix rain",
        }
    }
}

/// Where the viewer's controls effect is kept; absent or unknown is
/// [`OverlayEffect::Fade`].
pub const OVERLAY_EFFECT_KEY: &str = "pharos.overlay.effect";

/// How the video player's controls hide when they go idle during playback and
/// come back on a wake (V491). Every effect runs on the overlay (top strip,
/// control bar, skip button) alone, never the video, and ends within 300 ms.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum OverlayEffect {
    None,
    #[default]
    Fade,
    Glide,
    Zoom,
    Wobble,
    Dissolve,
    Burn,
}

impl OverlayEffect {
    /// Every effect offered, in the Appearance picker's order.
    pub const ALL: [OverlayEffect; 7] = [
        OverlayEffect::None,
        OverlayEffect::Fade,
        OverlayEffect::Glide,
        OverlayEffect::Zoom,
        OverlayEffect::Wobble,
        OverlayEffect::Dissolve,
        OverlayEffect::Burn,
    ];

    /// The effect a stored [`OVERLAY_EFFECT_KEY`] value holds; anything
    /// unknown is [`OverlayEffect::Fade`].
    pub fn parse(stored: &str) -> OverlayEffect {
        match stored.trim() {
            "none" => OverlayEffect::None,
            "glide" => OverlayEffect::Glide,
            "zoom" => OverlayEffect::Zoom,
            "wobble" => OverlayEffect::Wobble,
            "dissolve" => OverlayEffect::Dissolve,
            "burn" => OverlayEffect::Burn,
            _ => OverlayEffect::Fade,
        }
    }

    /// Both the stored value and the `data-overlay-fx` attribute on `<html>`.
    pub fn as_str(self) -> &'static str {
        match self {
            OverlayEffect::None => "none",
            OverlayEffect::Fade => "fade",
            OverlayEffect::Glide => "glide",
            OverlayEffect::Zoom => "zoom",
            OverlayEffect::Wobble => "wobble",
            OverlayEffect::Dissolve => "dissolve",
            OverlayEffect::Burn => "burn",
        }
    }

    /// The option text shown in the Appearance picker.
    pub fn label(self) -> &'static str {
        match self {
            OverlayEffect::None => "None",
            OverlayEffect::Fade => "Fade",
            OverlayEffect::Glide => "Glide",
            OverlayEffect::Zoom => "Zoom",
            OverlayEffect::Wobble => "Wobble",
            OverlayEffect::Dissolve => "Dissolve",
            OverlayEffect::Burn => "Burn",
        }
    }
}

/// The fade between this song and the next, in ms, for a crossfade of
/// `seconds` on a song `duration_ms` long: never more than a third of the
/// song, so a short song is still mostly heard on its own. `None` when off
/// or when the length is not yet known.
pub fn crossfade_ms(seconds: u32, duration_ms: u64) -> Option<u64> {
    let fade = u64::from(seconds).saturating_mul(1000).min(duration_ms / 3);
    (seconds > 0 && fade > 0).then_some(fade)
}

/// The volume `elapsed_ms` into a fade of `fade_ms` from `from` to `to`,
/// straight-line; the end volume once the fade is over.
pub fn fade_volume(from: f64, to: f64, elapsed_ms: f64, fade_ms: f64) -> f64 {
    if fade_ms <= 0.0 || elapsed_ms >= fade_ms {
        return to.clamp(0.0, 1.0);
    }
    let t = (elapsed_ms / fade_ms).max(0.0);
    (from + (to - from) * t).clamp(0.0, 1.0)
}

/// A `<video>` event the report clock cares about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaEvent {
    Playing,
    Pause,
    Seeked,
    TimeUpdate,
    Ended,
    /// Volume, duration or metadata changed: the controls redraw, nothing
    /// is reported.
    Changed,
    /// Playback ran out of data to show (`waiting`). Not a
    /// progress report; a group member tells the others it is buffering.
    Waiting,
    /// Enough data to play from the current position (`canplay`). Not a
    /// progress report; a group member tells the others it is ready.
    CanPlay,
}

/// What to send the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outgoing {
    Report { paused: bool },
    Stop,
}

/// Progress reports while playing. The server writes a report at once when
/// the pause state flips or the position jumps, and coalesces the rest, so
/// this matches its ~10 s cadence.
pub const REPORT_INTERVAL_MS: f64 = 10_000.0;

/// When a playback reports: on play, pause and seek at once, every
/// [`REPORT_INTERVAL_MS`] while playing, and a single stop at the end
/// (ended, unmount or page hide, whichever comes first).
#[derive(Debug, Clone, Default)]
pub struct ReportClock {
    last_report_ms: Option<f64>,
    paused: bool,
    stopped: bool,
}

impl ReportClock {
    pub fn new() -> Self {
        Self::default()
    }

    /// The send `event` at `now_ms` (a monotonic clock) calls for.
    pub fn on(&mut self, now_ms: f64, event: MediaEvent) -> Option<Outgoing> {
        if self.stopped {
            return None;
        }
        let send = match event {
            MediaEvent::Playing => {
                self.paused = false;
                Outgoing::Report { paused: false }
            }
            MediaEvent::Pause => {
                self.paused = true;
                Outgoing::Report { paused: true }
            }
            MediaEvent::Seeked => Outgoing::Report {
                paused: self.paused,
            },
            MediaEvent::TimeUpdate => {
                let due = self
                    .last_report_ms
                    .map_or(true, |last| now_ms - last >= REPORT_INTERVAL_MS);
                if self.paused || !due {
                    return None;
                }
                Outgoing::Report { paused: false }
            }
            MediaEvent::Ended => return self.stop().then_some(Outgoing::Stop),
            MediaEvent::Changed | MediaEvent::Waiting | MediaEvent::CanPlay => return None,
        };
        self.last_report_ms = Some(now_ms);
        Some(send)
    }

    /// The report owed when the page becomes hidden: a report, not a stop,
    /// since a hidden tab usually comes back. Nothing once stopped.
    pub fn on_hidden(&mut self, now_ms: f64) -> Option<Outgoing> {
        if self.stopped {
            return None;
        }
        self.last_report_ms = Some(now_ms);
        Some(Outgoing::Report {
            paused: self.paused,
        })
    }

    /// The page came back from the back/forward cache after `pagehide` sent
    /// the stop: reports flow again, the next one at once. Whether a stop
    /// had been sent.
    pub fn restore(&mut self) -> bool {
        self.last_report_ms = None;
        std::mem::replace(&mut self.stopped, false)
    }

    /// Whether a stop is still owed: true once, then false.
    pub fn stop(&mut self) -> bool {
        !std::mem::replace(&mut self.stopped, true)
    }
}

/// What a fatal hls.js error calls for (hls.js's own recovery calls).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recovery {
    /// `startLoad()`: reload from where loading stopped.
    StartLoad,
    /// `recoverMediaError()`: rebuild the media buffers.
    RecoverMedia,
    /// Give up and show the error.
    Fail,
}

impl Recovery {
    /// The label the error log carries.
    pub fn label(self) -> &'static str {
        match self {
            Recovery::StartLoad => "start_load",
            Recovery::RecoverMedia => "recover_media",
            Recovery::Fail => "fail",
        }
    }
}

/// Recoveries of one kind before a fatal error is shown. A fragment that
/// loads refills the budget, so a long film survives scattered blips but a
/// stream that keeps failing gives up within a few seconds.
pub const MAX_RECOVERIES: u32 = 3;

/// Chooses the recovery for each fatal hls.js error.
#[derive(Debug, Clone, Default)]
pub struct RecoveryBudget {
    network: u32,
    media: u32,
}

impl RecoveryBudget {
    pub fn new() -> Self {
        Self::default()
    }

    /// The recovery for a fatal error of hls.js `kind` (`ErrorTypes`) and
    /// `details` (`ErrorDetails`).
    pub fn on_fatal(&mut self, kind: &str, details: &str) -> Recovery {
        let (used, recovery) = match kind {
            // Nothing loaded yet to restart from: `startLoad` would do nothing.
            "networkError" if details.starts_with("manifest") => return Recovery::Fail,
            "networkError" => (&mut self.network, Recovery::StartLoad),
            "mediaError" => (&mut self.media, Recovery::RecoverMedia),
            _ => return Recovery::Fail,
        };
        if *used >= MAX_RECOVERIES {
            return Recovery::Fail;
        }
        *used += 1;
        recovery
    }

    /// The attempts used so far, for the error log.
    pub fn attempts(&self, kind: &str) -> u32 {
        match kind {
            "networkError" => self.network,
            "mediaError" => self.media,
            _ => 0,
        }
    }

    /// A fragment loaded: playback is moving again.
    pub fn progressed(&mut self) {
        *self = Self::default();
    }
}

/// The play head must pass this far beyond where a rebuild started before
/// the media budget refills. A loaded segment is not enough: a segment that
/// fails to decode every time loads fine each time, and would rebuild forever.
/// Longer than one segment (6 s) so the failing one is behind the play head.
pub const MEDIA_REFILL_MS: u64 = 10_000;

/// What to do about one Rust-engine media error (V638, V1062).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaVerdict {
    /// The MediaSource this error came from was already rebuilt: one fault
    /// fires several errors, and only the first one counts.
    Stale,
    /// Rebuild the MediaSource at the play head; `attempt` counts from 1.
    Rebuild { attempt: u32 },
    /// Give up: `attempt` recoveries already failed, or the error cannot be
    /// recovered by a rebuild.
    Fail { attempt: u32 },
}

impl MediaVerdict {
    /// The `action` token, shared with hls.js's error line.
    pub fn label(self) -> &'static str {
        match self {
            Self::Stale => "stale",
            Self::Rebuild { .. } => Recovery::RecoverMedia.label(),
            Self::Fail { .. } => Recovery::Fail.label(),
        }
    }

    pub fn attempt(self) -> u32 {
        match self {
            Self::Stale => 0,
            Self::Rebuild { attempt } | Self::Fail { attempt } => attempt,
        }
    }
}

/// Playback got [`MEDIA_REFILL_MS`] past the last rebuild with no new media
/// error: the rebuild fixed it. A head that merely moves is not enough, as the
/// fault is often a segment buffered ahead that fails again on its refetch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MediaResumed {
    pub attempt: u32,
    pub at_ms: u64,
    pub took_ms: u64,
}

/// Media-error recovery for the Rust engine: [`RecoveryBudget`]'s media
/// budget, counted per MediaSource (`epoch`) and refilled only once the play
/// head has moved [`MEDIA_REFILL_MS`] past the rebuild point.
#[derive(Debug, Clone, Default)]
pub struct MediaRecovery {
    budget: RecoveryBudget,
    epoch: u32,
    /// Where and when the last rebuild started, until playback passes it.
    rebuilt: Option<(u64, u64)>,
}

impl MediaRecovery {
    pub fn new() -> Self {
        Self::default()
    }

    /// The MediaSource generation errors are counted against.
    pub fn epoch(&self) -> u32 {
        self.epoch
    }

    /// A media error `details` from MediaSource `epoch` at play head `at_ms`.
    pub fn on_error(&mut self, epoch: u32, details: &str, at_ms: u64, now_ms: u64) -> MediaVerdict {
        if epoch != self.epoch {
            return MediaVerdict::Stale;
        }
        // A codec the browser refuses is refused again by a new MediaSource.
        if details == "bufferAddCodecError" {
            return MediaVerdict::Fail {
                attempt: self.budget.attempts("mediaError"),
            };
        }
        match self.budget.on_fatal("mediaError", details) {
            Recovery::RecoverMedia => {
                self.epoch = self.epoch.saturating_add(1);
                self.rebuilt = Some((at_ms, now_ms));
                MediaVerdict::Rebuild {
                    attempt: self.budget.attempts("mediaError"),
                }
            }
            Recovery::StartLoad | Recovery::Fail => MediaVerdict::Fail {
                attempt: self.budget.attempts("mediaError"),
            },
        }
    }

    /// The play head is at `at_ms` and moving. [`MEDIA_REFILL_MS`] past the
    /// rebuild point, the rebuild counts as a recovery: the budget refills and
    /// the recovery is reported, once.
    pub fn on_play_head(&mut self, at_ms: u64, now_ms: u64) -> Option<MediaResumed> {
        let (from_ms, started_ms) = self.rebuilt?;
        if at_ms < from_ms.saturating_add(MEDIA_REFILL_MS) {
            return None;
        }
        let attempt = self.budget.attempts("mediaError");
        self.budget.progressed();
        self.rebuilt = None;
        Some(MediaResumed {
            attempt,
            at_ms,
            took_ms: now_ms.saturating_sub(started_ms),
        })
    }
}

/// One media-error decision of the Rust engine, or the rebuilt MediaSource
/// playing again: the failure and the success side of V638 in one shape
/// (V640).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MediaRecoveryReport {
    Error {
        details: String,
        message: String,
        verdict: MediaVerdict,
        at_ms: u64,
    },
    Resumed(MediaResumed),
}

impl MediaRecoveryReport {
    pub fn line(&self, url: &str) -> String {
        match self {
            Self::Error {
                details,
                message,
                verdict,
                at_ms,
            } => format!(
                "pharos: playback error engine=rust type=mediaError details={details} message={message} fatal=true action={} attempt={} at_ms={at_ms} url={url}",
                verdict.label(),
                verdict.attempt(),
            ),
            Self::Resumed(r) => format!(
                "pharos: playback recovered engine=rust type=mediaError action=recover_media attempt={} at_ms={} took_ms={} url={url}",
                r.attempt, r.at_ms, r.took_ms
            ),
        }
    }

    pub fn event(&self, route: String) -> ClientEvent {
        let (level, fields) = match self {
            Self::Error {
                details,
                message,
                verdict,
                at_ms,
            } => (
                match verdict {
                    MediaVerdict::Stale => ClientLevel::Warn,
                    MediaVerdict::Rebuild { .. } | MediaVerdict::Fail { .. } => ClientLevel::Error,
                },
                vec![
                    ("engine", "rust".to_owned()),
                    ("type", "mediaError".to_owned()),
                    ("details", details.clone()),
                    ("message", message.clone()),
                    ("action", verdict.label().to_owned()),
                    ("attempt", verdict.attempt().to_string()),
                    ("at_ms", at_ms.to_string()),
                ],
            ),
            Self::Resumed(r) => (
                ClientLevel::Info,
                vec![
                    ("engine", "rust".to_owned()),
                    ("type", "mediaError".to_owned()),
                    ("details", "recovered".to_owned()),
                    ("action", Recovery::RecoverMedia.label().to_owned()),
                    ("attempt", r.attempt.to_string()),
                    ("at_ms", r.at_ms.to_string()),
                    ("took_ms", r.took_ms.to_string()),
                ],
            ),
        };
        ClientEvent {
            kind: ClientEventKind::PlaybackError,
            level,
            route,
            fields,
        }
    }
}

/// Where Play starts: the stored position of a started, unfinished item.
pub fn start_ms(position_ms: u64, played: bool) -> u64 {
    if played {
        0
    } else {
        position_ms
    }
}

/// hls.js `startPosition` for a start `start_ms` in, in seconds. Always an
/// explicit time: -1 lets hls.js take the playlist's `EXT-X-START`, which the
/// server sets to the saved position, so Play from start would resume.
#[allow(clippy::cast_precision_loss)]
pub fn hls_start_position(start_ms: u64) -> f64 {
    start_ms as f64 / 1000.0
}

/// Seconds hls.js aims to hold ahead of the play head. Above its 30 s
/// default so a long-latency viewer rides out a slow segment (B1325).
pub const HLS_FORWARD_S: f64 = 60.0;

/// Ceiling hls.js may raise the forward target to for a low-bitrate stream.
pub const HLS_FORWARD_CEILING_S: f64 = 120.0;

/// Seconds kept behind the play head. hls.js keeps everything by default,
/// which fills the browser's SourceBuffer quota a few minutes in; each
/// eviction then halves the forward ceiling, down to about two segments
/// (B1325, V1296).
pub const HLS_BACK_S: f64 = 30.0;

/// Bytes hls.js may hold ahead: 120 s of a ~6 Mbps stream, below Chrome's
/// ~150 MB video quota so the forward target never pushes it over.
pub const HLS_FORWARD_BYTES: f64 = 90_000_000.0;

/// The buffer settings handed to hls.js, as `(config key, value)`.
pub fn hls_buffer_config() -> [(&'static str, f64); 4] {
    [
        ("maxBufferLength", HLS_FORWARD_S),
        ("maxMaxBufferLength", HLS_FORWARD_CEILING_S),
        ("backBufferLength", HLS_BACK_S),
        ("maxBufferSize", HLS_FORWARD_BYTES),
    ]
}

/// Watches hls.js's forward ceiling (`maxMaxBufferLength`), which hls.js
/// lowers on its own after buffer-full errors and evictions and never
/// raises again. The first reading is the baseline; each drop below the
/// last one is reported once, with both values.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct BufferCeiling {
    last_s: Option<f64>,
}

impl BufferCeiling {
    /// `Some((from_s, to_s))` when `now_s` is below the last value seen.
    /// A missing or non-finite reading is ignored.
    pub fn observe(&mut self, now_s: f64) -> Option<(f64, f64)> {
        if !now_s.is_finite() || now_s <= 0.0 {
            return None;
        }
        let cut = self
            .last_s
            .filter(|last| now_s < *last)
            .map(|from| (from, now_s));
        if self.last_s.is_none() || cut.is_some() {
            self.last_s = Some(now_s);
        }
        cut
    }
}

/// One trickplay thumbnail: the tile it is on and where in the tile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Thumb {
    pub url: String,
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// The thumbnail nearest before `at_ms`, clamped to the last one; `None`
/// for a set with no thumbnails.
pub fn thumb_at(set: &Trickplay, at_ms: u64) -> Option<Thumb> {
    let per_tile = set.grid.checked_mul(set.grid).filter(|&n| n > 0)?;
    let last = set.thumb_count.checked_sub(1)?;
    let n = (at_ms / u64::from(set.interval_ms.max(1))).min(u64::from(last));
    let n = u32::try_from(n).unwrap_or(last);
    let (tile, within) = (n / per_tile, n % per_tile);
    Some(Thumb {
        url: set.tile_url.replace("{tile}", &tile.to_string()),
        x: (within % set.grid).saturating_mul(set.width),
        y: (within / set.grid).saturating_mul(set.height),
        width: set.width,
        height: set.height,
    })
}

/// The narrowest sprite set: the seek-bar preview is small, and the
/// narrowest tiles load fastest.
pub fn preview_set(sets: &[Trickplay]) -> Option<&Trickplay> {
    sets.iter().min_by_key(|s| s.width)
}

/// The segment playing at `at_ms`, whose skip button shows.
pub fn segment_at(segments: &[Segment], at_ms: u64) -> Option<Segment> {
    segments
        .iter()
        .find(|s| s.start_ms <= at_ms && at_ms < s.end_ms)
        .copied()
}

pub fn skip_label(kind: SegmentKind) -> &'static str {
    match kind {
        SegmentKind::Intro => "Skip intro",
        SegmentKind::Outro => "Skip credits",
        SegmentKind::Recap => "Skip recap",
        SegmentKind::Preview => "Skip preview",
        SegmentKind::Commercial => "Skip ad",
    }
}

/// Where the end credits start: the latest outro the server detected in the
/// second half of a video `total_ms` long. Credits in the first half (a cold
/// open's) are not the end.
pub fn credits_start(segments: &[Segment], total_ms: u64) -> Option<u64> {
    segments
        .iter()
        .filter(|s| s.kind == SegmentKind::Outro)
        .filter(|s| s.start_ms >= total_ms / 2 && s.start_ms < total_ms)
        .map(|s| s.start_ms)
        .max()
}

/// What is left to watch of a video.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Left {
    pub ms: u64,
    /// `ms` runs to where the credits start, not to the end.
    pub before_credits: bool,
}

/// What is left to watch from `at_ms`: up to the credits while they are
/// still ahead, else to the end. `None` while the length is unknown or once
/// nothing is left.
pub fn left_to_watch(segments: &[Segment], at_ms: u64, total_ms: u64) -> Option<Left> {
    let left = match credits_start(segments, total_ms) {
        Some(credits) if at_ms < credits => Left {
            ms: credits - at_ms,
            before_credits: true,
        },
        _ => Left {
            ms: total_ms.checked_sub(at_ms)?,
            before_credits: false,
        },
    };
    (left.ms > 0).then_some(left)
}

/// "Ends at 10:42 PM", or "Ends at 10:40 PM (before credits)", from the
/// local time it ends at.
pub fn ends_at_label(time: &str, before_credits: bool) -> String {
    if before_credits {
        format!("Ends at {time} (before credits)")
    } else {
        format!("Ends at {time}")
    }
}

/// How long the "Up next" card counts down from where the credits start.
pub const UP_NEXT_COUNTDOWN_MS: u64 = 15_000;
/// Without detected credits, the card shows this share of the runtime before
/// the end (1/20: 66 s of a 22-minute episode), where most credits start ...
pub const UP_NEXT_LEAD_SHARE: u64 = 20;
/// ... but no less than this before the end ...
pub const UP_NEXT_MIN_LEAD_MS: u64 = 20_000;
/// ... and no more, so an hour-long drama's last scene is not covered.
pub const UP_NEXT_MAX_LEAD_MS: u64 = 90_000;

/// Where the "Up next" card shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UpNextFrom {
    pub at_ms: u64,
    /// The server detected the credits there. Otherwise `at_ms` is a guess,
    /// and the card waits for the end rather than cut a scene short.
    pub credits: bool,
}

/// Where the "Up next" card shows: where the credits start, when the server
/// detected them in the second half, else a share of the runtime before the
/// end, between [`UP_NEXT_MIN_LEAD_MS`] and [`UP_NEXT_MAX_LEAD_MS`] but
/// never in the first half, which wins for a clip under 40 s. `None` while
/// the length is unknown.
pub fn up_next_at(segments: &[Segment], total_ms: u64) -> Option<UpNextFrom> {
    if total_ms == 0 {
        return None;
    }
    if let Some(at_ms) = credits_start(segments, total_ms) {
        return Some(UpNextFrom {
            at_ms,
            credits: true,
        });
    }
    let lead = (total_ms / UP_NEXT_LEAD_SHARE)
        .clamp(UP_NEXT_MIN_LEAD_MS, UP_NEXT_MAX_LEAD_MS)
        .min(total_ms / 2);
    Some(UpNextFrom {
        at_ms: total_ms - lead,
        credits: false,
    })
}

/// Whether "Up next" is the small pill rather than the full card: over a
/// guessed ending (no credits detected) the card would cover the last
/// scene for up to 90 s, so it waits as a pill until the last
/// [`UP_NEXT_COUNTDOWN_MS`] (038 FR-405, V1221). Over detected credits it
/// is the card throughout.
pub fn up_next_pill(credits: bool, left_ms: u64) -> bool {
    !credits && left_ms > UP_NEXT_COUNTDOWN_MS
}

/// What the "Up next" card does at a position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpNextStep {
    Hidden,
    /// Shown, with `left_ms` of a countdown `of_ms` long until the next
    /// episode plays.
    Counting {
        left_ms: u64,
        of_ms: u64,
    },
    /// Play the next episode now.
    Go,
}

/// The "Up next" countdown. It follows the video's position, so pausing
/// pauses it; seeking back before the card hides it and starts it over, and
/// Cancel hides it for the rest of this episode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct UpNext {
    shown_at: Option<u64>,
    cancelled: bool,
}

impl UpNext {
    /// The card's state at `at_ms`, when it shows from `from` in a video
    /// `total_ms` long. Over detected credits it counts
    /// [`UP_NEXT_COUNTDOWN_MS`]; from a guess it counts to the end.
    pub fn step(&mut self, at_ms: u64, from: Option<UpNextFrom>, total_ms: u64) -> UpNextStep {
        let Some(from) = from.filter(|_| !self.cancelled) else {
            return UpNextStep::Hidden;
        };
        if at_ms < from.at_ms {
            self.shown_at = None;
            return UpNextStep::Hidden;
        }
        // A seek back inside the card's window counts from where it landed,
        // so the countdown and its bar start over together.
        if self.shown_at.is_some_and(|shown| at_ms < shown) {
            self.shown_at = Some(at_ms);
        }
        let shown = *self.shown_at.get_or_insert(at_ms);
        let end = total_ms.max(shown);
        let due = if from.credits {
            shown.saturating_add(UP_NEXT_COUNTDOWN_MS).min(end)
        } else {
            end
        };
        match due.checked_sub(at_ms) {
            Some(left_ms) if left_ms > 0 => UpNextStep::Counting {
                left_ms,
                of_ms: due - shown,
            },
            _ => UpNextStep::Go,
        }
    }

    pub fn cancel(&mut self) {
        self.cancelled = true;
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled
    }

    /// Why the player should move on to the next episode after `event`,
    /// which left the card at `step`; `None` to stay.
    pub fn moves_on(&self, step: UpNextStep, event: MediaEvent) -> Option<&'static str> {
        match (step, event) {
            (UpNextStep::Go, _) => Some("countdown"),
            (_, MediaEvent::Ended) if !self.cancelled => Some("ended"),
            _ => None,
        }
    }
}

/// `at_ms` as a percentage of `total_ms`, for a left offset on the seek bar.
pub fn percent(at_ms: u64, total_ms: u64) -> f64 {
    if total_ms == 0 {
        return 0.0;
    }
    // Lossy for positions past 2^53 ms, far beyond any runtime.
    (at_ms.min(total_ms) as f64 / total_ms as f64) * 100.0
}

/// The position a fraction of the way along a seek bar `width` wide. `None`
/// while the length is unknown, as for a live stream, so a click seeks nowhere
/// rather than to the start.
pub fn position_at(x: f64, width: f64, total_ms: u64) -> Option<u64> {
    if total_ms == 0 || width.is_nan() || width <= 0.0 || !x.is_finite() {
        return None;
    }
    let fraction = (x / width).clamp(0.0, 1.0);
    // Saturating cast of a value already within 0..=total_ms.
    Some((fraction * total_ms as f64) as u64)
}

/// "4:05", "1:02:03".
pub fn clock(ms: u64) -> String {
    let s = ms / 1000;
    let (h, m, s) = (s / 3600, s / 60 % 60, s % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// How a track is named in a picker: its language, an audio track's codec
/// and channels, then its title when that says something more (a file's
/// "Surround" on a 5.1 track does not), and the forced and SDH flags the
/// title does not already say. A track with none of these is its position.
pub fn track_label(track: &Track, position: usize) -> String {
    let channels = (track.channels > 0).then_some(track.channels);
    let mut label = audio_label(
        &track.language,
        &track.codec,
        channels,
        &track.title,
        position,
    );
    let lower = track.title.to_lowercase();
    if track.is_forced && !lower.contains("forced") {
        label.push_str(" (forced)");
    }
    if track.is_hearing_impaired && !lower.contains("sdh") {
        label.push_str(" [SDH]");
    }
    label
}

/// The picker name from a track's parts; `channels` is `None` when the
/// track does not say them.
fn audio_label(
    language: &str,
    codec: &str,
    channels: Option<u16>,
    title: &str,
    position: usize,
) -> String {
    let mut parts: Vec<String> = Vec::new();
    let language = language_name(language);
    if !language.is_empty() {
        parts.push(language);
    }
    let sound = [codec_name(codec), channels.and_then(channel_name)]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" ");
    if !sound.is_empty() {
        parts.push(sound);
    }
    let said = parts.join(" ").to_lowercase();
    let adds = title.split_whitespace().any(|word| {
        let word = word.to_lowercase();
        !said.contains(&word) && !GENERIC_TITLE_WORDS.contains(&word.as_str())
    });
    if adds {
        parts.push(title.to_owned());
    }
    match parts.is_empty() {
        true => format!("Track {}", position.saturating_add(1)),
        false => parts.join(" · "),
    }
}

/// Title words that only restate a track's channels or that it is audio.
const GENERIC_TITLE_WORDS: &[&str] = &[
    "surround", "stereo", "mono", "audio", "sound", "track", "-", "ch", "channel", "channels",
];

/// A language's English name for an ISO 639-1 or 639-2 (bibliographic or
/// terminology) code, else the code in capitals; nothing for an undetermined
/// code (`und`, `mis`, `zxx`, `mul`) or an empty one. A region or script tag
/// after it (`pt-BR`, `zh-Hant`) is dropped; the track title says which.
pub(super) fn language_name(code: &str) -> String {
    let lower = code.trim().to_ascii_lowercase();
    let base = lower.split(['-', '_']).next().unwrap_or_default();
    match base {
        "aa" | "aar" => "Afar",
        "af" | "afr" => "Afrikaans",
        "sq" | "alb" | "sqi" => "Albanian",
        "am" | "amh" => "Amharic",
        "ar" | "ara" => "Arabic",
        "hy" | "arm" | "hye" => "Armenian",
        "az" | "aze" => "Azerbaijani",
        "eu" | "baq" | "eus" => "Basque",
        "be" | "bel" => "Belarusian",
        "bn" | "ben" => "Bengali",
        "bs" | "bos" => "Bosnian",
        "bg" | "bul" => "Bulgarian",
        "my" | "bur" | "mya" => "Burmese",
        "ca" | "cat" => "Catalan",
        "zh" | "chi" | "zho" => "Chinese",
        "hr" | "hrv" | "scr" => "Croatian",
        "cs" | "cze" | "ces" => "Czech",
        "da" | "dan" => "Danish",
        "nl" | "dut" | "nld" => "Dutch",
        "en" | "eng" => "English",
        "eo" | "epo" => "Esperanto",
        "et" | "est" => "Estonian",
        "fo" | "fao" => "Faroese",
        "fil" => "Filipino",
        "fi" | "fin" => "Finnish",
        "fr" | "fre" | "fra" => "French",
        "gl" | "glg" => "Galician",
        "ka" | "geo" | "kat" => "Georgian",
        "de" | "ger" | "deu" => "German",
        "el" | "gre" | "ell" => "Greek",
        "gu" | "guj" => "Gujarati",
        "he" | "heb" => "Hebrew",
        "hi" | "hin" => "Hindi",
        "hu" | "hun" => "Hungarian",
        "is" | "ice" | "isl" => "Icelandic",
        "id" | "ind" => "Indonesian",
        "ga" | "gle" => "Irish",
        "it" | "ita" => "Italian",
        "ja" | "jpn" => "Japanese",
        "kn" | "kan" => "Kannada",
        "kk" | "kaz" => "Kazakh",
        "km" | "khm" => "Khmer",
        "ko" | "kor" => "Korean",
        "ku" | "kur" => "Kurdish",
        "lo" | "lao" => "Lao",
        "la" | "lat" => "Latin",
        "lv" | "lav" => "Latvian",
        "lt" | "lit" => "Lithuanian",
        "lb" | "ltz" => "Luxembourgish",
        "mk" | "mac" | "mkd" => "Macedonian",
        "ms" | "may" | "msa" => "Malay",
        "ml" | "mal" => "Malayalam",
        "mt" | "mlt" => "Maltese",
        "mi" | "mao" | "mri" => "Maori",
        "mr" | "mar" => "Marathi",
        "mn" | "mon" => "Mongolian",
        "ne" | "nep" => "Nepali",
        "no" | "nor" => "Norwegian",
        "nb" | "nob" => "Norwegian Bokmål",
        "nn" | "nno" => "Norwegian Nynorsk",
        "fa" | "per" | "fas" => "Persian",
        "pl" | "pol" => "Polish",
        "pt" | "por" => "Portuguese",
        "pa" | "pan" => "Punjabi",
        "ro" | "rum" | "ron" => "Romanian",
        "ru" | "rus" => "Russian",
        "sr" | "srp" | "scc" => "Serbian",
        "si" | "sin" => "Sinhala",
        "sk" | "slo" | "slk" => "Slovak",
        "sl" | "slv" => "Slovenian",
        "so" | "som" => "Somali",
        "es" | "spa" => "Spanish",
        "sw" | "swa" => "Swahili",
        "sv" | "swe" => "Swedish",
        "tl" | "tgl" => "Tagalog",
        "ta" | "tam" => "Tamil",
        "te" | "tel" => "Telugu",
        "th" | "tha" => "Thai",
        "bo" | "tib" | "bod" => "Tibetan",
        "tr" | "tur" => "Turkish",
        "uk" | "ukr" => "Ukrainian",
        "ur" | "urd" => "Urdu",
        "uz" | "uzb" => "Uzbek",
        "vi" | "vie" => "Vietnamese",
        "cy" | "wel" | "cym" => "Welsh",
        "xh" | "xho" => "Xhosa",
        "yi" | "yid" => "Yiddish",
        "zu" | "zul" => "Zulu",
        "und" | "mis" | "zxx" | "mul" => "",
        _ => return code.trim().to_ascii_uppercase(),
    }
    .to_owned()
}

/// How an audio codec is usually written; nothing for a subtitle's.
fn codec_name(codec: &str) -> Option<String> {
    let name = match codec.to_ascii_lowercase().as_str() {
        "aac" => "AAC",
        "ac3" => "AC3",
        "eac3" => "E-AC3",
        "dts" => "DTS",
        "truehd" => "TrueHD",
        "flac" => "FLAC",
        "opus" => "Opus",
        "vorbis" => "Vorbis",
        "mp3" => "MP3",
        "mp2" => "MP2",
        "alac" => "ALAC",
        c if c.starts_with("pcm") => "PCM",
        _ => return None,
    };
    Some(name.to_owned())
}

/// "Stereo", "5.1": a layout by its usual name, nothing when unknown.
fn channel_name(channels: u16) -> Option<String> {
    Some(match channels {
        0 => return None,
        1 => "Mono".to_owned(),
        2 => "Stereo".to_owned(),
        6 => "5.1".to_owned(),
        8 => "7.1".to_owned(),
        n => format!("{n} ch"),
    })
}

/// How the player moves from the subtitle it shows to another.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubtitleSwitch {
    /// Show or hide WebVTT on the `<video>`; the stream plays on.
    InPlace,
    /// The server must start or stop burning a subtitle: a new playback.
    Renegotiate,
}

/// `burned` is whether the playing stream has a subtitle drawn in; `to` the
/// track picked, `None` for off.
pub fn subtitle_switch(burned: bool, to: Option<&Track>) -> SubtitleSwitch {
    match (burned, to) {
        (true, _)
        | (
            false,
            Some(Track {
                vtt_url: None,
                images_url: None,
                ..
            }),
        ) => SubtitleSwitch::Renegotiate,
        (false, _) => SubtitleSwitch::InPlace,
    }
}

/// The original script of an ASS/SSA text subtitle, for a renderer that
/// draws its typesetting (ADR-0057); `None` for any other track, which plays
/// as WebVTT. The server serves the script beside the WebVTT it converts it
/// to, at `stream.ass` in place of `stream.vtt`.
pub fn styled_subtitle_url(track: &Track) -> Option<String> {
    if !matches!(track.codec.to_ascii_lowercase().as_str(), "ass" | "ssa") {
        return None;
    }
    let vtt = track.vtt_url.as_deref()?;
    vtt.strip_suffix("/stream.vtt")
        .map(|base| format!("{base}/stream.ass"))
}

/// The URL of a generated styled track's script at `tier`, `level` and the
/// viewer's subtitle `size` (spec 028 FR-006, FR-022): `None` for any other
/// track, and for every track while Effects is Off, where the plain track
/// draws instead. Medium sends no `size`, the server's default, and
/// `speakers` off sends nothing either.
pub fn styled_track_url(
    track: &Track,
    tier: Tier,
    level: EffectsLevel,
    size: SubtitleSize,
    speakers: bool,
) -> Option<String> {
    if level == EffectsLevel::Off {
        return None;
    }
    let url = track.styled_url.as_deref()?;
    let sized = match size.percent() {
        100 => String::new(),
        p => format!("&size={p}"),
    };
    let speakers = if speakers { "&speakers=1" } else { "" };
    Some(format!(
        "{url}?tier={}&effects={}{sized}{speakers}",
        tier.label(),
        level.as_str()
    ))
}

/// The subtitle tracks a picker offers at `level`: the generated styled track
/// is hidden while Effects is Off (spec 028 FR-006), as the plain tracks
/// beside it still draw.
pub fn offered_subtitles(tracks: &[Track], level: EffectsLevel) -> Vec<Track> {
    tracks
        .iter()
        .filter(|t| level != EffectsLevel::Off || t.styled_url.is_none())
        .cloned()
        .collect()
}

/// Whether the ASS renderer must be rebuilt for `script` when it was last
/// started for `loaded` (B635): for a new script, or to tear it down when
/// there is none. The pick set again to the track already drawn (as happens
/// once playback starts) keeps the renderer and fetches nothing.
pub fn restart_styled(loaded: Option<&str>, script: Option<&str>) -> bool {
    script.is_none() || loaded != script
}

/// The track a generated styled track `styled` is made from: the one that
/// serves the same WebVTT and is not itself generated. `None` when `styled`
/// is not a generated track or its source is not listed.
pub fn source_of_styled(tracks: &[Track], styled: u32) -> Option<u32> {
    let generated = tracks
        .iter()
        .find(|t| t.index == styled && t.styled_url.is_some())?;
    let vtt = generated.vtt_url.as_deref()?;
    tracks
        .iter()
        .find(|t| t.styled_url.is_none() && t.vtt_url.as_deref() == Some(vtt))
        .map(|t| t.index)
}

/// The log line and client event for the ASS renderer getting ready or
/// failing, which falls back to plain text (B600). Both outcomes carry the
/// same fields, so a fallback is as visible in the server log as a success.
pub fn styled_subtitle_signals(result: &Result<(), String>) -> (String, ClientEvent) {
    let (mode, level, reason) = match result {
        Ok(()) => ("styled", ClientLevel::Info, "ready".to_owned()),
        Err(e) => ("plain", ClientLevel::Warn, e.clone()),
    };
    let line = format!("pharos: subtitle renderer=jassub mode={mode} reason={reason}");
    let event = ClientEvent {
        kind: ClientEventKind::RendererMode,
        level,
        route: String::new(),
        fields: vec![
            ("renderer", "jassub".to_owned()),
            ("mode", mode.to_owned()),
            ("reason", reason),
        ],
    };
    (line, event)
}

/// The fonts Pharos ships for the subtitles it styles itself (spec 028), as
/// (lowercased family name, file under `/ui/assets/fonts/`). The ASS renderer
/// gets them as `availableFonts`, which it fetches only when a style names
/// one, so a device downloads a font only for a track that uses it. Each key
/// is the font's own family name: libass matches `Fontname` against the
/// name table, so a key that differs falls back to the default face (V530).
pub const SUBTITLE_FONT_PACK: [(&str, &str); 3] = [
    // Shouts, attack calls and sound effects.
    ("bangers", "Bangers-Regular.ttf"),
    // Signs, captions and narration.
    ("anton", "Anton-Regular.ttf"),
    // Inner voice and thoughts.
    ("patrick hand", "PatrickHand-Regular.ttf"),
];

/// The URLs of the file's embedded fonts, each cut to the glyphs the subtitle
/// at `subtitle` draws, for the ASS renderer to load up front (026 FR-010):
/// libass then matches a style's `Fontname` against the fonts' own names.
pub fn subtitle_font_urls(fonts: &[Font], subtitle: u32) -> Vec<String> {
    fonts
        .iter()
        .map(|f| f.url.replace("{subtitle}", &subtitle.to_string()))
        .collect()
}

/// The font URLs for the track `shown`. A generated styled track has no font
/// route of its own, so it loads its source track's embedded fonts; none when
/// the source is not listed.
pub fn shown_font_urls(fonts: &[Font], tracks: &[Track], shown: u32) -> Vec<String> {
    let is_generated = tracks
        .iter()
        .any(|t| t.index == shown && t.styled_url.is_some());
    if !is_generated {
        return subtitle_font_urls(fonts, shown);
    }
    source_of_styled(tracks, shown)
        .map(|source| subtitle_font_urls(fonts, source))
        .unwrap_or_default()
}

/// The WebVTT URL of a plain text subtitle the Rust renderer draws (spec
/// 026): SRT-family tracks the server serves as WebVTT. ASS/SSA keep their
/// own renderer this slice ([`styled_subtitle_url`]); image subtitles are
/// drawn by the player (019); `None` for both and for a burned track.
pub fn rust_subtitle_url(track: &Track) -> Option<String> {
    let text = matches!(
        track.codec.to_ascii_lowercase().as_str(),
        "srt" | "subrip" | "webvtt" | "vtt" | "mov_text" | "text"
    );
    track.vtt_url.clone().filter(|_| text)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn hls_buffer_config_bounds_the_back_buffer_and_lengthens_the_forward_one() {
        let config: std::collections::HashMap<_, _> = hls_buffer_config().into_iter().collect();
        assert_eq!(config.get("backBufferLength"), Some(&30.0));
        assert_eq!(config.get("maxBufferLength"), Some(&60.0));
        assert_eq!(config.get("maxMaxBufferLength"), Some(&120.0));
        assert_eq!(config.get("maxBufferSize"), Some(&90_000_000.0));
        let forward = config.get("maxBufferLength").copied().unwrap_or_default();
        let ceiling = config
            .get("maxMaxBufferLength")
            .copied()
            .unwrap_or_default();
        assert!(forward <= ceiling, "hls.js caps the target at the ceiling");
    }

    #[test]
    fn buffer_ceiling_reports_each_drop_once_with_both_values() {
        let mut ceiling = BufferCeiling::default();
        assert_eq!(ceiling.observe(120.0), None);
        assert_eq!(ceiling.observe(60.0), Some((120.0, 60.0)));
        assert_eq!(ceiling.observe(60.0), None);
        assert_eq!(ceiling.observe(12.0), Some((60.0, 12.0)));
        assert_eq!(ceiling.observe(f64::NAN), None);
        assert_eq!(ceiling.observe(0.0), None);
        assert_eq!(ceiling.observe(30.0), None, "a rise is not a cut");
    }

    /// B1160: only hls.js media it could not parse or append counts as
    /// damage; a stall or a missing codec does not, nor does the network.
    #[test]
    fn only_unparseable_hls_media_counts_as_damage() {
        assert!(Fatal::from_hls("mediaError", "bufferAppendError").is_media());
        assert!(Fatal::from_hls("mediaError", "fragParsingError").is_media());
        assert!(!Fatal::from_hls("mediaError", "bufferStalledError").is_media());
        assert!(!Fatal::from_hls("mediaError", "manifestIncompatibleCodecsError").is_media());
        assert!(!Fatal::from_hls("networkError", "fragLoadError").is_media());
    }

    /// 038 FR-405, V1221: a guessed ending gets the pill until its last
    /// 15 s; detected credits get the card. Disarm check: returning `false`
    /// fails the first assert.
    #[test]
    fn a_guessed_ending_shows_the_pill_until_its_last_seconds() {
        assert!(up_next_pill(false, 60_000));
        assert!(up_next_pill(false, UP_NEXT_COUNTDOWN_MS + 1));
        assert!(!up_next_pill(false, UP_NEXT_COUNTDOWN_MS));
        assert!(!up_next_pill(true, 60_000));
    }

    /// 038 FR-403: the Rust engine's give-ups name the same culprit
    /// hls.js's would; a refused codec or a stall is not called damage.
    #[test]
    fn a_rust_engine_media_failure_is_damage_only_when_bytes_would_not_decode() {
        use FailureKind::{Damaged, Network, Unplayable};
        for (details, want) in [
            ("bufferAppendError", Damaged),
            ("videoDecodeError", Damaged),
            ("bufferAddCodecError", Unplayable),
            ("bufferStalledError", Network),
            ("bufferRemoveError", Network),
            ("internalException", Network),
        ] {
            assert_eq!(FailureKind::of_media_error(details), want, "{details}");
        }
    }

    /// 038 FR-403: a codec the browser lacks is the browser's limit, a
    /// fetch the server refused is the server's, and the rest the network's.
    #[test]
    fn a_failure_names_who_can_fix_it() {
        let kind = |k, d| Fatal::from_hls(k, d).kind;
        assert_eq!(
            kind("mediaError", "manifestIncompatibleCodecsError"),
            FailureKind::Unplayable
        );
        assert_eq!(
            kind("mediaError", "bufferIncompatibleCodecsError"),
            FailureKind::Unplayable
        );
        assert_eq!(kind("networkError", "fragLoadError"), FailureKind::Network);
        assert_eq!(kind("mediaError", "fragParsingError"), FailureKind::Damaged);
        assert_eq!(FailureKind::of_status(404), FailureKind::Server);
        assert_eq!(FailureKind::of_status(503), FailureKind::Server);
        assert_eq!(FailureKind::of_status(0), FailureKind::Network);
        let labels = [
            FailureKind::Network,
            FailureKind::Server,
            FailureKind::Unplayable,
            FailureKind::Damaged,
        ]
        .map(FailureKind::label);
        let distinct: std::collections::HashSet<_> = labels.iter().collect();
        assert_eq!(distinct.len(), labels.len());
    }

    /// B600: a styled renderer that fails says so with its reason, in the
    /// same shape as one that gets ready.
    #[test]
    fn a_styled_subtitle_fallback_is_logged_and_sent_with_its_reason() {
        let (line, event) = styled_subtitle_signals(&Ok(()));
        assert_eq!(
            line,
            "pharos: subtitle renderer=jassub mode=styled reason=ready"
        );
        assert_eq!(event.kind, ClientEventKind::RendererMode);
        assert_eq!(event.level, ClientLevel::Info);

        let (line, event) = styled_subtitle_signals(&Err("not ready after 8000 ms".into()));
        assert_eq!(
            line,
            "pharos: subtitle renderer=jassub mode=plain reason=not ready after 8000 ms"
        );
        assert_eq!(event.level, ClientLevel::Warn);
        assert_eq!(
            event.fields,
            [
                ("renderer", "jassub".to_owned()),
                ("mode", "plain".to_owned()),
                ("reason", "not ready after 8000 ms".to_owned()),
            ]
        );
    }

    /// V530: each pack font is registered under the family name in its own
    /// name table, the name a style's `Fontname` has to match.
    #[test]
    fn each_pack_font_is_keyed_by_its_own_family_name() {
        use skrifa::raw::TableProvider;
        use skrifa::MetadataProvider;
        for (key, file) in SUBTITLE_FONT_PACK {
            let path = format!("{}/assets/fonts/{file}", env!("CARGO_MANIFEST_DIR"));
            let bytes = std::fs::read(&path).unwrap();
            let font = skrifa::FontRef::new(&bytes).unwrap();
            let family = font
                .localized_strings(skrifa::string::StringId::FAMILY_NAME)
                .english_or_first()
                .unwrap()
                .to_string()
                .to_lowercase();
            assert_eq!(key, family, "{file}");
            assert!(font.name().is_ok(), "{file} keeps its name table");
        }
    }

    /// V488 (B572): every embedded font is fetched for the track being drawn.
    #[test]
    fn each_font_url_names_the_track_it_is_cut_for() {
        let font = |index, url: &str| Font {
            index,
            url: url.into(),
            ..Font::default()
        };
        let fonts = [
            font(4, "/videos/7/subtitles/{subtitle}/fonts/4"),
            font(6, "/videos/7/subtitles/{subtitle}/fonts/6"),
        ];
        assert_eq!(
            subtitle_font_urls(&fonts, 3),
            [
                "/videos/7/subtitles/3/fonts/4",
                "/videos/7/subtitles/3/fonts/6"
            ]
        );
        assert!(subtitle_font_urls(&[], 3).is_empty());
    }

    #[test]
    fn a_generated_track_loads_its_sources_fonts() {
        let fonts = [Font {
            index: 4,
            url: "/videos/7/subtitles/{subtitle}/fonts/4".into(),
            ..Font::default()
        }];
        let track = |index, vtt: &str, styled: Option<&str>| Track {
            index,
            vtt_url: Some(vtt.into()),
            styled_url: styled.map(Into::into),
            ..Track::default()
        };
        let tracks = [
            track(3, "/s/3.vtt", None),
            track(9, "/s/3.vtt", Some("/s/9.ass")),
        ];
        let urls = |shown| shown_font_urls(&fonts, &tracks, shown);
        assert_eq!(urls(9), ["/videos/7/subtitles/3/fonts/4"]);
        assert_eq!(urls(3), ["/videos/7/subtitles/3/fonts/4"]);
        let orphan = [track(9, "/s/x.vtt", Some("/s/9.ass"))];
        assert!(shown_font_urls(&fonts, &orphan, 9).is_empty());
    }

    #[test]
    fn subtitle_sizes_scale_75_to_150_percent() {
        let scales = SubtitleSize::ALL.map(SubtitleSize::scale);
        assert_eq!(scales, [0.75, 1.0, 1.25, 1.5]);
        assert_eq!(SubtitleSize::default().scale(), 1.0);
        assert_eq!(
            SubtitleSize::ALL.map(SubtitleSize::percent),
            [75, 100, 125, 150]
        );
    }

    #[test]
    fn the_rust_renderer_takes_plain_text_codecs_only() {
        let track = |codec: &str, vtt: Option<&str>| Track {
            codec: codec.into(),
            vtt_url: vtt.map(Into::into),
            ..Track::default()
        };
        let url = Some("/videos/7/subtitles/3/stream.vtt");
        for codec in ["srt", "SubRip", "webvtt", "vtt", "mov_text", "text"] {
            assert_eq!(
                rust_subtitle_url(&track(codec, url)).as_deref(),
                url,
                "{codec}"
            );
        }
        // ASS keeps JASSUB; bitmap tracks and burned ones have no WebVTT.
        for codec in ["ass", "ssa", "hdmv_pgs_subtitle", "dvd_subtitle", ""] {
            assert_eq!(rust_subtitle_url(&track(codec, url)), None, "{codec}");
        }
        assert_eq!(rust_subtitle_url(&track("srt", None)), None);
    }

    /// Disarm check: with `lifted_line` returning -2 for every cue, the two-
    /// and three-line cases fail.
    #[test]
    fn a_cue_is_lifted_one_line_above_its_last_line() {
        assert_eq!(lifted_line("Hello", 80), -2.0);
        assert_eq!(lifted_line("", 80), -2.0);
        assert_eq!(lifted_line("Hello\nthere", 80), -3.0);
        assert_eq!(lifted_line("one\ntwo\nthree", 80), -4.0);
        assert_eq!(lifted_line("one\n\ntwo\n", 80), -3.0);
    }

    /// Disarm check: counting only the cue's own breaks, the wrapped cases
    /// read -2 and -3 and fail.
    #[test]
    fn a_cue_that_wraps_is_lifted_above_its_last_rendered_line() {
        let long = "a line of subtitle text long enough to wrap";
        assert_eq!(long.chars().count(), 43);
        assert_eq!(lifted_line(long, 20), -4.0);
        assert_eq!(lifted_line(long, 43), -2.0);
        assert_eq!(lifted_line(&format!("{long}\nshort"), 30), -4.0);
        // Tags take no room on screen.
        assert_eq!(lifted_line("<i>twelve chars</i>", 12), -2.0);
        assert_eq!(lifted_line("<c.yellow>x</c>", 1), -2.0);
        // Lines break at spaces: 20 characters, but no two words share a
        // 10-character line.
        assert_eq!(lifted_line("WARMER WARMER WARMER", 10), -4.0);
        // A word longer than a line breaks across lines.
        assert_eq!(lifted_line("abcdefghijklmnopqrstuvwxy", 10), -4.0);
        assert_eq!(lifted_line("   ", 10), -2.0);
        // Fullwidth glyphs take two cells: ten of them fill two
        // 10-character lines.
        assert_eq!(lifted_line("今日はいい天気ですね。", 10), -4.0);
        assert_eq!(lifted_line("今日はいい天気ですね", 10), -3.0);
        assert_eq!(lifted_line("안녕하세요", 10), -2.0);
    }

    /// Disarm check: returning `size` alone, the positioned cases read 100.
    #[test]
    fn a_cue_box_never_crosses_the_pictures_edge() {
        assert_eq!(cue_width_share(100.0, None, "auto", "center"), 100.0);
        assert_eq!(cue_width_share(40.0, None, "auto", "center"), 40.0);
        // Centred at 10%: 10% either side.
        assert_eq!(cue_width_share(100.0, Some(10.0), "auto", "center"), 20.0);
        assert_eq!(cue_width_share(100.0, Some(90.0), "center", "start"), 20.0);
        assert_eq!(
            cue_width_share(100.0, Some(30.0), "line-left", "center"),
            70.0
        );
        assert_eq!(
            cue_width_share(100.0, Some(30.0), "line-right", "center"),
            30.0
        );
        // `auto` follows the text alignment.
        // WebVTT puts an `auto` start or end cue at 50%, anchored by its
        // alignment, so it spans half the picture.
        assert_eq!(cue_width_share(100.0, None, "auto", "start"), 50.0);
        assert_eq!(cue_width_share(100.0, None, "auto", "end"), 50.0);
        assert_eq!(cue_width_share(100.0, None, "auto", "left"), 100.0);
        assert_eq!(cue_width_share(100.0, Some(25.0), "auto", "end"), 25.0);
        assert_eq!(cue_width_share(30.0, Some(50.0), "auto", "center"), 30.0);
    }

    #[test]
    fn cue_size_follows_the_player_and_its_floor() {
        // 1000px wide, tall viewport: 2.8cqw = 28px.
        assert_eq!(cue_font_px(1000.0, 2000.0, 1.0, false), 28.0);
        // Short viewport: 5vh = 20px wins.
        assert_eq!(cue_font_px(1000.0, 400.0, 1.0, false), 20.0);
        // Narrow: the 14px floor, then the scale.
        assert_eq!(cue_font_px(300.0, 800.0, 0.8, false), 14.0 * 0.8);
        // The 7rem mini player has no floor.
        assert!((cue_font_px(112.0, 800.0, 1.0, true) - 3.136).abs() < 1e-9);
        assert_eq!(cue_chars_per_line(1000.0, 28.0), 53);
        assert_eq!(cue_chars_per_line(360.0, 14.0 * 1.65), 23);
        assert_eq!(cue_chars_per_line(0.0, 14.0), usize::MAX);
        assert_eq!(cue_chars_per_line(10.0, 100.0), 1);
        // A cue of `size:20%` wraps in a fifth of the picture.
        assert_eq!(cue_chars_per_line(1000.0 * 0.2, 28.0), 10);
        assert_eq!(
            lifted_line("short words", cue_chars_per_line(1000.0 * 0.2, 28.0)),
            -3.0
        );
    }

    #[test]
    fn max_bitrate_reads_only_the_offered_choices() {
        assert_eq!(parse_max_bitrate("4000000"), 4_000_000);
        assert_eq!(parse_max_bitrate(" 1000000 "), 1_000_000);
        assert_eq!(parse_max_bitrate("0"), 0);
        assert_eq!(parse_max_bitrate("3000000"), 0);
        assert_eq!(parse_max_bitrate("+4000000"), 0);
        assert_eq!(parse_max_bitrate("fast"), 0);
        assert_eq!(parse_max_bitrate(""), 0);
        assert_eq!(max_bitrate_label(0), "auto");
        assert_eq!(max_bitrate_label(2_000_000), "2000000");
    }

    #[test]
    fn library_transition_round_trips_and_falls_back_to_fade() {
        for fx in LibraryTransition::ALL {
            assert_eq!(LibraryTransition::parse(fx.as_str()), fx);
            assert_eq!(LibraryTransition::parse(&format!(" {} ", fx.as_str())), fx);
        }
        assert_eq!(LibraryTransition::parse(""), LibraryTransition::Fade);
        assert_eq!(LibraryTransition::parse("spin"), LibraryTransition::Fade);
        assert_eq!(LibraryTransition::parse("None"), LibraryTransition::Fade);
        assert_eq!(LibraryTransition::default(), LibraryTransition::Fade);
        let mut names = LibraryTransition::ALL
            .map(LibraryTransition::as_str)
            .to_vec();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), LibraryTransition::ALL.len());
    }

    #[test]
    fn overlay_effect_round_trips_and_falls_back_to_fade() {
        for fx in OverlayEffect::ALL {
            assert_eq!(OverlayEffect::parse(fx.as_str()), fx);
            assert_eq!(OverlayEffect::parse(&format!(" {} ", fx.as_str())), fx);
        }
        assert_eq!(OverlayEffect::parse(""), OverlayEffect::Fade);
        assert_eq!(OverlayEffect::parse("spin"), OverlayEffect::Fade);
        assert_eq!(OverlayEffect::parse("None"), OverlayEffect::Fade);
        assert_eq!(OverlayEffect::default(), OverlayEffect::Fade);
        let mut names = OverlayEffect::ALL.map(OverlayEffect::as_str).to_vec();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), OverlayEffect::ALL.len());
    }

    #[test]
    fn effects_level_round_trips_and_falls_back_to_subtle() {
        for level in EffectsLevel::ALL {
            assert_eq!(EffectsLevel::parse(level.as_str()), level);
            assert_eq!(EffectsLevel::parse(&format!(" {} ", level.as_str())), level);
        }
        assert_eq!(EffectsLevel::parse(""), EffectsLevel::Subtle);
        assert_eq!(EffectsLevel::parse("loud"), EffectsLevel::Subtle);
        assert_eq!(EffectsLevel::parse("Full"), EffectsLevel::Subtle);
        assert_eq!(EffectsLevel::default(), EffectsLevel::Subtle);
        assert_eq!(
            EffectsLevel::ALL.map(EffectsLevel::label),
            ["Off", "Subtle", "Full"]
        );
        assert_eq!(EFFECTS_LEVEL_KEY, "pharos.subtitles.effects");
    }

    #[test]
    fn tier_labels_are_the_servers_tier_query_values() {
        assert_eq!(Tier::ALL.map(Tier::label), ["low", "tv", "desktop"]);
        assert!(Tier::Low < Tier::Tv && Tier::Tv < Tier::Desktop);
    }

    #[test]
    fn device_class_follows_the_user_agent_pointer_and_shell() {
        let chrome = "Mozilla/5.0 (X11; Linux x86_64) Chrome/130 Safari/537.36";
        let cases = [
            (chrome, false, false, false, DeviceClass::Desktop),
            (chrome, true, false, false, DeviceClass::Desktop),
            (chrome, false, true, false, DeviceClass::Desktop),
            (chrome, true, true, false, DeviceClass::Phone),
            (chrome, false, false, true, DeviceClass::Tv),
            (
                "Mozilla/5.0 (Linux; Android 12; Android TV) Chrome/110",
                true,
                true,
                false,
                DeviceClass::Tv,
            ),
            (
                "Mozilla/5.0 (Linux; GoogleTV 5.0) Chrome/90",
                false,
                false,
                false,
                DeviceClass::Tv,
            ),
            (
                "Mozilla/5.0 (Linux; Android 9; AFTMM) Chrome/90",
                false,
                false,
                false,
                DeviceClass::Tv,
            ),
            // A token only counts at the start of a word.
            (
                "Mozilla/5.0 (X11; Linux x86_64) Chrome/130 XAFTER/1.0",
                false,
                false,
                false,
                DeviceClass::Desktop,
            ),
            (
                "Mozilla/5.0 (Linux; BRAVIA 4K) Chrome/90",
                false,
                false,
                false,
                DeviceClass::Tv,
            ),
            (
                "Mozilla/5.0 (SMART-TV; Linux; Tizen 6.0) AppleWebKit",
                false,
                false,
                false,
                DeviceClass::Tv,
            ),
            (
                "Mozilla/5.0 (Linux; Tizen 5.5) AppleWebKit",
                false,
                false,
                false,
                DeviceClass::Tv,
            ),
            (
                "Mozilla/5.0 (Linux; Android 14; Pixel 8) Chrome/130 Mobile",
                true,
                true,
                false,
                DeviceClass::Phone,
            ),
            ("", false, false, false, DeviceClass::Desktop),
        ];
        for (agent, coarse, narrow, shell, want) in cases {
            assert_eq!(
                device_class(agent, coarse, narrow, shell),
                want,
                "{agent} {coarse} {narrow} {shell}"
            );
        }
        assert_eq!(
            [DeviceClass::Tv, DeviceClass::Phone, DeviceClass::Desktop].map(DeviceClass::label),
            ["tv", "phone", "desktop"]
        );
    }

    #[test]
    fn starting_tier_follows_device_motion_and_the_remembered_drop() {
        use DeviceClass::{Desktop, Phone, Tv};
        let cases = [
            (Desktop, false, None, Tier::Desktop),
            (Tv, false, None, Tier::Tv),
            (Phone, false, None, Tier::Tv),
            (Desktop, true, None, Tier::Low),
            (Tv, true, None, Tier::Low),
            (Desktop, false, Some(Tier::Tv), Tier::Tv),
            (Desktop, false, Some(Tier::Low), Tier::Low),
            (Desktop, false, Some(Tier::Desktop), Tier::Desktop),
            (Tv, false, Some(Tier::Desktop), Tier::Tv),
            (Phone, false, Some(Tier::Low), Tier::Low),
            (Desktop, true, Some(Tier::Desktop), Tier::Low),
        ];
        for (device, reduced, remembered, want) in cases {
            assert_eq!(
                starting_tier(device, reduced, remembered, EffectsDetail::Auto),
                want,
                "{device:?} {reduced} {remembered:?}"
            );
        }
    }

    #[test]
    fn rich_detail_asks_a_tv_for_the_desktop_tier_under_the_same_caps() {
        use DeviceClass::{Desktop, Phone, Tv};
        let rich = EffectsDetail::Rich;
        let cases = [
            (Tv, false, None, Tier::Desktop),
            (Phone, false, None, Tier::Desktop),
            (Desktop, false, None, Tier::Desktop),
            (Tv, true, None, Tier::Low),
            (Tv, false, Some(Tier::Tv), Tier::Tv),
            (Tv, false, Some(Tier::Low), Tier::Low),
        ];
        for (device, reduced, remembered, want) in cases {
            assert_eq!(
                starting_tier(device, reduced, remembered, rich),
                want,
                "{device:?} {reduced} {remembered:?}"
            );
        }
    }

    #[test]
    fn effects_detail_reads_back_what_it_stores() {
        for d in EffectsDetail::ALL {
            assert_eq!(EffectsDetail::parse(d.as_str()), d);
        }
        assert_eq!(EffectsDetail::parse("max"), EffectsDetail::Auto);
        assert_eq!(EffectsDetail::default(), EffectsDetail::Auto);
    }

    #[test]
    fn the_starting_tier_log_line_names_every_input() {
        assert_eq!(
            starting_tier_line(
                Tier::Low,
                DeviceClass::Tv,
                true,
                Some(Tier::Tv),
                EffectsDetail::Rich
            ),
            "pharos: subtitle tier=low device=tv reduced_motion=true remembered=tv detail=rich"
        );
        assert_eq!(
            starting_tier_line(
                Tier::Desktop,
                DeviceClass::Desktop,
                false,
                None,
                EffectsDetail::Auto
            ),
            "pharos: subtitle tier=desktop device=desktop reduced_motion=false remembered=none detail=auto"
        );
    }

    #[test]
    fn subtitle_size_round_trips_and_falls_back_to_medium() {
        for size in SubtitleSize::ALL {
            assert_eq!(SubtitleSize::parse(size.as_str()), size);
            assert_eq!(SubtitleSize::parse(&format!(" {} ", size.as_str())), size);
        }
        assert_eq!(SubtitleSize::parse(""), SubtitleSize::Medium);
        assert_eq!(SubtitleSize::parse("   "), SubtitleSize::Medium);
        assert_eq!(SubtitleSize::parse("huge"), SubtitleSize::Medium);
        assert_eq!(SubtitleSize::parse("Small"), SubtitleSize::Medium);
        assert_eq!(SubtitleSize::default(), SubtitleSize::Medium);
        assert_eq!(
            SubtitleSize::ALL.map(SubtitleSize::label),
            ["Small", "Medium", "Large", "Extra large"]
        );
    }

    #[test]
    fn crossfade_is_off_unless_a_listed_length_is_stored() {
        assert_eq!(parse_crossfade(""), 0);
        assert_eq!(parse_crossfade("6"), 6);
        assert_eq!(parse_crossfade(" 12 "), 12);
        assert_eq!(parse_crossfade("5"), 0);
        assert_eq!(parse_crossfade("-2"), 0);
        assert_eq!(parse_crossfade("on"), 0);
        assert_eq!(parse_crossfade("+6"), 0);
    }

    #[test]
    fn a_crossfade_is_at_most_a_third_of_the_song() {
        assert_eq!(crossfade_ms(0, 200_000), None);
        assert_eq!(crossfade_ms(6, 0), None);
        assert_eq!(crossfade_ms(6, 200_000), Some(6_000));
        assert_eq!(crossfade_ms(12, 9_000), Some(3_000));
    }

    #[test]
    fn a_fade_runs_in_a_straight_line_and_ends_on_its_target() {
        assert!((fade_volume(0.8, 0.0, 0.0, 4000.0) - 0.8).abs() < 1e-9);
        assert!((fade_volume(0.8, 0.0, 2000.0, 4000.0) - 0.4).abs() < 1e-9);
        assert!((fade_volume(0.0, 0.6, 1000.0, 4000.0) - 0.15).abs() < 1e-9);
        assert_eq!(fade_volume(0.8, 0.0, 9000.0, 4000.0), 0.0);
        assert_eq!(fade_volume(0.8, 0.5, 10.0, 0.0), 0.5);
    }

    #[test]
    fn a_stored_volume_reads_back_as_it_was_set() {
        for (volume, muted) in [(0.35, false), (0.0, true), (1.0, false), (0.5, true)] {
            assert_eq!(
                parse_volume_setting(&volume_setting(volume, muted)),
                Some((volume, muted))
            );
        }
        assert_eq!(volume_setting(1.7, false), "1.000");
    }

    #[test]
    fn a_new_player_starts_at_the_saved_volume() {
        let state = MediaState::at_volume(Some((0.3, true)));
        assert_eq!((state.volume, state.muted), (0.3, true));
        assert!(!state.loading);
        assert_eq!(MediaState::at_volume(None), MediaState::default());
    }

    #[test]
    fn loading_shows_while_seeking_or_playing_without_future_data() {
        // Seeking always counts as loading, whether playing or paused.
        assert!(is_loading(true, 4, false, false));
        assert!(is_loading(true, 4, true, false));
        assert!(is_loading(true, 0, true, false));
        // Initial autoplay load (readyState < 2, unpaused) and mid-play stalls
        // (readyState == 2 HAVE_CURRENT_DATA, unpaused) are loading.
        assert!(is_loading(false, 0, false, false));
        assert!(is_loading(false, 1, false, false));
        assert!(is_loading(false, 2, false, false));
        // HAVE_FUTURE_DATA (3) and HAVE_ENOUGH_DATA (4) are ready to play.
        assert!(!is_loading(false, 3, false, false));
        assert!(!is_loading(false, 4, false, false));
        // Paused or ended without an active seek is not waiting on data.
        assert!(!is_loading(false, 0, true, false));
        assert!(!is_loading(false, 2, true, false));
        assert!(!is_loading(false, 2, false, true));
    }

    #[test]
    fn a_stored_volume_out_of_range_or_garbled_is_ignored() {
        for stored in ["", "loud", "1.5", "-0.1", "NaN", "0.5 quiet", " muted"] {
            assert_eq!(parse_volume_setting(stored), None, "{stored:?}");
        }
    }

    /// Buffering and readiness are for a watch-together group, not the
    /// server's progress report: they send nothing and do not disturb the
    /// cadence.
    #[test]
    fn waiting_and_can_play_report_nothing() {
        let mut clock = ReportClock::new();
        assert_eq!(
            clock.on(0.0, MediaEvent::Playing),
            Some(Outgoing::Report { paused: false })
        );
        assert_eq!(clock.on(1.0, MediaEvent::Waiting), None);
        assert_eq!(clock.on(2.0, MediaEvent::CanPlay), None);
        assert_eq!(clock.on(3.0, MediaEvent::TimeUpdate), None);
        assert_eq!(
            clock.on(REPORT_INTERVAL_MS, MediaEvent::TimeUpdate),
            Some(Outgoing::Report { paused: false })
        );
    }

    fn sprites() -> Trickplay {
        Trickplay {
            width: 320,
            height: 180,
            interval_ms: 10_000,
            thumb_count: 250,
            tile_count: 3,
            grid: 10,
            tile_url: "/videos/7/trickplay/320/{tile}.jpg".into(),
        }
    }

    #[test]
    fn a_thumbnail_is_found_by_tile_column_and_row() {
        let set = sprites();
        let at = |ms| thumb_at(&set, ms).map(|t| (t.url, t.x, t.y));
        assert_eq!(at(0), Some(("/videos/7/trickplay/320/0.jpg".into(), 0, 0)));
        // Thumbnail 23: tile 0, row 2, column 3.
        assert_eq!(
            at(235_000),
            Some(("/videos/7/trickplay/320/0.jpg".into(), 960, 360))
        );
        // Thumbnail 105: tile 1, row 0, column 5.
        assert_eq!(
            at(1_050_000),
            Some(("/videos/7/trickplay/320/1.jpg".into(), 1600, 0))
        );
        // Past the end: the last thumbnail, 249 (tile 2, row 4, column 9).
        assert_eq!(
            at(99_000_000),
            Some(("/videos/7/trickplay/320/2.jpg".into(), 2880, 720))
        );
        let empty = Trickplay {
            thumb_count: 0,
            ..sprites()
        };
        assert_eq!(thumb_at(&empty, 0), None);
    }

    #[test]
    fn the_preview_uses_the_narrowest_sprites() {
        let wide = Trickplay {
            width: 640,
            ..sprites()
        };
        let sets = [wide, sprites()];
        assert_eq!(preview_set(&sets).map(|s| s.width), Some(320));
        assert_eq!(preview_set(&[]), None);
    }

    #[test]
    fn credits_are_the_latest_outro_in_the_second_half() {
        let at = |kind, start_ms| Segment {
            kind,
            start_ms,
            end_ms: start_ms + 60_000,
        };
        let total = 1_200_000;
        assert_eq!(credits_start(&[], total), None);
        assert_eq!(
            credits_start(&[at(SegmentKind::Outro, 1_100_000)], total),
            Some(1_100_000)
        );
        // A cold open's credits, an intro, or an outro past the end are not the end.
        assert_eq!(
            credits_start(&[at(SegmentKind::Outro, 30_000)], total),
            None
        );
        assert_eq!(
            credits_start(&[at(SegmentKind::Intro, 1_100_000)], total),
            None
        );
        assert_eq!(credits_start(&[at(SegmentKind::Outro, total)], total), None);
        assert_eq!(
            credits_start(
                &[
                    at(SegmentKind::Outro, 900_000),
                    at(SegmentKind::Outro, 1_000_000)
                ],
                total
            ),
            Some(1_000_000)
        );
    }

    #[test]
    fn what_is_left_stops_at_the_credits_until_they_play() {
        let outro = Segment {
            kind: SegmentKind::Outro,
            start_ms: 1_100_000,
            end_ms: 1_200_000,
        };
        let total = 1_200_000;
        assert_eq!(
            left_to_watch(&[outro], 100_000, total),
            Some(Left {
                ms: 1_000_000,
                before_credits: true
            })
        );
        assert_eq!(
            left_to_watch(&[outro], 1_150_000, total),
            Some(Left {
                ms: 50_000,
                before_credits: false
            })
        );
        assert_eq!(
            left_to_watch(&[], 200_000, total),
            Some(Left {
                ms: 1_000_000,
                before_credits: false
            })
        );
        assert_eq!(left_to_watch(&[], total, total), None);
        assert_eq!(left_to_watch(&[], 5, 0), None);
    }

    #[test]
    fn the_estimate_says_when_it_stops_before_the_credits() {
        assert_eq!(ends_at_label("10:42 PM", false), "Ends at 10:42 PM");
        assert_eq!(
            ends_at_label("22:40", true),
            "Ends at 22:40 (before credits)"
        );
    }

    #[test]
    fn up_next_shows_at_the_credits_or_a_share_of_the_runtime_from_the_end() {
        let at = |kind, start_ms| Segment {
            kind,
            start_ms,
            end_ms: start_ms + 60_000,
        };
        let credits = |at_ms| {
            Some(UpNextFrom {
                at_ms,
                credits: true,
            })
        };
        let guess = |at_ms| {
            Some(UpNextFrom {
                at_ms,
                credits: false,
            })
        };
        let total = 1_200_000;
        assert_eq!(up_next_at(&[], total), guess(1_140_000));
        assert_eq!(up_next_at(&[], 0), None);
        assert_eq!(
            up_next_at(&[at(SegmentKind::Outro, 1_100_000)], total),
            credits(1_100_000)
        );
        // An intro, or credits in the first half (a cold open's), are not the end.
        assert_eq!(
            up_next_at(
                &[
                    at(SegmentKind::Intro, 1_100_000),
                    at(SegmentKind::Outro, 30_000)
                ],
                total
            ),
            guess(1_140_000)
        );
        // A 22-minute episode's last 66 s; never more than 90 s or less
        // than 20 s, and never the first half of a short clip.
        assert_eq!(up_next_at(&[], 1_320_000), guess(1_254_000));
        assert_eq!(up_next_at(&[], 3_600_000), guess(3_510_000));
        assert_eq!(up_next_at(&[], 300_000), guess(280_000));
        assert_eq!(up_next_at(&[], 5_000), guess(2_500));
    }

    #[test]
    fn up_next_counts_down_from_where_it_showed_and_then_goes() {
        let mut up = UpNext::default();
        let from = Some(UpNextFrom {
            at_ms: 100_000,
            credits: true,
        });
        let total = 400_000;
        let counting = |left_ms| UpNextStep::Counting {
            left_ms,
            of_ms: 15_000,
        };
        assert_eq!(up.step(99_999, from, total), UpNextStep::Hidden);
        assert_eq!(up.step(100_000, from, total), counting(15_000));
        assert_eq!(up.step(110_000, from, total), counting(5_000));
        assert_eq!(up.step(115_000, from, total), UpNextStep::Go);
        // Seeking back hides it; coming back starts the count again.
        assert_eq!(up.step(50_000, from, total), UpNextStep::Hidden);
        assert_eq!(up.step(200_000, from, total), counting(15_000));
        up.cancel();
        assert_eq!(up.step(210_000, from, total), UpNextStep::Hidden);
    }

    #[test]
    fn up_next_from_a_guess_counts_to_the_end() {
        let mut up = UpNext::default();
        let from = Some(UpNextFrom {
            at_ms: 1_140_000,
            credits: false,
        });
        let total = 1_200_000;
        assert_eq!(
            up.step(1_140_000, from, total),
            UpNextStep::Counting {
                left_ms: 60_000,
                of_ms: 60_000
            }
        );
        assert_eq!(
            up.step(1_170_000, from, total),
            UpNextStep::Counting {
                left_ms: 30_000,
                of_ms: 60_000
            }
        );
        assert_eq!(up.step(1_200_000, from, total), UpNextStep::Go);
    }

    /// Disarm check: without the reset on a seek back, the second step reads
    /// `of_ms: 5_000` beside `left_ms: 60_000`.
    #[test]
    fn up_next_counts_again_from_a_seek_back_inside_its_window() {
        let mut up = UpNext::default();
        let from = Some(UpNextFrom {
            at_ms: 1_140_000,
            credits: false,
        });
        let total = 1_200_000;
        assert_eq!(
            up.step(1_195_000, from, total),
            UpNextStep::Counting {
                left_ms: 5_000,
                of_ms: 5_000
            }
        );
        assert_eq!(
            up.step(1_140_000, from, total),
            UpNextStep::Counting {
                left_ms: 60_000,
                of_ms: 60_000
            }
        );
    }

    #[test]
    fn up_next_moves_on_at_the_count_or_the_end_unless_cancelled() {
        let mut up = UpNext::default();
        let counting = UpNextStep::Counting {
            left_ms: 3_000,
            of_ms: 15_000,
        };
        assert_eq!(
            up.moves_on(UpNextStep::Go, MediaEvent::TimeUpdate),
            Some("countdown")
        );
        assert_eq!(up.moves_on(counting, MediaEvent::TimeUpdate), None);
        assert_eq!(
            up.moves_on(UpNextStep::Hidden, MediaEvent::Ended),
            Some("ended")
        );
        assert_eq!(up.moves_on(UpNextStep::Hidden, MediaEvent::Pause), None);
        up.cancel();
        assert_eq!(up.moves_on(UpNextStep::Hidden, MediaEvent::Ended), None);
    }

    #[test]
    fn up_next_never_counts_past_the_end() {
        let mut up = UpNext::default();
        let from = Some(UpNextFrom {
            at_ms: 390_000,
            credits: true,
        });
        assert_eq!(
            up.step(395_000, from, 400_000),
            UpNextStep::Counting {
                left_ms: 5_000,
                of_ms: 5_000
            }
        );
        assert_eq!(up.step(400_000, from, 400_000), UpNextStep::Go);
        assert_eq!(UpNext::default().step(0, None, 0), UpNextStep::Hidden);
    }

    #[test]
    fn the_skip_button_shows_only_inside_a_segment() {
        let intro = Segment {
            kind: SegmentKind::Intro,
            start_ms: 5_000,
            end_ms: 65_000,
        };
        let segments = [intro];
        assert_eq!(segment_at(&segments, 4_999), None);
        assert_eq!(segment_at(&segments, 5_000), Some(intro));
        assert_eq!(segment_at(&segments, 64_999), Some(intro));
        assert_eq!(segment_at(&segments, 65_000), None);
        assert_eq!(skip_label(intro.kind), "Skip intro");
    }

    #[test]
    fn seek_bar_maths_clamps_to_the_runtime() {
        assert_eq!(percent(30_000, 120_000), 25.0);
        assert_eq!(percent(500_000, 120_000), 100.0);
        assert_eq!(percent(1, 0), 0.0);
        assert_eq!(position_at(50.0, 200.0, 120_000), Some(30_000));
        assert_eq!(position_at(-5.0, 200.0, 120_000), Some(0));
        assert_eq!(position_at(400.0, 200.0, 120_000), Some(120_000));
        assert_eq!(position_at(10.0, 0.0, 120_000), None);
        assert_eq!(position_at(10.0, 200.0, 0), None);
    }

    #[test]
    fn clock_shows_hours_only_when_there_are_some() {
        assert_eq!(clock(0), "0:00");
        assert_eq!(clock(245_000), "4:05");
        assert_eq!(clock(3_723_000), "1:02:03");
    }

    #[test]
    fn an_image_cue_shows_from_its_start_until_its_end() {
        let cue = |start_ms, end_ms| ImageCue {
            start_ms,
            end_ms,
            x: 0,
            y: 0,
            width: 1,
            height: 1,
            url: format!("/{start_ms}.png"),
        };
        let cues = ImageCues {
            width: 720,
            height: 480,
            complete: true,
            events: vec![cue(500, 2_500), cue(4_000, 6_000)],
        };
        let at = |ms| cues_at(&cues, ms).map(|c| c.start_ms).collect::<Vec<_>>();
        assert_eq!(at(499), Vec::<u64>::new());
        assert_eq!(at(500), [500]);
        assert_eq!(at(2_499), [500]);
        assert_eq!(at(2_500), Vec::<u64>::new());
        assert_eq!(at(5_000), [4_000]);
    }

    #[test]
    fn the_server_manifest_reads_as_image_cues() {
        let raw = r#"{"width":720,"height":480,"complete":false,"through_ms":6000,
            "events":[{"start_ms":500,"end_ms":2500,"x":240,"y":400,"width":240,
            "height":48,"forced":false,"url":"/videos/9/subtitles/3/images/0.png"}]}"#;
        let cues: ImageCues = serde_json::from_str(raw).unwrap();
        assert!(!cues.complete);
        assert_eq!(cues.events[0].url, "/videos/9/subtitles/3/images/0.png");
        assert_eq!(share(cues.events[0].x, cues.width), 240.0 * 100.0 / 720.0);
        assert_eq!(share(1, 0), 0.0);
    }

    #[test]
    fn every_browser_declares_the_image_subtitles_it_draws() {
        let caps = capabilities(|_| false, |_| false);
        assert_eq!(caps.image_subtitles, IMAGE_SUBTITLES);
    }

    #[test]
    fn tracks_are_named_by_language_sound_and_a_title_that_adds_something() {
        let mut t = Track {
            language: "eng".into(),
            title: "Commentary".into(),
            codec: "aac".into(),
            ..Track::default()
        };
        assert_eq!(track_label(&t, 0), "English · AAC · Commentary");
        t.is_forced = true;
        assert_eq!(track_label(&t, 0), "English · AAC · Commentary (forced)");
        let subtitle = Track {
            language: "jpn".into(),
            title: "Signs".into(),
            codec: "subrip".into(),
            ..Track::default()
        };
        assert_eq!(track_label(&subtitle, 0), "Japanese · Signs");
        let untagged = Track {
            title: "Commentary".into(),
            ..Track::default()
        };
        assert_eq!(track_label(&untagged, 0), "Commentary");
        assert_eq!(track_label(&Track::default(), 2), "Track 3");
    }

    #[test]
    fn audio_is_named_with_its_layout_and_a_title_only_when_it_adds_something() {
        assert_eq!(
            audio_label("eng", "aac", Some(6), "Surround", 0),
            "English · AAC 5.1"
        );
        assert_eq!(
            audio_label("jpn", "aac", Some(6), "Surround", 0),
            "Japanese · AAC 5.1"
        );
        assert_eq!(
            audio_label("eng", "aac", Some(6), "Director's commentary", 0),
            "English · AAC 5.1 · Director's commentary"
        );
        assert_eq!(
            audio_label("eng", "aac", None, "Surround", 0),
            "English · AAC"
        );
        assert_eq!(audio_label("", "", None, "", 0), "Track 1");
        assert_eq!(audio_label("und", "aac", None, "", 0), "AAC");
    }

    #[test]
    fn channel_layouts_are_named_by_their_usual_names() {
        assert_eq!(channel_name(1).as_deref(), Some("Mono"));
        assert_eq!(channel_name(2).as_deref(), Some("Stereo"));
        assert_eq!(channel_name(6).as_deref(), Some("5.1"));
        assert_eq!(channel_name(8).as_deref(), Some("7.1"));
        assert_eq!(channel_name(0), None);
    }

    #[test]
    fn every_kind_of_language_code_reads_as_its_name() {
        let named = |language: &str, title: &str, forced: bool, sdh: bool| {
            let t = Track {
                language: language.into(),
                title: title.into(),
                codec: "subrip".into(),
                is_forced: forced,
                is_hearing_impaired: sdh,
                ..Track::default()
            };
            track_label(&t, 0)
        };
        assert_eq!(named("gre", "", false, false), "Greek");
        assert_eq!(named("ell", "", false, false), "Greek");
        assert_eq!(named("rum", "", false, false), "Romanian");
        assert_eq!(named("heb", "", false, false), "Hebrew");
        assert_eq!(named("ind", "", false, false), "Indonesian");
        assert_eq!(named("tha", "", false, false), "Thai");
        assert_eq!(named("pt-BR", "", false, false), "Portuguese");
        assert_eq!(
            named("por", "Brazilian", false, false),
            "Portuguese · Brazilian"
        );
        assert_eq!(
            named("chi", "Simplified", false, false),
            "Chinese · Simplified"
        );
        assert_eq!(named("eng", "SDH", false, true), "English · SDH");
        assert_eq!(named("eng", "Forced", true, false), "English · Forced");
        assert_eq!(named("eng", "", true, false), "English (forced)");
        assert_eq!(named("qaa", "", false, false), "QAA");
    }

    #[test]
    fn only_a_burned_subtitle_needs_a_new_playback() {
        let text = Track {
            vtt_url: Some("/videos/7/subtitles/3/stream.vtt".into()),
            ..Track::default()
        };
        let image = Track::default();
        let drawn = Track {
            images_url: Some("/videos/7/subtitles/4/images.json".into()),
            ..Track::default()
        };
        assert_eq!(subtitle_switch(false, Some(&text)), SubtitleSwitch::InPlace);
        assert_eq!(
            subtitle_switch(false, Some(&drawn)),
            SubtitleSwitch::InPlace
        );
        assert_eq!(
            subtitle_switch(true, Some(&drawn)),
            SubtitleSwitch::Renegotiate
        );
        assert_eq!(subtitle_switch(false, None), SubtitleSwitch::InPlace);
        assert_eq!(
            subtitle_switch(false, Some(&image)),
            SubtitleSwitch::Renegotiate
        );
        assert_eq!(
            subtitle_switch(true, Some(&text)),
            SubtitleSwitch::Renegotiate
        );
        assert_eq!(subtitle_switch(true, None), SubtitleSwitch::Renegotiate);
    }

    /// B635: the pick set again to the drawn track keeps its renderer; a new
    /// script or none rebuilds or tears it down.
    #[test]
    fn the_ass_renderer_is_rebuilt_only_for_a_new_script() {
        let a = Some("/videos/1/subtitles/4/stream.ass?tier=desktop&effects=subtle");
        let b = Some("/videos/1/subtitles/4/stream.ass?tier=low&effects=subtle");
        assert!(restart_styled(None, a), "first script");
        assert!(!restart_styled(a, a), "same script again: no second fetch");
        assert!(restart_styled(a, b), "a tier drop reloads");
        assert!(restart_styled(a, None), "no script tears it down");
        assert!(restart_styled(None, None));
    }

    #[test]
    fn a_styled_track_is_fetched_at_the_tier_and_level() {
        let styled = Track {
            vtt_url: Some("/videos/7/subtitles/3/stream.vtt".into()),
            styled_url: Some("/videos/7/subtitles/3/stream.ass".into()),
            ..Track::default()
        };
        assert_eq!(
            styled_track_url(
                &styled,
                Tier::Tv,
                EffectsLevel::Subtle,
                SubtitleSize::Medium,
                false
            )
            .as_deref(),
            Some("/videos/7/subtitles/3/stream.ass?tier=tv&effects=subtle")
        );
        assert_eq!(
            styled_track_url(
                &styled,
                Tier::Desktop,
                EffectsLevel::Full,
                SubtitleSize::Medium,
                false
            )
            .as_deref(),
            Some("/videos/7/subtitles/3/stream.ass?tier=desktop&effects=full")
        );
        // B852: the size choice reaches the styled track.
        assert_eq!(
            styled_track_url(
                &styled,
                Tier::Tv,
                EffectsLevel::Subtle,
                SubtitleSize::ExtraLarge,
                false
            )
            .as_deref(),
            Some("/videos/7/subtitles/3/stream.ass?tier=tv&effects=subtle&size=150")
        );
        assert_eq!(
            styled_track_url(
                &styled,
                Tier::Tv,
                EffectsLevel::Subtle,
                SubtitleSize::Small,
                false
            )
            .as_deref(),
            Some("/videos/7/subtitles/3/stream.ass?tier=tv&effects=subtle&size=75")
        );
        assert_eq!(
            styled_track_url(
                &styled,
                Tier::Low,
                EffectsLevel::Off,
                SubtitleSize::Large,
                false
            ),
            None
        );
        assert_eq!(
            styled_track_url(
                &styled,
                Tier::Tv,
                EffectsLevel::Subtle,
                SubtitleSize::Small,
                true
            )
            .as_deref(),
            Some("/videos/7/subtitles/3/stream.ass?tier=tv&effects=subtle&size=75&speakers=1")
        );
        let source = Track {
            styled_url: None,
            ..styled
        };
        assert_eq!(
            styled_track_url(
                &source,
                Tier::Tv,
                EffectsLevel::Full,
                SubtitleSize::Large,
                false
            ),
            None
        );
    }

    fn styled_pair() -> Vec<Track> {
        let vtt = "/videos/7/subtitles/3/stream.vtt";
        vec![
            Track {
                index: 1,
                vtt_url: Some("/videos/7/subtitles/1/stream.vtt".into()),
                ..Track::default()
            },
            Track {
                index: 3,
                codec: "ass".into(),
                vtt_url: Some(vtt.into()),
                ..Track::default()
            },
            Track {
                index: 9,
                codec: "ass".into(),
                vtt_url: Some(vtt.into()),
                styled_url: Some("/videos/7/subtitles/3/stream.ass".into()),
                ..Track::default()
            },
        ]
    }

    #[test]
    fn effects_off_hides_the_styled_track_from_the_picker() {
        let tracks = styled_pair();
        let indexes = |level| -> Vec<u32> {
            offered_subtitles(&tracks, level)
                .iter()
                .map(|t| t.index)
                .collect()
        };
        assert_eq!(indexes(EffectsLevel::Off), [1, 3]);
        assert_eq!(indexes(EffectsLevel::Subtle), [1, 3, 9]);
        assert_eq!(indexes(EffectsLevel::Full), [1, 3, 9]);
    }

    #[test]
    fn the_source_of_a_styled_track_is_the_one_with_its_plain_text() {
        let tracks = styled_pair();
        assert_eq!(source_of_styled(&tracks, 9), Some(3));
        // Not generated: nothing to switch from.
        assert_eq!(source_of_styled(&tracks, 3), None);
        assert_eq!(source_of_styled(&tracks, 1), None);
        assert_eq!(source_of_styled(&tracks, 42), None);
        // The source is no longer listed: the player turns subtitles off.
        let orphan: Vec<Track> = tracks.into_iter().filter(|t| t.index != 3).collect();
        assert_eq!(source_of_styled(&orphan, 9), None);
    }

    #[test]
    fn only_an_ass_text_subtitle_is_drawn_from_its_script() {
        let track = |codec: &str, vtt: Option<&str>| Track {
            codec: codec.into(),
            vtt_url: vtt.map(Into::into),
            ..Track::default()
        };
        let vtt = Some("/videos/7/subtitles/3/stream.vtt");
        assert_eq!(
            styled_subtitle_url(&track("ass", vtt)).as_deref(),
            Some("/videos/7/subtitles/3/stream.ass")
        );
        assert_eq!(
            styled_subtitle_url(&track("SSA", vtt)).as_deref(),
            Some("/videos/7/subtitles/3/stream.ass")
        );
        assert_eq!(styled_subtitle_url(&track("subrip", vtt)), None);
        assert_eq!(styled_subtitle_url(&track("webvtt", vtt)), None);
        // Burned, or a URL of another shape: nothing to fetch the script from.
        assert_eq!(styled_subtitle_url(&track("ass", None)), None);
        assert_eq!(
            styled_subtitle_url(&track("ass", Some("/elsewhere.vtt"))),
            None
        );
    }

    #[test]
    fn hls_prefers_native_then_hlsjs() {
        let js = MseEngine::HlsJs;
        assert_eq!(choose_engine(false, false, false, js), Engine::Direct);
        assert_eq!(choose_engine(false, true, true, js), Engine::Direct);
        assert_eq!(choose_engine(true, true, true, js), Engine::NativeHls);
        assert_eq!(choose_engine(true, false, true, js), Engine::HlsJs);
        assert_eq!(choose_engine(true, false, false, js), Engine::Unsupported);
    }

    #[test]
    fn the_rust_engine_only_replaces_hlsjs() {
        let rust = MseEngine::Rust;
        assert_eq!(choose_engine(false, false, true, rust), Engine::Direct);
        assert_eq!(choose_engine(true, true, true, rust), Engine::NativeHls);
        assert_eq!(choose_engine(true, false, true, rust), Engine::RustHls);
        assert_eq!(choose_engine(true, false, false, rust), Engine::Unsupported);
    }

    /// V630: override, then setting, then default; junk falls through.
    #[test]
    fn mse_engine_is_override_then_setting_then_default() {
        use EngineReason::{Default, Override, Setting};
        assert_eq!(pick_mse_engine("", None), (MseEngine::HlsJs, Default));
        assert_eq!(
            pick_mse_engine("", Some("rust")),
            (MseEngine::Rust, Setting)
        );
        assert_eq!(
            pick_mse_engine("", Some("hlsjs")),
            (MseEngine::HlsJs, Setting)
        );
        assert_eq!(
            pick_mse_engine("", Some("shaka")),
            (MseEngine::HlsJs, Default)
        );
        assert_eq!(
            pick_mse_engine("?a=1&engine=rust", Some("hlsjs")),
            (MseEngine::Rust, Override)
        );
        assert_eq!(
            pick_mse_engine("?engine=hlsjs", Some("rust")),
            (MseEngine::HlsJs, Override)
        );
        assert_eq!(
            pick_mse_engine("?engine=nope", Some("rust")),
            (MseEngine::Rust, Setting)
        );
        assert_eq!(
            pick_mse_engine("?xengine=rust", None),
            (MseEngine::HlsJs, Default)
        );
        assert_eq!(
            pick_mse_engine("?engine=nope&engine=rust", Some("hlsjs")),
            (MseEngine::Rust, Override)
        );
    }

    #[test]
    fn mse_engine_names_round_trip_and_are_distinct() {
        for engine in MseEngine::ALL {
            assert_eq!(MseEngine::parse(engine.as_str()), Some(engine));
        }
        assert_ne!(MseEngine::HlsJs.as_str(), MseEngine::Rust.as_str());
    }

    #[test]
    fn engine_labels_are_distinct() {
        let all = [
            Engine::Direct,
            Engine::NativeHls,
            Engine::HlsJs,
            Engine::RustHls,
            Engine::Unsupported,
        ];
        let labels: std::collections::BTreeSet<_> = all.iter().map(|e| engine_label(*e)).collect();
        assert_eq!(labels.len(), all.len());
    }

    #[test]
    fn only_the_rust_engine_adapts_quality() {
        assert!(Engine::RustHls.adapts_quality());
        for e in [
            Engine::HlsJs,
            Engine::NativeHls,
            Engine::Direct,
            Engine::Unsupported,
        ] {
            assert!(!e.adapts_quality(), "{e:?}");
        }
    }

    #[test]
    fn with_abr_adds_the_flag_to_either_query_shape() {
        assert_eq!(with_abr("/m.m3u8"), "/m.m3u8?abr=1");
        assert_eq!(with_abr("/m.m3u8?api_key=k"), "/m.m3u8?api_key=k&abr=1");
    }

    const ABR_MASTER: &str = "#EXTM3U\n#EXT-X-VERSION:7\n\
        #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",NAME=\"Audio\",DEFAULT=YES,URI=\"a.m3u8\"\n\
        #EXT-X-STREAM-INF:BANDWIDTH=8128000,CODECS=\"avc1.640028,opus\",AUDIO=\"aud\",RESOLUTION=1920x1080\n\
        main.m3u8\n\
        #EXT-X-STREAM-INF:BANDWIDTH=3128000,CODECS=\"avc1.640028,opus\",AUDIO=\"aud\",RESOLUTION=1280x720\n\
        720p/main.m3u8\n\
        #EXT-X-STREAM-INF:BANDWIDTH=928000,CODECS=\"avc1.640028,opus\",AUDIO=\"aud\",RESOLUTION=640x360\n\
        360p/main.m3u8\n\
        #EXT-X-STREAM-INF:BANDWIDTH=2000000,CODECS=\"vp09.00.40.08,opus\",AUDIO=\"aud\"\n\
        vp9/main.m3u8\n";

    // Abr indexes its ladder lowest bandwidth first; the master lists the
    // source rung first. The entries must follow Abr's order, or a switch
    // fetches the wrong rung.
    #[test]
    fn abr_ladder_is_lowest_first_and_keeps_only_the_first_variants_codecs() {
        let master =
            pharos_hls::playlist::parse_master(ABR_MASTER, "https://h/videos/9/master.m3u8")
                .unwrap();
        let ladder = abr_ladder(&master);
        let got: Vec<(u64, Option<u32>, &str)> = ladder
            .iter()
            .map(|e| (e.rung.bandwidth_bps, e.rung.height, e.uri.as_str()))
            .collect();
        assert_eq!(
            got,
            vec![
                (928_000, Some(360), "https://h/videos/9/360p/main.m3u8"),
                (3_128_000, Some(720), "https://h/videos/9/720p/main.m3u8"),
                (8_128_000, Some(1080), "https://h/videos/9/main.m3u8"),
            ]
        );
        let abr = pharos_hls::abr::Abr::new(
            ladder.iter().map(|e| e.rung).collect(),
            pharos_hls::abr::AbrConfig::default(),
        );
        let order: Vec<_> = abr.ladder().iter().map(|r| r.bandwidth_bps).collect();
        let ours: Vec<_> = ladder.iter().map(|e| e.rung.bandwidth_bps).collect();
        assert_eq!(order, ours, "entries and Abr disagree on rung order");
    }

    #[test]
    fn abr_ladder_of_a_master_without_variants_is_empty() {
        let master = pharos_hls::playlist::parse_master("#EXTM3U\n", "https://h/m.m3u8").unwrap();
        assert!(abr_ladder(&master).is_empty());
    }

    #[test]
    fn rung_labels_name_the_height_or_the_bandwidth() {
        use pharos_hls::abr::Rung;
        let r = |bandwidth_bps, height| Rung {
            bandwidth_bps,
            height,
        };
        assert_eq!(rung_label(&r(3_128_000, Some(720))), "720p");
        assert_eq!(rung_label(&r(928_000, None)), "928k");
    }

    #[test]
    fn forward_buffer_is_measured_in_the_range_holding_the_play_head() {
        let ranges = [(0, 6_000), (12_000, 30_000)];
        assert_eq!(forward_buffer_ms(&ranges, 2_000), 4_000);
        assert_eq!(forward_buffer_ms(&ranges, 12_000), 18_000);
        assert_eq!(forward_buffer_ms(&ranges, 8_000), 0, "in a gap");
        assert_eq!(forward_buffer_ms(&ranges, 30_000), 0, "at the end");
        assert_eq!(forward_buffer_ms(&[], 0), 0);
    }

    // V702: every stall verdict is logged and reported with the play head's
    // surroundings.
    #[test]
    fn a_stall_verdict_is_logged_and_reported_with_its_surroundings() {
        use pharos_hls::stall::Verdict;
        let r = StallReport {
            verdict: Verdict::Jump {
                from_ms: 10_000,
                to_ms: 10_500,
                gap_ms: 400,
                attempt: 1,
            },
            video_ahead_ms: 0,
            audio_ahead_ms: 2_000,
            in_flight: "video",
        };
        assert_eq!(
            r.line(),
            "pharos: playback stall engine=rust action=jump at_ms=10000 to_ms=10500 gap_ms=400 attempt=1 stalled_ms=none video_ahead_ms=0 audio_ahead_ms=2000 in_flight=video"
        );
        let e = r.event("/ui/watch/9".to_owned());
        assert_eq!(e.kind, ClientEventKind::PlaybackStall);
        assert_eq!(e.level, ClientLevel::Warn);
        let keys: Vec<_> = e.fields.iter().map(|(k, _)| *k).collect();
        assert_eq!(
            keys,
            [
                "engine",
                "action",
                "at_ms",
                "to_ms",
                "gap_ms",
                "attempt",
                "stalled_ms",
                "video_ahead_ms",
                "audio_ahead_ms",
                "in_flight"
            ]
        );
        let gave_up = StallReport {
            verdict: Verdict::GiveUp {
                at_ms: 30_000,
                stalled_ms: 8_000,
                attempts: 3,
            },
            ..r
        };
        assert!(gave_up.line().contains(
            "action=give_up at_ms=30000 to_ms=none gap_ms=none attempt=3 stalled_ms=8000"
        ));
        assert_eq!(gave_up.event(String::new()).level, ClientLevel::Error);
    }

    // V649: every switch is logged and reported with from, to, reason,
    // estimate, buffer and cold-encode time.
    #[test]
    fn an_abr_switch_is_logged_and_reported_with_its_inputs() {
        let r = AbrSwitchReport {
            from: "1080p".to_owned(),
            to: "480p".to_owned(),
            reason: pharos_hls::abr::Reason::Bandwidth.label(),
            estimate_bps: Some(1_234_567),
            buffer_ms: 9_000,
            cold_ms: 2_500,
            applied: true,
        };
        assert_eq!(
            r.line(),
            "pharos: abr switch from=1080p to=480p reason=bandwidth est_kbps=1234 buffer_ms=9000 cold_ms=2500 applied=true"
        );
        let e = r.event("/ui/watch/9".to_owned());
        assert_eq!(e.kind, ClientEventKind::QualitySwitch);
        assert_eq!(e.level, ClientLevel::Info);
        let keys: Vec<_> = e.fields.iter().map(|(k, _)| *k).collect();
        assert_eq!(
            keys,
            [
                "engine",
                "from",
                "to",
                "reason",
                "est_kbps",
                "buffer_ms",
                "cold_ms",
                "applied"
            ]
        );
        let refused = AbrSwitchReport {
            estimate_bps: None,
            applied: false,
            ..r
        };
        assert!(refused.line().contains("est_kbps=none"));
        assert!(refused.line().ends_with("applied=false"));
        assert_eq!(refused.event(String::new()).level, ClientLevel::Warn);
    }

    // V637: every failed segment request logs and reports its cause, the
    // deadline that passed, the rung and what the engine does next.
    #[test]
    fn a_segment_failure_is_logged_and_reported_with_its_recovery() {
        use pharos_hls::retry::{Cause, GiveUp};
        let r = SegmentFailureReport {
            track: "video",
            index: "7".to_owned(),
            cause: Cause::TimeoutFirstByte,
            deadline_ms: Some(10_000),
            recovery: SegmentRecovery::Retry {
                attempt: 2,
                after_ms: 2_000,
            },
            rung: "720p".to_owned(),
            reason: "no response".to_owned(),
            url: "/s/7.m4s".to_owned(),
        };
        assert_eq!(
            r.line(),
            "pharos: playback error engine=rust type=network track=video index=7 cause=timeout_first_byte status=none deadline_ms=10000 rung=720p action=retry attempt=2 after_ms=2000 give_up=none reason=no response url=/s/7.m4s"
        );
        assert!(!r.fatal());
        let e = r.event("/ui/watch/9".to_owned());
        assert_eq!(e.kind, ClientEventKind::PlaybackError);
        assert_eq!(e.level, ClientLevel::Warn);
        let get = |e: &ClientEvent, k: &str| {
            e.fields
                .iter()
                .find(|(key, _)| *key == k)
                .map(|(_, v)| v.clone())
        };
        assert_eq!(get(&e, "type").as_deref(), Some("networkError"));
        assert_eq!(get(&e, "details").as_deref(), Some("timeout_first_byte"));
        assert_eq!(get(&e, "after_ms").as_deref(), Some("2000"));

        let failed = SegmentFailureReport {
            cause: Cause::Http(404),
            deadline_ms: None,
            recovery: SegmentRecovery::Fail {
                attempt: 1,
                why: GiveUp::NotRetryable,
            },
            ..r.clone()
        };
        assert!(failed.fatal());
        assert!(failed
            .line()
            .contains("cause=http status=404 deadline_ms=none rung=720p action=fail attempt=1 after_ms=none give_up=not_retryable"));
        assert_eq!(failed.event(String::new()).level, ClientLevel::Error);

        let stepped = SegmentFailureReport {
            cause: Cause::TimeoutLoad,
            deadline_ms: Some(60_000),
            recovery: SegmentRecovery::StepDown,
            ..r
        };
        assert!(!stepped.fatal());
        assert!(stepped.line().contains(
            "cause=timeout_load status=none deadline_ms=60000 rung=720p action=step_down attempt=0"
        ));
    }

    #[test]
    fn retry_verdicts_map_onto_recoveries() {
        use pharos_hls::retry::{GiveUp, Verdict};
        assert_eq!(
            SegmentRecovery::from(Verdict::Retry {
                attempt: 3,
                after_ms: 4_000
            }),
            SegmentRecovery::Retry {
                attempt: 3,
                after_ms: 4_000
            },
        );
        assert_eq!(
            SegmentRecovery::from(Verdict::Fail {
                attempt: 7,
                why: GiveUp::Exhausted
            }),
            SegmentRecovery::Fail {
                attempt: 7,
                why: GiveUp::Exhausted
            },
        );
    }

    #[test]
    fn weak_devices_wait_longer_for_a_segment() {
        let desktop = segment_deadlines(DeviceClass::Desktop);
        for weak in [DeviceClass::Tv, DeviceClass::Phone] {
            let d = segment_deadlines(weak);
            assert!(d.first_byte_ms > desktop.first_byte_ms);
            assert!(d.load_ms > desktop.load_ms);
        }
        assert_eq!(desktop.first_byte_ms, 10_000);
    }

    #[test]
    fn only_mse_engines_report_fragments() {
        assert!(Engine::HlsJs.reports_fragments());
        assert!(Engine::RustHls.reports_fragments());
        assert!(!Engine::NativeHls.reports_fragments());
        assert!(!Engine::Direct.reports_fragments());
    }

    /// V630: one verdict per play, naming what plays, what was asked and why.
    #[test]
    fn the_rust_engine_hands_over_to_hlsjs_until_built_and_says_so() {
        let v = engine_verdict_with(Engine::RustHls, EngineReason::Override, false);
        assert_eq!(v.engine, Engine::HlsJs);
        assert_eq!(v.fallback, Some("not_built"));
        assert_eq!(
            v.line(),
            "pharos: playback engine=hlsjs reason=fallback cause=not_built asked=rust asked_by=override"
        );
        let event = v.report();
        assert_eq!(event.kind, ClientEventKind::RendererMode);
        for field in [
            ("renderer", "hls"),
            ("mode", "hlsjs"),
            ("asked", "rust"),
            ("asked_by", "override"),
            ("reason", "fallback"),
            ("cause", "not_built"),
        ] {
            assert!(
                event.fields.contains(&(field.0, field.1.to_owned())),
                "{field:?}"
            );
        }

        let built = engine_verdict(Engine::RustHls, EngineReason::Override);
        assert_eq!((built.engine, built.fallback), (Engine::RustHls, None));
        assert_eq!(built.line(), "pharos: playback engine=rust reason=override");
    }

    #[test]
    fn hls_fallback_cause_tokens_are_distinct() {
        let labels: std::collections::BTreeSet<_> = HlsFallbackCause::ALL
            .into_iter()
            .map(HlsFallbackCause::label)
            .collect();
        assert_eq!(labels.len(), HlsFallbackCause::ALL.len());
        assert_eq!(
            HlsFallbackCause::ALL.map(HlsFallbackCause::label),
            ["unsupported_tag", "not_fmp4", "live", "codec", "no_variant"]
        );
    }

    #[test]
    fn split_codecs_separates_video_and_audio_prefixes() {
        let s = |items: &[&str]| items.iter().map(|c| (*c).to_owned()).collect::<Vec<_>>();
        assert_eq!(
            split_codecs(&s(&["avc1.640028", "opus"])),
            (s(&["avc1.640028"]), s(&["opus"]))
        );
        assert_eq!(
            split_codecs(&s(&["vp09.00.40.08", "opus"])),
            (s(&["vp09.00.40.08"]), s(&["opus"]))
        );
        assert_eq!(
            split_codecs(&s(&[
                "avc3.64001f",
                "hvc1.1.6.L120.90",
                "hev1.1.6.L120.90",
                "av01.0.08M.08",
                "dvh1.08.06",
                "mp4a.40.2",
                "flac",
                "ac-3",
                "ec-3"
            ])),
            (
                s(&[
                    "avc3.64001f",
                    "hvc1.1.6.L120.90",
                    "hev1.1.6.L120.90",
                    "av01.0.08M.08",
                    "dvh1.08.06"
                ]),
                s(&["mp4a.40.2", "flac", "ac-3", "ec-3"])
            )
        );
    }

    #[test]
    fn plan_hls_covers_demuxed_muxed_and_every_fallback_cause() {
        use pharos_hls::playlist::{parse_master, parse_media};

        let fmp4_media = parse_media(
            "#EXTM3U\n\
             #EXT-X-VERSION:7\n\
             #EXT-X-PLAYLIST-TYPE:VOD\n\
             #EXT-X-TARGETDURATION:6\n\
             #EXT-X-MAP:URI=\"init.mp4\"\n\
             #EXTINF:6.000,\n\
             0.m4s\n\
             #EXT-X-ENDLIST\n",
            "http://pharos.local/videos/42/main.m3u8",
        )
        .unwrap();

        let avc_master = parse_master(
            "#EXTM3U\n\
             #EXT-X-VERSION:7\n\
             #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",NAME=\"Audio\",DEFAULT=YES,URI=\"audio.m3u8\"\n\
             #EXT-X-STREAM-INF:BANDWIDTH=8000000,CODECS=\"avc1.640028,opus\",AUDIO=\"aud\"\n\
             h264/main.m3u8\n",
            "http://pharos.local/videos/42/master.m3u8",
        )
        .unwrap();
        assert_eq!(
            plan_hls(&avc_master, &fmp4_media, Some(&fmp4_media), |_| true),
            Ok(Plan {
                video_uri: "http://pharos.local/videos/42/h264/main.m3u8".to_owned(),
                audio_uri: Some("http://pharos.local/videos/42/audio.m3u8".to_owned()),
                video_mime: r#"video/mp4; codecs="avc1.640028""#.to_owned(),
                audio_mime: Some(r#"audio/mp4; codecs="opus""#.to_owned()),
            })
        );

        let vp9_master = parse_master(
            "#EXTM3U\n\
             #EXT-X-VERSION:7\n\
             #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",NAME=\"Audio\",DEFAULT=NO,URI=\"audio.m3u8\"\n\
             #EXT-X-STREAM-INF:BANDWIDTH=6000000,CODECS=\"vp09.00.40.08,opus\",AUDIO=\"aud\"\n\
             vp9/main.m3u8\n",
            "http://pharos.local/videos/42/master.m3u8",
        )
        .unwrap();
        assert_eq!(
            plan_hls(&vp9_master, &fmp4_media, Some(&fmp4_media), |_| true),
            Ok(Plan {
                video_uri: "http://pharos.local/videos/42/vp9/main.m3u8".to_owned(),
                audio_uri: Some("http://pharos.local/videos/42/audio.m3u8".to_owned()),
                video_mime: r#"video/mp4; codecs="vp09.00.40.08""#.to_owned(),
                audio_mime: Some(r#"audio/mp4; codecs="opus""#.to_owned()),
            })
        );

        let muxed_master = parse_master(
            "#EXTM3U\n\
             #EXT-X-VERSION:7\n\
             #EXT-X-STREAM-INF:BANDWIDTH=4000000,CODECS=\"avc1.640028,mp4a.40.2\"\n\
             muxed/main.m3u8\n",
            "http://pharos.local/videos/42/master.m3u8",
        )
        .unwrap();
        assert_eq!(
            plan_hls(&muxed_master, &fmp4_media, None, |_| true),
            Ok(Plan {
                video_uri: "http://pharos.local/videos/42/muxed/main.m3u8".to_owned(),
                audio_uri: None,
                video_mime: r#"video/mp4; codecs="avc1.640028,mp4a.40.2""#.to_owned(),
                audio_mime: None,
            })
        );

        let ts_audio_master = parse_master(
            "#EXTM3U\n\
             #EXT-X-VERSION:3\n\
             #EXT-X-STREAM-INF:BANDWIDTH=128000,CODECS=\"mp4a.40.2\"\n\
             audio.m3u8\n",
            "http://pharos.local/Videos/42/master.m3u8",
        )
        .unwrap();
        let ts_media = parse_media(
            "#EXTM3U\n\
             #EXT-X-TARGETDURATION:6\n\
             #EXTINF:6.000,\n\
             0.ts\n\
             #EXT-X-ENDLIST\n",
            "http://pharos.local/Videos/42/audio.m3u8",
        )
        .unwrap();
        assert_eq!(
            plan_hls(&ts_audio_master, &ts_media, None, |_| true).map_err(|(c, _)| c),
            Err(HlsFallbackCause::NotFmp4)
        );

        let live_media = parse_media(
            "#EXTM3U\n\
             #EXT-X-TARGETDURATION:6\n\
             #EXT-X-MAP:URI=\"init.mp4\"\n\
             #EXTINF:6.000,\n\
             0.m4s\n",
            "http://pharos.local/videos/42/live.m3u8",
        )
        .unwrap();
        assert_eq!(
            plan_hls(&avc_master, &live_media, Some(&fmp4_media), |_| true),
            Err((HlsFallbackCause::Live, "live".to_owned()))
        );

        assert_eq!(
            plan_hls(&avc_master, &fmp4_media, Some(&fmp4_media), |m| !m
                .contains("opus")),
            Err((
                HlsFallbackCause::Codec,
                r#"audio/mp4; codecs="opus""#.to_owned()
            ))
        );
        assert_eq!(
            plan_hls(&avc_master, &fmp4_media, Some(&fmp4_media), |m| !m
                .contains("avc1")),
            Err((
                HlsFallbackCause::Codec,
                r#"video/mp4; codecs="avc1.640028""#.to_owned()
            ))
        );

        let encrypted_media = parse_media(
            "#EXTM3U\n\
             #EXT-X-TARGETDURATION:6\n\
             #EXT-X-MAP:URI=\"init.mp4\"\n\
             #EXT-X-KEY:METHOD=AES-128,URI=\"key\"\n\
             #EXTINF:6.000,\n\
             0.m4s\n\
             #EXT-X-ENDLIST\n",
            "http://pharos.local/videos/42/enc.m3u8",
        )
        .unwrap();
        assert_eq!(
            plan_hls(&avc_master, &encrypted_media, None, |_| true),
            Err((HlsFallbackCause::UnsupportedTag, "encrypted".to_owned()))
        );

        let empty_master = parse_master("#EXTM3U\n", "http://pharos.local/master.m3u8").unwrap();
        assert_eq!(
            plan_hls(&empty_master, &fmp4_media, None, |_| true),
            Err((HlsFallbackCause::NoVariant, "none".to_owned()))
        );
    }

    #[test]
    fn ranges_to_ms_converts_seconds_and_sorts() {
        assert_eq!(
            ranges_to_ms(&[(12.012, 18.018), (0.0, 6.006), (-1.0, 0.0), (5.0, 5.0)]),
            vec![(0, 6_006), (12_012, 18_018)]
        );
    }

    #[test]
    fn hlsjs_plays_as_picked() {
        let v = engine_verdict(Engine::HlsJs, EngineReason::Default);
        assert_eq!((v.engine, v.fallback), (Engine::HlsJs, None));
        assert_eq!(v.line(), "pharos: playback engine=hlsjs reason=default");
        assert!(v
            .report()
            .fields
            .contains(&("reason", "default".to_owned())));
    }

    /// A browser without WebM still offers VP9: codecs are asked about in
    /// MP4, containers separately.
    #[test]
    fn capabilities_list_what_the_browser_said_yes_to() {
        let caps = capabilities(
            |mime| mime.contains("vp09") || mime.contains("opus") || mime == "audio/mpeg",
            |mime| mime == "video/mp4",
        );
        assert_eq!(caps.video_codecs, ["vp9"]);
        assert_eq!(caps.audio_codecs, ["opus", "mp3"]);
        assert_eq!(caps.containers, ["mp4", "m4v"]);
        assert_eq!(caps.max_bitrate, 0);
    }

    #[test]
    fn play_pause_and_seek_report_at_once() {
        let mut clock = ReportClock::new();
        assert_eq!(
            clock.on(0.0, MediaEvent::Playing),
            Some(Outgoing::Report { paused: false })
        );
        assert_eq!(
            clock.on(500.0, MediaEvent::Pause),
            Some(Outgoing::Report { paused: true })
        );
        assert_eq!(
            clock.on(600.0, MediaEvent::Seeked),
            Some(Outgoing::Report { paused: true })
        );
    }

    #[test]
    fn time_updates_report_every_interval_while_playing() {
        let mut clock = ReportClock::new();
        clock.on(0.0, MediaEvent::Playing);
        assert_eq!(clock.on(4_000.0, MediaEvent::TimeUpdate), None);
        assert_eq!(clock.on(9_999.0, MediaEvent::TimeUpdate), None);
        assert_eq!(
            clock.on(10_000.0, MediaEvent::TimeUpdate),
            Some(Outgoing::Report { paused: false })
        );
        assert_eq!(clock.on(15_000.0, MediaEvent::TimeUpdate), None);
        clock.on(16_000.0, MediaEvent::Pause);
        assert_eq!(clock.on(40_000.0, MediaEvent::TimeUpdate), None);
    }

    /// A time update before any play event still reports, so a stream that
    /// starts without `playing` firing is not silent.
    #[test]
    fn the_first_time_update_reports() {
        let mut clock = ReportClock::new();
        assert_eq!(
            clock.on(100.0, MediaEvent::TimeUpdate),
            Some(Outgoing::Report { paused: false })
        );
    }

    #[test]
    fn stop_is_sent_once_and_silences_the_clock() {
        let mut clock = ReportClock::new();
        clock.on(0.0, MediaEvent::Playing);
        assert_eq!(clock.on(1.0, MediaEvent::Ended), Some(Outgoing::Stop));
        assert!(!clock.stop());
        assert_eq!(clock.on(2.0, MediaEvent::Playing), None);

        let mut clock = ReportClock::new();
        assert!(clock.stop());
        assert_eq!(clock.on(1.0, MediaEvent::Ended), None);
    }

    /// B809: a page that goes hidden reports where it is, as a report and
    /// with the pause state, and keeps reporting afterwards.
    #[test]
    fn hiding_the_page_reports_without_stopping() {
        let mut clock = ReportClock::new();
        clock.on(0.0, MediaEvent::Playing);
        assert_eq!(
            clock.on_hidden(3_000.0),
            Some(Outgoing::Report { paused: false })
        );
        // The interval restarts from the hidden report.
        assert_eq!(clock.on(12_999.0, MediaEvent::TimeUpdate), None);
        assert_eq!(
            clock.on(13_000.0, MediaEvent::TimeUpdate),
            Some(Outgoing::Report { paused: false })
        );
        clock.on(14_000.0, MediaEvent::Pause);
        assert_eq!(
            clock.on_hidden(15_000.0),
            Some(Outgoing::Report { paused: true })
        );
        assert!(clock.stop(), "the stop is still owed");
    }

    #[test]
    fn a_stopped_clock_does_not_report_on_hide_until_the_page_is_restored() {
        let mut clock = ReportClock::new();
        clock.on(0.0, MediaEvent::Playing);
        assert!(clock.stop());
        assert_eq!(clock.on_hidden(1.0), None);
        assert_eq!(clock.on(2.0, MediaEvent::TimeUpdate), None);
        assert!(clock.restore(), "a stop had been sent");
        assert_eq!(
            clock.on(3.0, MediaEvent::TimeUpdate),
            Some(Outgoing::Report { paused: false }),
            "reporting resumes at once"
        );
        assert!(!clock.restore(), "nothing to restore on a live clock");
    }

    #[test]
    fn fatal_errors_recover_a_few_times_then_fail() {
        let mut budget = RecoveryBudget::new();
        for _ in 0..MAX_RECOVERIES {
            assert_eq!(
                budget.on_fatal("networkError", "fragLoadError"),
                Recovery::StartLoad
            );
        }
        assert_eq!(
            budget.on_fatal("networkError", "fragLoadError"),
            Recovery::Fail
        );
        assert_eq!(budget.attempts("networkError"), MAX_RECOVERIES);
        assert_eq!(
            budget.on_fatal("mediaError", "bufferStalledError"),
            Recovery::RecoverMedia,
            "each kind has its own budget"
        );
    }

    // V638/V1062: one fault costs one rebuild, and only playback well past
    // the rebuild point refills the budget, so a segment that never decodes
    // gives up after MAX_RECOVERIES instead of rebuilding forever.
    #[test]
    fn a_media_error_rebuilds_once_per_mediasource_and_refills_only_on_playback() {
        let mut r = MediaRecovery::new();
        assert_eq!(
            r.on_error(0, "bufferAppendError", 60_000, 1_000),
            MediaVerdict::Rebuild { attempt: 1 }
        );
        assert_eq!(
            r.on_error(0, "videoError", 60_000, 1_010),
            MediaVerdict::Stale,
            "the same fault's second error"
        );
        assert_eq!(r.epoch(), 1);
        assert_eq!(r.on_play_head(60_000, 1_500), None, "not moving yet");
        assert_eq!(
            r.on_play_head(60_250, 2_000),
            None,
            "moving again is not recovered: the bad segment may still be ahead"
        );

        // The same segment fails again before playback gets past it.
        for attempt in 2..=MAX_RECOVERIES {
            let epoch = r.epoch();
            assert_eq!(
                r.on_error(epoch, "bufferAppendError", 60_500, 3_000),
                MediaVerdict::Rebuild { attempt }
            );
        }
        let epoch = r.epoch();
        assert_eq!(
            r.on_error(epoch, "bufferAppendError", 60_500, 4_000),
            MediaVerdict::Fail {
                attempt: MAX_RECOVERIES
            }
        );
    }

    #[test]
    fn playback_well_past_a_rebuild_refills_the_media_budget() {
        let mut r = MediaRecovery::new();
        for _ in 0..MAX_RECOVERIES {
            let epoch = r.epoch();
            r.on_error(epoch, "bufferStalledError", 30_000, 0);
        }
        assert_eq!(r.on_play_head(30_000 + MEDIA_REFILL_MS - 1, 100), None);
        let epoch = r.epoch();
        assert!(matches!(
            r.on_error(epoch, "bufferStalledError", 30_000, 200),
            MediaVerdict::Fail { .. }
        ));
        let mut r = MediaRecovery::new();
        for _ in 0..MAX_RECOVERIES {
            let epoch = r.epoch();
            r.on_error(epoch, "bufferStalledError", 30_000, 0);
        }
        assert_eq!(
            r.on_play_head(30_000 + MEDIA_REFILL_MS, 100),
            Some(MediaResumed {
                attempt: MAX_RECOVERIES,
                at_ms: 30_000 + MEDIA_REFILL_MS,
                took_ms: 100,
            }),
            "the recovery is reported where the budget refills"
        );
        assert_eq!(
            r.on_play_head(30_000 + MEDIA_REFILL_MS + 500, 600),
            None,
            "once"
        );
        let epoch = r.epoch();
        assert_eq!(
            r.on_error(epoch, "bufferStalledError", 45_000, 200),
            MediaVerdict::Rebuild { attempt: 1 }
        );
    }

    // V640: the rebuild, the give-up and the resume each log and report
    // the same fields, with the attempt and the play head.
    #[test]
    fn media_recovery_is_logged_and_reported_on_both_sides() {
        let rebuild = MediaRecoveryReport::Error {
            details: "bufferAppendError".to_owned(),
            message: "video SourceBuffer error".to_owned(),
            verdict: MediaVerdict::Rebuild { attempt: 1 },
            at_ms: 42_000,
        };
        assert_eq!(
            rebuild.line("/m.m3u8"),
            "pharos: playback error engine=rust type=mediaError details=bufferAppendError message=video SourceBuffer error fatal=true action=recover_media attempt=1 at_ms=42000 url=/m.m3u8"
        );
        let e = rebuild.event("/ui/watch/1".to_owned());
        assert_eq!(e.kind, ClientEventKind::PlaybackError);
        assert_eq!(e.level, ClientLevel::Error);
        let keys: Vec<_> = e.fields.iter().map(|(k, _)| *k).collect();
        assert_eq!(
            keys,
            ["engine", "type", "details", "message", "action", "attempt", "at_ms"]
        );
        let fail = MediaRecoveryReport::Error {
            details: "bufferAppendError".to_owned(),
            message: "video SourceBuffer error".to_owned(),
            verdict: MediaVerdict::Fail { attempt: 3 },
            at_ms: 42_000,
        };
        assert!(fail.line("u").contains("action=fail attempt=3 at_ms=42000"));
        let resumed = MediaRecoveryReport::Resumed(MediaResumed {
            attempt: 1,
            at_ms: 42_300,
            took_ms: 1_800,
        });
        assert_eq!(
            resumed.line("u"),
            "pharos: playback recovered engine=rust type=mediaError action=recover_media attempt=1 at_ms=42300 took_ms=1800 url=u"
        );
        let e = resumed.event(String::new());
        assert_eq!(e.level, ClientLevel::Info);
        assert!(e.fields.contains(&("action", "recover_media".to_owned())));
    }

    #[test]
    fn a_refused_codec_fails_without_a_rebuild() {
        let mut r = MediaRecovery::new();
        assert_eq!(
            r.on_error(0, "bufferAddCodecError", 0, 0),
            MediaVerdict::Fail { attempt: 0 }
        );
        assert_eq!(r.epoch(), 0);
        assert_eq!(
            MediaVerdict::Rebuild { attempt: 1 }.label(),
            "recover_media"
        );
        assert_eq!(MediaVerdict::Fail { attempt: 1 }.label(), "fail");
        assert_ne!(MediaVerdict::Stale.label(), "fail");
    }

    #[test]
    fn a_loaded_fragment_refills_the_budget() {
        let mut budget = RecoveryBudget::new();
        for _ in 0..MAX_RECOVERIES {
            budget.on_fatal("mediaError", "bufferAppendError");
        }
        budget.progressed();
        assert_eq!(
            budget.on_fatal("mediaError", "bufferAppendError"),
            Recovery::RecoverMedia
        );
    }

    #[test]
    fn a_failed_manifest_and_other_errors_fail_at_once() {
        let mut budget = RecoveryBudget::new();
        assert_eq!(
            budget.on_fatal("networkError", "manifestLoadError"),
            Recovery::Fail
        );
        assert_eq!(
            budget.on_fatal("keySystemError", "keySystemNoKeys"),
            Recovery::Fail
        );
        assert_eq!(
            budget.on_fatal("otherError", "internalException"),
            Recovery::Fail
        );
    }

    #[test]
    fn recovery_labels_are_distinct() {
        let labels =
            [Recovery::StartLoad, Recovery::RecoverMedia, Recovery::Fail].map(Recovery::label);
        assert_eq!(labels, ["start_load", "recover_media", "fail"]);
    }

    #[test]
    fn play_starts_at_the_stored_position_unless_finished() {
        assert_eq!(start_ms(90_000, false), 90_000);
        assert_eq!(start_ms(90_000, true), 0);
        assert_eq!(start_ms(0, false), 0);
    }

    #[test]
    fn a_start_from_zero_is_an_explicit_zero_not_the_playlist_start() {
        assert_eq!(hls_start_position(0), 0.0);
        assert_eq!(hls_start_position(90_500), 90.5);
    }

    /// Feeds `(dropped, presented)` totals once a second from t=0 and returns
    /// every drop with the second it fired at.
    fn run(start: Tier, totals: &[(u64, u64)]) -> Vec<(u64, TierDropEvent)> {
        let mut td = TierDrop::new(start);
        let mut out = Vec::new();
        for (i, (d, p)) in totals.iter().enumerate() {
            let sec = i as u64;
            if let Some(e) = td.sample(sec * 1000, *d, *p, true) {
                out.push((sec, e));
            }
        }
        out
    }

    /// `secs` samples of a stream at 60 fps losing `loss` of every 100 frames.
    fn stream(secs: u64, loss: u64) -> Vec<(u64, u64)> {
        (0..=secs).map(|s| (s * 60 * loss / 100, s * 60)).collect()
    }

    #[test]
    fn a_steady_good_stream_never_drops() {
        assert!(run(Tier::Desktop, &stream(120, 1)).is_empty());
    }

    #[test]
    fn a_sustained_bad_stream_drops_once_per_cooldown() {
        let drops = run(Tier::Desktop, &stream(25, 20));
        assert_eq!(drops.len(), 1);
        let (sec, e) = drops[0];
        assert_eq!(sec, 10);
        assert_eq!(
            e,
            TierDropEvent {
                from: Tier::Desktop,
                to: Tier::Tv,
                dropped: 120,
                presented: 600,
                window_ms: 10_000,
                renderer: None,
            }
        );
    }

    #[test]
    fn the_cooldown_spaces_drops_thirty_seconds_apart() {
        let drops = run(Tier::Desktop, &stream(80, 20));
        let at: Vec<_> = drops.iter().map(|(s, e)| (*s, e.to)).collect();
        assert_eq!(at, [(10, Tier::Tv), (40, Tier::Low)]);
    }

    #[test]
    fn low_is_the_floor() {
        assert!(run(Tier::Low, &stream(120, 50)).is_empty());
        let mut td = TierDrop::new(Tier::Desktop);
        for (i, (d, p)) in stream(200, 50).into_iter().enumerate() {
            td.sample(i as u64 * 1000, d, p, true);
        }
        assert_eq!(td.tier(), Tier::Low);
    }

    #[test]
    fn a_partial_window_does_nothing() {
        assert!(run(Tier::Desktop, &stream(9, 90)).is_empty());
    }

    #[test]
    fn a_window_with_too_few_frames_cannot_drop_the_tier() {
        // 3 of 55 frames over 10 s is over budget, but too few frames to judge.
        let thin: Vec<_> = (0..=10).map(|s| (s * 3 / 10, s * 55 / 10)).collect();
        assert!(run(Tier::Desktop, &thin).is_empty());
        let full: Vec<_> = (0..=10).map(|s| (s * 30 / 10, s * 550 / 10)).collect();
        assert!(!run(Tier::Desktop, &full).is_empty());
    }

    #[test]
    fn a_window_needs_exactly_the_minimum_frames_to_drop_the_tier() {
        let window = |dropped: u64, presented: u64| -> Vec<(u64, u64)> {
            (0..=10)
                .map(|s| (dropped * s / 10, presented * s / 10))
                .collect()
        };
        assert!(run(Tier::Desktop, &window(10, 119)).is_empty());
        assert_eq!(run(Tier::Desktop, &window(10, 120)).len(), 1);
        assert!(run(Tier::Desktop, &window(119, 0)).is_empty());
        assert_eq!(run(Tier::Desktop, &window(120, 0)).len(), 1);
    }

    #[test]
    fn exactly_five_percent_is_within_budget() {
        let totals: Vec<_> = (0..=30).map(|s| (s * 3, s * 60)).collect();
        assert!(run(Tier::Desktop, &totals).is_empty());
        let totals: Vec<_> = (0..=30).map(|s| (s * 3 + s / 10, s * 60)).collect();
        assert!(!run(Tier::Desktop, &totals).is_empty());
    }

    #[test]
    fn counters_going_backwards_restart_the_window() {
        let mut totals = stream(8, 50);
        totals.extend(stream(8, 50));
        assert!(run(Tier::Desktop, &totals).is_empty());
    }

    #[test]
    fn a_recovered_stream_stops_dropping() {
        let mut totals = stream(10, 20);
        let (d0, p0) = totals[10];
        totals.extend((1..=60).map(|s| (d0, p0 + s * 60)));
        let drops = run(Tier::Desktop, &totals);
        assert_eq!(drops.len(), 1);
    }

    #[test]
    fn samples_while_not_counting_restart_the_window() {
        let sample_at = |hidden_at: Option<u64>| {
            let mut td = TierDrop::new(Tier::Desktop);
            let mut fired = Vec::new();
            for (sec, (d, p)) in stream(14, 50).into_iter().enumerate() {
                let sec = sec as u64;
                if td
                    .sample(sec * 1000, d, p, Some(sec) != hidden_at)
                    .is_some()
                {
                    fired.push(sec);
                }
            }
            fired
        };
        assert_eq!(sample_at(None), [10]);
        // Hidden at 5 s: the window restarts, so the first full one ends at 15 s.
        assert!(sample_at(Some(5)).is_empty());
    }

    #[test]
    fn dropped_frames_with_none_presented_are_over_budget() {
        let totals: Vec<_> = (0..=10).map(|s| (s * 24, 0)).collect();
        assert_eq!(run(Tier::Desktop, &totals).len(), 1);
        let totals: Vec<_> = (0..=10).map(|_| (0, 0)).collect();
        assert!(run(Tier::Desktop, &totals).is_empty());
    }

    /// `secs` samples of a renderer drawing 60 frames a second and skipping
    /// `skip` of every 100.
    fn renderer(secs: u64, skip: u64) -> Vec<RendererFrames> {
        (0..=secs)
            .map(|s| RendererFrames {
                skipped: s * 60 * skip / 100,
                drawn: s * 60 * (100 - skip) / 100,
            })
            .collect()
    }

    fn judge_all(video: &[(u64, u64)], frames: &[RendererFrames]) -> Vec<(u64, TierVerdict)> {
        let mut td = TierDrop::new(Tier::Desktop);
        video
            .iter()
            .zip(frames)
            .enumerate()
            .map(|(sec, ((d, p), r))| {
                let sec = sec as u64;
                (sec, td.judge(sec * 1000, *d, *p, Some(*r), true))
            })
            .filter(|(_, v)| *v != TierVerdict::Hold)
            .collect()
    }

    /// B667: the counters `StyledSubtitles::frames` reads exist in the
    /// vendored JASSUB, and its per-frame console callback is the one the
    /// player silences. A JASSUB upgrade that renames them fails here.
    #[test]
    fn vendored_jassub_has_the_debug_counters_read() {
        let js = include_str!("../../assets/vendor/jassub/jassub.js");
        assert!(js.contains("droppedFrames=0;presentedFrames=0"));
        assert!(js.contains("this.onsubtitleFrameCallback?.("));
    }

    /// B667: a renderer whose counters did not move is no evidence it kept up.
    #[test]
    fn a_renderer_that_counted_nothing_cannot_spare_the_tier() {
        let idle: Vec<_> = (0..=45)
            .map(|_| RendererFrames {
                skipped: 0,
                drawn: 0,
            })
            .collect();
        let verdicts = judge_all(&stream(45, 9), &idle);
        assert!(matches!(verdicts.first(), Some((10, TierVerdict::Drop(_)))));
    }

    /// B667: a video dropping 9% of its frames while the subtitle renderer
    /// keeps up is the decoder's cost, not the effects': the tier stays, and
    /// the spared window is reported once per cooldown.
    #[test]
    fn video_drops_the_renderer_kept_up_with_spare_the_tier() {
        let verdicts = judge_all(&stream(45, 9), &renderer(45, 0));
        let at: Vec<_> = verdicts
            .iter()
            .map(|(s, v)| (*s, matches!(v, TierVerdict::Spared(_))))
            .collect();
        assert_eq!(at, [(10, true), (40, true)]);
        let TierVerdict::Spared(e) = verdicts[0].1 else {
            unreachable!()
        };
        assert_eq!(
            e.renderer,
            Some(RendererFrames {
                skipped: 0,
                drawn: 600
            })
        );
        let line = tier_spared_line(&e);
        assert!(
            line.contains("tier=desktop reason=renderer_kept_up"),
            "{line}"
        );
        assert!(
            line.contains("renderer_skipped=0 renderer_drawn=600"),
            "{line}"
        );
    }

    #[test]
    fn video_drops_with_a_busy_renderer_still_drop() {
        let verdicts = judge_all(&stream(25, 9), &renderer(25, 10));
        assert_eq!(verdicts.len(), 1);
        let (sec, TierVerdict::Drop(e)) = verdicts[0] else {
            unreachable!("{verdicts:?}")
        };
        assert_eq!((sec, e.to), (10, Tier::Tv));
        assert!(tier_drop_line(&e).contains("renderer_skipped=60 renderer_drawn=540"));
    }

    #[test]
    fn a_busy_renderer_alone_never_drops() {
        assert!(judge_all(&stream(45, 1), &renderer(45, 50)).is_empty());
    }

    /// B666: one over-budget window no longer caps a device for two weeks.
    #[test]
    fn a_remembered_cap_lasts_a_day() {
        let set = 1_000_000_000_000;
        let cap = format_tier_drop(Tier::Tv, set);
        let hour = 60 * 60 * 1000;
        assert_eq!(parse_tier_drop(&cap, set + 23 * hour), Some(Tier::Tv));
        assert_eq!(parse_tier_drop(&cap, set + 25 * hour), None);
        // Stamped by a clock that ran ahead: not a cap until 2031.
        assert_eq!(parse_tier_drop(&cap, set - 365 * 24 * hour), None);
    }

    #[test]
    fn a_stored_tier_reads_back_and_unknown_is_none() {
        assert_eq!(TIER_DROP_KEY, "pharos.subtitles.tier_drop.v3");
        for tier in Tier::ALL {
            assert_eq!(Tier::parse(tier.label()), Some(tier));
        }
        assert_eq!(Tier::parse(" tv \n"), Some(Tier::Tv));
        for unknown in ["", "ultra", "TV", "none"] {
            assert_eq!(Tier::parse(unknown), None, "{unknown:?}");
        }
    }

    fn drop_event() -> TierDropEvent {
        TierDropEvent {
            from: Tier::Desktop,
            to: Tier::Tv,
            dropped: 120,
            presented: 600,
            window_ms: 10_000,
            renderer: None,
        }
    }

    #[test]
    fn the_tier_drop_log_line_names_the_measurement() {
        assert_eq!(
            tier_drop_line(&drop_event()),
            "pharos: subtitle tier drop from=desktop to=tv reason=dropped_frames dropped=120 presented=600 window_ms=10000 renderer=uncounted"
        );
    }

    #[test]
    fn the_tier_drop_report_is_a_renderer_mode_event_with_the_measurement() {
        let event = tier_drop_report("jassub", &drop_event());
        assert_eq!(event.kind, ClientEventKind::RendererMode);
        assert_eq!(event.level, ClientLevel::Info);
        assert_eq!(
            event.fields,
            [
                ("renderer", "jassub".to_owned()),
                ("from", "desktop".to_owned()),
                ("to", "tv".to_owned()),
                ("reason", "dropped_frames".to_owned()),
                ("dropped", "120".to_owned()),
                ("presented", "600".to_owned()),
                ("window_ms", "10000".to_owned()),
            ]
        );
    }

    #[test]
    fn a_stored_cap_lasts_fourteen_days_and_a_bare_tier_still_counts() {
        let set = 1_000_000_000_000;
        assert_eq!(format_tier_drop(Tier::Tv, set), "tv@1000000000000");
        let at = |age| parse_tier_drop("tv@1000000000000", set + age);
        assert_eq!(at(0), Some(Tier::Tv));
        assert_eq!(at(TIER_DROP_TTL_MS), Some(Tier::Tv));
        assert_eq!(at(TIER_DROP_TTL_MS + 1), None);
        assert_eq!(parse_tier_drop("tv@1000000000000", set - 5), None);
        assert_eq!(parse_tier_drop("low", set), Some(Tier::Low));
        for bad in ["tv@", "tv@soon", "ultra@1000000000000", "@5", ""] {
            assert_eq!(parse_tier_drop(bad, set), None, "{bad:?}");
        }
    }
}
