//! The background worker: decodes the playing song just ahead of the
//! playhead, only while the visualiser is shown and the song plays, and
//! hands out the frame under the playhead.

use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use symphonia::core::io::MediaSource;

use crate::decode::{Decoder, OpenError};
use crate::frame::{Frame, FPS};
use crate::pipeline::{lock, Pipeline, SharedRing};
use crate::ring::{playhead_frame, AHEAD_FRAMES};

/// A report that differs from where the clock put the playhead by more than
/// this is a seek, not drift.
const JUMP_MS: u64 = 400;
/// The worker never sleeps less than this between refills.
const MIN_NAP: Duration = Duration::from_millis(100);
/// How often a finished song looks for a seek back.
const ENDED_NAP: Duration = Duration::from_secs(1);
/// How long after a failed open or decode the same track is tried again: a
/// dropped connection should not blank the visualiser for the whole song, and
/// a source that stays down is asked about this rarely.
const SOURCE_RETRY: Duration = Duration::from_secs(5);

/// A song's bytes and a hint at its container.
pub struct Opened {
    pub source: Box<dyn MediaSource>,
    pub extension: Option<String>,
}

/// Finds a track's bytes: the cached file if whole, else a ranged read of
/// its stream URL. Called on the worker thread, so it may block.
pub trait Opener: Send + Sync + 'static {
    fn open(&self, track_id: &str) -> Result<Opened, String>;
}

/// What the visualiser's source is doing and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    pub verdict: &'static str,
    pub codec: String,
    pub reason: String,
}

impl Status {
    fn new(verdict: &'static str, codec: &str, reason: impl Into<String>) -> Self {
        Self {
            verdict,
            codec: codec.to_owned(),
            reason: reason.into(),
        }
    }

    /// The log line: `verdict` is `on`, `off` or `unsupported`.
    pub fn line(&self) -> String {
        format!(
            "visualiser source=decode verdict={} codec={} reason={}",
            self.verdict, self.codec, self.reason
        )
    }
}

/// The frame under the playhead and the rate its song plays at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pulled {
    pub sample_rate: u32,
    pub frame: Frame,
}

struct State {
    enabled: bool,
    playing: bool,
    track: Option<String>,
    /// The player's last position and when it was reported.
    anchor: Option<(u64, Instant)>,
    sample_rate: u32,
    status: Status,
    shutdown: bool,
    /// Bumped by every change the worker must act on, so one that lands
    /// while it is busy is not slept through.
    epoch: u64,
}

struct Shared {
    state: Mutex<State>,
    wake: Condvar,
    ring: SharedRing,
}

fn locked(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

pub struct Engine {
    shared: Arc<Shared>,
}

impl Drop for Engine {
    fn drop(&mut self) {
        locked(&self.shared.state).shutdown = true;
        self.shared.wake.notify_all();
    }
}

impl Engine {
    pub fn new(opener: Arc<dyn Opener>) -> Self {
        Self::with_retry(opener, SOURCE_RETRY)
    }

    /// [`Engine::new`] with the wait before a failed open or decode is tried
    /// again; an unsupported codec is never retried.
    pub fn with_retry(opener: Arc<dyn Opener>, retry: Duration) -> Self {
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                enabled: false,
                playing: false,
                track: None,
                anchor: None,
                sample_rate: 0,
                status: Status::new("off", "none", "hidden"),
                shutdown: false,
                epoch: 0,
            }),
            wake: Condvar::new(),
            ring: Arc::default(),
        });
        let worker = shared.clone();
        if let Err(e) = std::thread::Builder::new()
            .name("pharos-vis".into())
            .spawn(move || run(&worker, opener.as_ref(), retry))
        {
            log::error!("visualiser source=decode verdict=off codec=none reason=thread:{e}");
        }
        Self { shared }
    }

    /// The visualiser is shown (or not). Hiding frees the decoder and ring.
    pub fn set_enabled(&self, on: bool) {
        let changed = {
            let mut st = locked(&self.shared.state);
            let changed = st.enabled != on;
            st.enabled = on;
            st.epoch += changed as u64;
            if changed && !on {
                st.status = Status::new("off", "none", "hidden");
            }
            changed
        };
        if changed {
            if !on {
                log::info!("{}", self.status().line());
            }
            self.shared.wake.notify_all();
        }
    }

    /// The player's state: whether a song plays, which, and where it was at
    /// `at`. Wakes the worker only for a change that matters.
    pub fn update_at(&self, playing: bool, track: Option<&str>, position_ms: u64, at: Instant) {
        let wake = {
            let mut st = locked(&self.shared.state);
            let jumped = st.anchor.is_some_and(|(pos, was)| {
                let expected = pos + at.saturating_duration_since(was).as_millis() as u64;
                expected.abs_diff(position_ms) > JUMP_MS
            });
            let changed = st.playing != playing || st.track.as_deref() != track || jumped;
            st.playing = playing;
            if st.track.as_deref() != track {
                st.track = track.map(str::to_owned);
            }
            st.anchor = Some((position_ms, at));
            st.epoch += changed as u64;
            changed
        };
        if wake {
            self.shared.wake.notify_all();
        }
    }

    pub fn update(&self, playing: bool, track: Option<&str>, position_ms: u64) {
        self.update_at(playing, track, position_ms, Instant::now());
    }

    /// The frame under the playhead at `now`, if one is ready.
    pub fn frame_at(&self, now: Instant) -> Option<Pulled> {
        let (index, sample_rate) = {
            let st = locked(&self.shared.state);
            if !(st.enabled && st.playing) {
                return None;
            }
            let (pos, at) = st.anchor?;
            (
                playhead_frame(pos, now.saturating_duration_since(at)),
                st.sample_rate,
            )
        };
        let frame = lock(&self.shared.ring).get(index).cloned()?;
        Some(Pulled { sample_rate, frame })
    }

    pub fn frame(&self) -> Option<Pulled> {
        self.frame_at(Instant::now())
    }

    pub fn status(&self) -> Status {
        locked(&self.shared.state).status.clone()
    }
}

/// Whether the pipeline cannot serve `playhead` from where it is: the
/// playhead is before the oldest frame held, or more than a second beyond
/// the last one made.
pub fn needs_seek(playhead: u64, next_frame: u64, held: u64) -> bool {
    playhead > next_frame + FPS || playhead < next_frame.saturating_sub(held)
}

/// Why a track is not being visualised, and when to try it again. `retry_at`
/// is `None` for a song that will never work (an unsupported codec).
struct Failed {
    track: String,
    retry_at: Option<Instant>,
}

struct Snap {
    epoch: u64,
    track: String,
    position_ms: u64,
    at: Instant,
}

fn set_status(shared: &Shared, status: Status) {
    let line = status.line();
    let changed = {
        let mut st = locked(&shared.state);
        let changed = st.status != status;
        st.status = status;
        changed
    };
    if changed {
        match line.split("verdict=").nth(1) {
            Some(v) if v.starts_with("on") => log::info!("{line}"),
            _ => log::warn!("{line}"),
        }
    }
}

/// Wait until the worker has something to do; `None` on shutdown.
fn next_job(
    shared: &Shared,
    pipe: &mut Option<(String, Pipeline)>,
    failed: &mut Option<Failed>,
) -> Option<Snap> {
    let mut st = locked(&shared.state);
    loop {
        if st.shutdown {
            return None;
        }
        if failed.as_ref().map(|f| f.track.as_str()) != st.track.as_deref() {
            *failed = None;
        }
        if failed
            .as_ref()
            .and_then(|f| f.retry_at)
            .is_some_and(|at| Instant::now() >= at)
        {
            *failed = None;
        }
        if let (true, true, Some(track), Some((position_ms, at)), None) = (
            st.enabled,
            st.playing,
            st.track.clone(),
            st.anchor,
            failed.as_ref(),
        ) {
            return Some(Snap {
                epoch: st.epoch,
                track,
                position_ms,
                at,
            });
        }
        if !st.enabled && pipe.take().is_some() {
            lock(&shared.ring).clear();
        }
        st = match failed.as_ref().and_then(|f| f.retry_at) {
            Some(at) => {
                shared
                    .wake
                    .wait_timeout(st, at.saturating_duration_since(Instant::now()))
                    .unwrap_or_else(PoisonError::into_inner)
                    .0
            }
            None => shared.wake.wait(st).unwrap_or_else(PoisonError::into_inner),
        };
    }
}

fn run(shared: &Shared, opener: &dyn Opener, retry: Duration) {
    let mut pipe: Option<(String, Pipeline)> = None;
    let mut failed: Option<Failed> = None;
    while let Some(snap) = next_job(shared, &mut pipe, &mut failed) {
        if pipe.as_ref().map_or(true, |(t, _)| *t != snap.track) {
            pipe = None;
            lock(&shared.ring).clear();
            match open(shared, opener, &snap.track) {
                Ok(p) => pipe = Some((snap.track.clone(), p)),
                Err(status) => {
                    failed = Some(Failed {
                        track: snap.track.clone(),
                        retry_at: (status.verdict != "unsupported").then(|| Instant::now() + retry),
                    });
                    set_status(shared, status);
                    continue;
                }
            }
        }
        let Some((_, p)) = pipe.as_mut() else {
            continue;
        };
        let playhead = playhead_frame(snap.position_ms, snap.at.elapsed());
        let held = lock(&shared.ring).len() as u64;
        let result = (|| {
            if p.ended() && playhead >= p.next_frame() {
                return Ok(());
            }
            if needs_seek(playhead, p.next_frame(), held) {
                p.seek(playhead * 1000 / FPS)?;
            }
            p.fill(playhead, AHEAD_FRAMES)
        })();
        if let Err(e) = result {
            failed = Some(Failed {
                track: snap.track.clone(),
                retry_at: Some(Instant::now() + retry),
            });
            pipe = None;
            lock(&shared.ring).clear();
            set_status(shared, Status::new("off", "unknown", format!("decode:{e}")));
            continue;
        }
        // A timeout, never an open-ended wait: a change that lands between
        // this check and the wait would otherwise be missed.
        let nap = if p.ended() {
            ENDED_NAP
        } else {
            let spare = lock(&shared.ring)
                .ahead_of(playhead)
                .saturating_sub(AHEAD_FRAMES / 2);
            Duration::from_millis(spare * 1000 / FPS).max(MIN_NAP)
        };
        let st = locked(&shared.state);
        if st.epoch != snap.epoch {
            continue;
        }
        drop(
            shared
                .wake
                .wait_timeout(st, nap)
                .unwrap_or_else(PoisonError::into_inner),
        );
    }
}

fn open(shared: &Shared, opener: &dyn Opener, track: &str) -> Result<Pipeline, Status> {
    let opened = opener
        .open(track)
        .map_err(|e| Status::new("off", "unknown", format!("source:{e}")))?;
    let decoder =
        Decoder::open(opened.source, opened.extension.as_deref()).map_err(|e| match e {
            OpenError::Unsupported(why) => Status::new("unsupported", "unknown", why),
            OpenError::Failed(why) => Status::new("off", "unknown", format!("open:{why}")),
        })?;
    let rate = decoder.sample_rate();
    let status = Status::new("on", decoder.codec(), format!("track={track}"));
    locked(&shared.state).sample_rate = rate;
    set_status(shared, status);
    Ok(Pipeline::new(decoder, shared.ring.clone()))
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::fixture;
    use crate::frame::BINS;

    const RATE: u32 = 16_000;

    struct Songs {
        opened: AtomicUsize,
    }

    impl Opener for Songs {
        fn open(&self, id: &str) -> Result<Opened, String> {
            self.opened.fetch_add(1, Ordering::SeqCst);
            let bytes = match id {
                "tones" => {
                    let mut s = fixture::sine(1000.0, RATE, 1.0, 0.5);
                    s.extend(fixture::sine(3000.0, RATE, 2.0, 0.5));
                    fixture::wav(&s, RATE, 1)
                }
                "opus" => vec![9u8; 4096],
                other => return Err(format!("no track {other}")),
            };
            Ok(Opened {
                source: Box::new(Cursor::new(bytes)),
                extension: Some("wav".into()),
            })
        }
    }

    fn engine() -> (Engine, Arc<Songs>) {
        let songs = Arc::new(Songs {
            opened: AtomicUsize::new(0),
        });
        (Engine::new(songs.clone()), songs)
    }

    fn wait_for<T>(mut f: impl FnMut() -> Option<T>) -> T {
        let until = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(v) = f() {
                return v;
            }
            assert!(Instant::now() < until, "timed out");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn peak(p: &Pulled) -> usize {
        (0..BINS).min_by_key(|&i| p.frame.bins[i]).unwrap_or(0)
    }

    #[test]
    fn a_shown_playing_song_gets_frames_and_says_why() {
        let (e, _) = engine();
        let t0 = Instant::now();
        e.set_enabled(true);
        e.update_at(true, Some("tones"), 500, t0);
        let got = wait_for(|| e.frame());
        assert_eq!(got.sample_rate, RATE);
        assert!((126..=130).contains(&peak(&got)), "1 kHz at 0.5 s");
        let line = e.status().line();
        assert!(
            line.starts_with("visualiser source=decode verdict=on codec="),
            "{line}"
        );
        assert!(line.contains("reason=track=tones"), "{line}");
    }

    #[test]
    fn the_playhead_moves_with_the_clock_between_reports() {
        let (e, _) = engine();
        let t0 = Instant::now();
        e.set_enabled(true);
        e.update_at(true, Some("tones"), 500, t0);
        wait_for(|| e.frame());
        let later = wait_for(|| e.frame_at(t0 + Duration::from_millis(1500)));
        assert!(
            (382..=386).contains(&peak(&later)),
            "2.0 s is the 3 kHz tone"
        );
    }

    #[test]
    fn a_jump_in_position_seeks_the_decoder() {
        let (e, _) = engine();
        let t0 = Instant::now();
        e.set_enabled(true);
        e.update_at(true, Some("tones"), 200, t0);
        wait_for(|| e.frame());
        let t1 = t0 + Duration::from_millis(100);
        e.update_at(true, Some("tones"), 2500, t1);
        let got = wait_for(|| e.frame());
        assert!((382..=386).contains(&peak(&got)), "2.5 s is the 3 kHz tone");
    }

    #[test]
    fn nothing_is_opened_while_hidden_or_paused() {
        let (e, songs) = engine();
        let t0 = Instant::now();
        e.update_at(true, Some("tones"), 0, t0);
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(songs.opened.load(Ordering::SeqCst), 0, "hidden");
        assert!(e.frame().is_none());
        e.update_at(false, Some("tones"), 0, t0);
        e.set_enabled(true);
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(songs.opened.load(Ordering::SeqCst), 0, "paused");
    }

    #[test]
    fn pausing_or_hiding_stops_the_frames() {
        let (e, _) = engine();
        let t0 = Instant::now();
        e.set_enabled(true);
        e.update_at(true, Some("tones"), 500, t0);
        wait_for(|| e.frame());
        e.update_at(false, Some("tones"), 500, t0);
        assert!(e.frame().is_none(), "paused");
        e.update_at(true, Some("tones"), 500, t0);
        wait_for(|| e.frame());
        e.set_enabled(false);
        assert!(e.frame().is_none(), "hidden");
        assert_eq!(
            e.status().line(),
            "visualiser source=decode verdict=off codec=none reason=hidden"
        );
    }

    #[test]
    fn an_unsupported_song_has_no_frames_and_the_status_names_the_cause() {
        let (e, songs) = engine();
        let t0 = Instant::now();
        e.set_enabled(true);
        e.update_at(true, Some("opus"), 0, t0);
        wait_for(|| (e.status().verdict == "unsupported").then_some(()));
        assert!(e.frame().is_none());
        let line = e.status().line();
        assert!(
            line.starts_with("visualiser source=decode verdict=unsupported codec="),
            "{line}"
        );
        assert!(
            line.contains("reason=") && !line.ends_with("reason="),
            "{line}"
        );
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(songs.opened.load(Ordering::SeqCst), 1, "no retry loop");
    }

    #[test]
    fn a_track_that_cannot_be_read_is_off_with_the_reason() {
        let (e, _) = engine();
        e.set_enabled(true);
        e.update_at(true, Some("missing"), 0, Instant::now());
        wait_for(|| e.status().reason.starts_with("source:").then_some(()));
        assert!(e.status().line().contains("reason=source:no track missing"));
    }

    struct Flaky {
        fails: usize,
        opened: AtomicUsize,
    }

    impl Opener for Flaky {
        fn open(&self, _id: &str) -> Result<Opened, String> {
            if self.opened.fetch_add(1, Ordering::SeqCst) < self.fails {
                return Err("connection reset".into());
            }
            Songs {
                opened: AtomicUsize::new(0),
            }
            .open("tones")
        }
    }

    #[test]
    fn a_source_that_failed_to_open_is_tried_again_for_the_same_track() {
        let flaky = Arc::new(Flaky {
            fails: 2,
            opened: AtomicUsize::new(0),
        });
        let e = Engine::with_retry(flaky.clone(), Duration::from_millis(20));
        e.set_enabled(true);
        e.update_at(true, Some("tones"), 500, Instant::now());
        let got = wait_for(|| e.frame());
        assert_eq!(got.sample_rate, RATE);
        assert_eq!(
            flaky.opened.load(Ordering::SeqCst),
            3,
            "two failures, one success"
        );
    }

    #[test]
    fn a_seek_is_needed_only_outside_what_is_held() {
        assert!(!needs_seek(10, 70, 60), "inside the window");
        assert!(!needs_seek(70, 70, 0), "right at the edge");
        assert!(needs_seek(9, 70, 60), "before the oldest held");
        assert!(needs_seek(101, 70, 60), "over a second beyond");
        assert!(needs_seek(40, 45, 0), "behind an empty ring");
    }
}
