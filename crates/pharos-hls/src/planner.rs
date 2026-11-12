//! Pure buffer-management state machine for the Pharos HLS engine (ADR-0071).

use crate::playlist::Segment;
use crate::timeline::{range_of_slice, segment_at_slice, segments_duration_ms};

/// Default forward buffer target in milliseconds (spec 031 budget table).
const DEFAULT_FORWARD_MS: u64 = 30_000;

/// Default back-buffer retention window in milliseconds (spec 031 budget table).
const DEFAULT_BACK_MS: u64 = 15_000;

/// Minimum forward buffer target in milliseconds after quota reductions.
const DEFAULT_MIN_FORWARD_MS: u64 = 8_000;

/// The most one track may buffer ahead of the other. Under one 6 s segment, so
/// a track never starts a segment whose aligned predecessor the other has not
/// finished, yet above the few hundred ms their range ends differ by.
const MAX_TRACK_LEAD_MS: u64 = 2_000;

/// Appends of one segment that leave it unbuffered before it is given up on: a
/// refetch can repair a partial remove, a third cannot change the answer.
const MAX_APPENDS_PER_SEGMENT: u32 = 2;

/// A skip lands this far past the start of the data it jumps to, as the stall
/// watchdog's jumps do (hls.js `skipBufferHolePadding`).
const SKIP_PADDING_MS: u64 = 100;

/// Media track managed by [`Planner`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Track {
    Video,
    Audio,
}

impl Track {
    /// Stable telemetry and log token for this track.
    pub fn label(self) -> &'static str {
        match self {
            Self::Video => "video",
            Self::Audio => "audio",
        }
    }
}

/// Forward and back-buffer targets for [`Planner`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    /// Media to keep buffered ahead of the play head.
    pub forward_ms: u64,
    /// Media to keep behind the play head before evicting.
    pub back_ms: u64,
    /// The forward target never shrinks below this after quota errors.
    pub min_forward_ms: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            forward_ms: DEFAULT_FORWARD_MS,
            back_ms: DEFAULT_BACK_MS,
            min_forward_ms: DEFAULT_MIN_FORWARD_MS,
        }
    }
}

/// An in-flight segment request for a single [`Track`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Request {
    Init,
    Media(usize),
}

/// Command emitted by [`Planner`] for the MSE adapter to execute.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Fetch and append the track's init segment (once, before any media).
    FetchInit { track: Track },
    /// Fetch and append media segment `index` of the track.
    Fetch { track: Track, index: usize },
    /// Abort the track's in-flight request (after a seek).
    Abort { track: Track },
    /// Remove [start_ms, end_ms) from the track's buffer.
    Evict {
        track: Track,
        start_ms: u64,
        end_ms: u64,
    },
    /// Every segment of every track is appended: call endOfStream.
    EndOfStream,
    /// The play head sits in a segment that was given up on: set `currentTime`
    /// to `to_ms`, the start of the data after it.
    Skip { to_ms: u64 },
}

/// A decision the planner took that its actions do not show, drained with
/// [`Planner::take_notes`] for the adapter to log and report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Note {
    /// `EndOfStream` was emitted with the play head at `play_ms` and the
    /// furthest buffered end at `buffered_end_ms`.
    EndOfStream { play_ms: u64, buffered_end_ms: u64 },
    /// A segment still unbuffered after `appends` appends is no longer fetched
    /// (its data is shorter than the playlist says, or the append is a no-op).
    GaveUp {
        track: Track,
        index: usize,
        appends: u32,
    },
    /// The play head moved from `from_ms` to `to_ms` past the given-up segment
    /// `index` of `track`.
    Skipped {
        track: Track,
        index: usize,
        from_ms: u64,
        to_ms: u64,
    },
    /// `track` waits for the other: it would buffer `lead_ms` ahead of it,
    /// over `limit_ms`. Noted once per wait.
    Held {
        track: Track,
        lead_ms: u64,
        limit_ms: u64,
    },
}

impl Note {
    /// Stable log and telemetry token.
    pub fn label(&self) -> &'static str {
        match self {
            Self::EndOfStream { .. } => "end_of_stream",
            Self::GaveUp { .. } => "gave_up",
            Self::Held { .. } => "held",
            Self::Skipped { .. } => "skipped",
        }
    }
}

/// Buffered `[start_ms, end_ms)` ranges reported by each track's `SourceBuffer`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Buffered {
    pub video: Vec<(u64, u64)>,
    pub audio: Vec<(u64, u64)>,
}

#[derive(Debug, Clone)]
struct TrackState {
    segments: Vec<Segment>,
    needs_init: bool,
    in_flight: Option<Request>,
    seek_target_ms: Option<u64>,
    /// The seek target's segment was fetched again because the target itself
    /// was not buffered, though most of its segment was; once per seek.
    seek_refetched: bool,
    last_appended: bool,
    /// The segment most recently appended and how many times in a row.
    appends: Option<(usize, u32)>,
    /// Segments left unbuffered after [`MAX_APPENDS_PER_SEGMENT`] appends.
    given_up: Vec<usize>,
    /// Waiting for the other track; set while the wait has been noted.
    held: bool,
}

impl TrackState {
    fn new(segments: Vec<Segment>, has_init: bool, start_ms: u64) -> Self {
        Self {
            last_appended: segments.is_empty() && !has_init,
            segments,
            needs_init: has_init,
            in_flight: None,
            seek_target_ms: Some(start_ms),
            seek_refetched: false,
            appends: None,
            given_up: Vec::new(),
            held: false,
        }
    }
}

/// Pure state machine deciding segment fetches, aborts, evictions, and end-of-stream.
#[derive(Debug, Clone)]
pub struct Planner {
    video: TrackState,
    audio: Option<TrackState>,
    config: Config,
    forward_target_ms: u64,
    eos_emitted: bool,
    notes: Vec<Note>,
}

impl Planner {
    /// `video` and optional `audio` are each track's segment list (from
    /// `MediaPlaylist::segments`); `has_init` per track says whether an
    /// EXT-X-MAP init must be appended first.
    pub fn new(
        video: Vec<Segment>,
        video_init: bool,
        audio: Option<(Vec<Segment>, bool)>,
        config: Config,
        start_ms: u64,
    ) -> Self {
        let forward_target_ms = config.forward_ms.max(config.min_forward_ms);
        Self {
            video: TrackState::new(video, video_init, start_ms),
            audio: audio.map(|(segs, init)| TrackState::new(segs, init, start_ms)),
            config,
            forward_target_ms,
            eos_emitted: false,
            notes: Vec::new(),
        }
    }

    /// The decisions taken since the last call, oldest first.
    pub fn take_notes(&mut self) -> Vec<Note> {
        std::mem::take(&mut self.notes)
    }

    /// Called on every timeupdate / progress tick and after each append, with
    /// the play head and each track's buffered ranges (sorted, from the
    /// SourceBuffer). Returns the actions to take now.
    pub fn tick(&mut self, play_ms: u64, buffered: &Buffered) -> Vec<Action> {
        let mut actions = Vec::new();
        let has_audio = self.audio.is_some();

        let audio_finished = self.audio.as_ref().map_or(true, |a| {
            !a.needs_init && is_track_covered_from_playhead(a, play_ms, &buffered.audio)
        });
        let video_finished = !self.video.needs_init
            && is_track_covered_from_playhead(&self.video, play_ms, &buffered.video);

        let video_view = gate_view(&self.video, &buffered.video);
        let audio_view = self
            .audio
            .as_ref()
            .map_or_else(Vec::new, |a| gate_view(a, &buffered.audio));

        // The track further behind plans first, so it gets the connection.
        let audio_first = has_audio
            && buffered_end_at(&buffered.audio, play_ms)
                < buffered_end_at(&buffered.video, play_ms);
        let order = if audio_first {
            [Track::Audio, Track::Video]
        } else {
            [Track::Video, Track::Audio]
        };
        for track in order {
            let (state, own, other, other_finished) = match track {
                Track::Video => (
                    Some(&mut self.video),
                    &buffered.video[..],
                    &audio_view[..],
                    audio_finished,
                ),
                Track::Audio => (
                    self.audio.as_mut(),
                    &buffered.audio[..],
                    &video_view[..],
                    video_finished,
                ),
            };
            let Some(state) = state else {
                continue;
            };
            skip_given_up(track, state, play_ms, own, &mut actions, &mut self.notes);
            plan_track(
                track,
                state,
                play_ms,
                own,
                has_audio.then_some((other, other_finished)),
                self.config.back_ms,
                self.forward_target_ms,
                &mut actions,
                &mut self.notes,
            );
        }

        if !self.eos_emitted && self.ready_for_eos(play_ms, buffered) {
            self.eos_emitted = true;
            let buffered_end_ms = buffered
                .video
                .iter()
                .chain(&buffered.audio)
                .map(|&(_, end)| end)
                .max()
                .unwrap_or(0);
            self.notes.push(Note::EndOfStream {
                play_ms,
                buffered_end_ms,
            });
            actions.push(Action::EndOfStream);
        }

        actions
    }

    /// A seek to `to_ms`: aborts in-flight requests whose segment does not
    /// hold `to_ms`, and restarts fetching from the segment that holds it.
    pub fn seek(&mut self, to_ms: u64, buffered: &Buffered) -> Vec<Action> {
        let mut actions = Vec::new();
        let video_end = segments_duration_ms(&self.video.segments);
        let audio_end = self
            .audio
            .as_ref()
            .map_or(0, |a| segments_duration_ms(&a.segments));
        let stream_end = video_end.max(audio_end);

        if to_ms < stream_end || stream_end == 0 {
            self.eos_emitted = false;
        }

        seek_track(
            Track::Video,
            &mut self.video,
            to_ms,
            &buffered.video,
            &mut actions,
        );
        if let Some(audio) = &mut self.audio {
            seek_track(Track::Audio, audio, to_ms, &buffered.audio, &mut actions);
        }

        actions
    }

    /// The adapter finished appending the request it was given.
    pub fn appended(&mut self, track: Track) {
        let Some(state) = self.track_mut(track) else {
            return;
        };
        match state.in_flight.take() {
            Some(Request::Init) => {
                state.needs_init = false;
                if state.segments.is_empty() {
                    state.last_appended = true;
                }
            }
            Some(Request::Media(idx)) => {
                let n = match state.appends {
                    Some((prev, n)) if prev == idx => n.saturating_add(1),
                    _ => 1,
                };
                state.appends = Some((idx, n));
                if idx.saturating_add(1) >= state.segments.len() {
                    state.last_appended = true;
                }
            }
            None => {}
        }
    }

    /// The track moves to another rendition on the same segment grid (a quality
    /// switch): its init is fetched again before the next media segment, and
    /// what is already buffered stays. Refused (`false`) while a request is in
    /// flight, so a segment of the old rendition never lands after the new
    /// init, and once the last segment is appended, since nothing is left to
    /// fetch at the new quality.
    pub fn reinit(&mut self, track: Track) -> bool {
        let Some(state) = self.track_mut(track) else {
            return false;
        };
        if state.in_flight.is_some() || state.last_appended {
            return false;
        }
        state.needs_init = true;
        true
    }

    /// The request failed or was aborted; the segment is fetched again on a later tick.
    pub fn failed(&mut self, track: Track) {
        if let Some(state) = self.track_mut(track) {
            state.in_flight = None;
        }
    }

    /// The browser refused an append with QuotaExceededError: halve the
    /// forward target (not below `min_forward_ms`), return the new target.
    pub fn quota_exceeded(&mut self, track: Track) -> u64 {
        if let Some(state) = self.track_mut(track) {
            state.in_flight = None;
        }
        self.forward_target_ms = (self.forward_target_ms / 2).max(self.config.min_forward_ms);
        self.forward_target_ms
    }

    /// Current forward buffer target in milliseconds.
    pub fn forward_target_ms(&self) -> u64 {
        self.forward_target_ms
    }

    /// Returns the currently in-flight request for `track`, if any.
    pub fn in_flight(&self, track: Track) -> Option<Request> {
        match track {
            Track::Video => self.video.in_flight,
            Track::Audio => self.audio.as_ref().and_then(|a| a.in_flight),
        }
    }

    fn track_mut(&mut self, track: Track) -> Option<&mut TrackState> {
        match track {
            Track::Video => Some(&mut self.video),
            Track::Audio => self.audio.as_mut(),
        }
    }

    fn ready_for_eos(&self, play_ms: u64, buffered: &Buffered) -> bool {
        track_ready_for_eos(&self.video, play_ms, &buffered.video)
            && self
                .audio
                .as_ref()
                .map_or(true, |a| track_ready_for_eos(a, play_ms, &buffered.audio))
    }
}

fn seek_track(
    track: Track,
    state: &mut TrackState,
    to_ms: u64,
    track_buffered: &[(u64, u64)],
    actions: &mut Vec<Action>,
) {
    let target_idx = clamped_segment_at(&state.segments, to_ms);
    if let Some(Request::Media(in_flight_idx)) = state.in_flight {
        if Some(in_flight_idx) != target_idx {
            state.in_flight = None;
            actions.push(Action::Abort { track });
        }
    }
    state.seek_target_ms = Some(to_ms);
    state.seek_refetched = false;
    state.appends = None;
    state.given_up.clear();
    let end_ms = segments_duration_ms(&state.segments);
    if to_ms < end_ms {
        state.last_appended = state
            .segments
            .last()
            .is_some_and(|last| is_segment_covered(last, track_buffered));
    }
}

/// The play head sits in a segment given up on, with no data under it and
/// data further on: the stall watchdog only jumps ordinary gaps, so the planner
/// moves the play head to that data and lets go of the seek anchor.
fn skip_given_up(
    track: Track,
    state: &mut TrackState,
    play_ms: u64,
    track_buffered: &[(u64, u64)],
    actions: &mut Vec<Action>,
    notes: &mut Vec<Note>,
) {
    if actions.iter().any(|a| matches!(a, Action::Skip { .. })) {
        return;
    }
    let Some(index) = state.given_up.iter().copied().find(|&idx| {
        range_of_slice(&state.segments, idx).is_some_and(|(s, e)| s <= play_ms && play_ms < e)
    }) else {
        return;
    };
    if point_buffered(track_buffered, play_ms) {
        return;
    }
    let Some(next_start) = track_buffered
        .iter()
        .map(|&(start, _)| start)
        .filter(|&start| start > play_ms)
        .min()
    else {
        return;
    };
    let to_ms = next_start.saturating_add(SKIP_PADDING_MS);
    state.seek_target_ms = None;
    actions.push(Action::Skip { to_ms });
    notes.push(Note::Skipped {
        track,
        index,
        from_ms: play_ms,
        to_ms,
    });
}

#[allow(clippy::too_many_arguments)]
fn plan_track(
    track: Track,
    state: &mut TrackState,
    play_ms: u64,
    track_buffered: &[(u64, u64)],
    other_track: Option<(&[(u64, u64)], bool)>,
    back_ms: u64,
    forward_target_ms: u64,
    actions: &mut Vec<Action>,
    notes: &mut Vec<Note>,
) {
    if state.in_flight.is_some() {
        return;
    }

    let cutoff_ms = segment_floor(
        &state.segments,
        eviction_anchor(state, play_ms).saturating_sub(back_ms),
    );
    if let Some(&(range_start, range_end)) = track_buffered.first() {
        if range_start < range_end && range_start < cutoff_ms {
            actions.push(Action::Evict {
                track,
                start_ms: range_start,
                end_ms: cutoff_ms,
            });
        }
    }

    if state.needs_init {
        state.in_flight = Some(Request::Init);
        actions.push(Action::FetchInit { track });
        return;
    }

    let Some((start_idx, anchor_ms)) = resolve_playhead(state, play_ms, track_buffered) else {
        return;
    };

    // A segment counts as buffered when its midpoint is; a seek target just
    // short of where a partly removed segment's data starts is not, so that
    // segment is fetched again once.
    let refetch_target = state.seek_target_ms.is_some()
        && !state.seek_refetched
        && !point_buffered(track_buffered, anchor_ms);
    // Appends only count against a segment that stays unbuffered.
    if let Some((idx, _)) = state.appends {
        if state
            .segments
            .get(idx)
            .is_some_and(|seg| is_segment_covered(seg, track_buffered))
        {
            state.appends = None;
        }
    }
    let mut found = None;
    for idx in start_idx..state.segments.len() {
        let Some(seg) = state.segments.get(idx) else {
            break;
        };
        if idx == start_idx && refetch_target {
            found = Some((idx, seg.clone()));
            break;
        }
        if is_settled(state, idx, track_buffered) {
            continue;
        }
        match state.appends {
            Some((last, appends)) if last == idx && appends >= MAX_APPENDS_PER_SEGMENT => {
                state.given_up.push(idx);
                notes.push(Note::GaveUp {
                    track,
                    index: idx,
                    appends,
                });
            }
            _ => {
                found = Some((idx, seg.clone()));
                break;
            }
        }
    }
    let Some((candidate_idx, candidate_seg)) = found else {
        return;
    };

    if candidate_seg.start_ms >= anchor_ms.saturating_add(forward_target_ms) {
        return;
    }

    if let Some((other_buffered, other_finished)) = other_track {
        if !other_finished {
            let other_end = buffered_end_at(other_buffered, anchor_ms);
            let lead_ms = candidate_seg.start_ms.saturating_sub(other_end);
            if lead_ms > MAX_TRACK_LEAD_MS {
                if !state.held {
                    state.held = true;
                    notes.push(Note::Held {
                        track,
                        lead_ms,
                        limit_ms: MAX_TRACK_LEAD_MS,
                    });
                }
                return;
            }
        }
    }
    state.held = false;

    if candidate_idx == start_idx && refetch_target {
        state.seek_refetched = true;
    }
    state.in_flight = Some(Request::Media(candidate_idx));
    actions.push(Action::Fetch {
        track,
        index: candidate_idx,
    });
}

/// The start of the segment holding `at_ms`, so eviction removes whole
/// segments and never leaves one partly buffered; `at_ms` outside every segment.
fn segment_floor(segments: &[Segment], at_ms: u64) -> u64 {
    segment_at_slice(segments, at_ms)
        .and_then(|idx| segments.get(idx))
        .map_or(at_ms, |seg| seg.start_ms)
}

/// What the other track's lead is measured against: its buffered ranges plus
/// every settled segment (given up on, or buffered short of its end), which
/// will gain no more data, so the track waiting on it never waits on a gap
/// that no fetch will fill.
fn gate_view(state: &TrackState, buffered: &[(u64, u64)]) -> Vec<(u64, u64)> {
    let mut view = buffered.to_vec();
    view.extend(
        (0..state.segments.len())
            .filter(|&idx| is_settled(state, idx, buffered))
            .filter_map(|idx| range_of_slice(&state.segments, idx)),
    );
    view.sort_unstable();
    view
}

fn point_buffered(buffered: &[(u64, u64)], at_ms: u64) -> bool {
    buffered
        .iter()
        .any(|&(start, end)| start <= at_ms && at_ms < end)
}

/// The play position eviction keeps `back_ms` behind: the play head clamped
/// into the track, and no later than a pending seek target, so a seek past
/// the end never evicts the segment it is about to fetch.
fn eviction_anchor(state: &TrackState, play_ms: u64) -> u64 {
    let end_ms = segments_duration_ms(&state.segments).saturating_sub(1);
    let at = play_ms.min(end_ms);
    match state.seek_target_ms {
        Some(target) => at.min(target),
        None => at,
    }
}

fn resolve_playhead(
    state: &mut TrackState,
    play_ms: u64,
    track_buffered: &[(u64, u64)],
) -> Option<(usize, u64)> {
    if state.segments.is_empty() {
        return None;
    }

    if let Some(target_ms) = state.seek_target_ms {
        let target_idx = clamped_segment_at(&state.segments, target_ms)?;
        let target_seg = state.segments.get(target_idx)?;
        let covered = state.given_up.contains(&target_idx)
            || (is_segment_covered(target_seg, track_buffered)
                && (state.seek_refetched || point_buffered(track_buffered, target_ms)));
        if covered && play_ms != 0 && play_ms != target_ms {
            state.seek_target_ms = None;
        } else {
            let (seg_start, seg_end) = range_of_slice(&state.segments, target_idx)?;
            let anchor_ms = target_ms.clamp(seg_start, seg_end.saturating_sub(1).max(seg_start));
            return Some((target_idx, anchor_ms));
        }
    }

    let idx = segment_at_or_after(&state.segments, play_ms)?;
    Some((idx, play_ms))
}

fn clamped_segment_at(segments: &[Segment], at_ms: u64) -> Option<usize> {
    if segments.is_empty() {
        return None;
    }
    if let Some(idx) = segment_at_slice(segments, at_ms) {
        return Some(idx);
    }
    if let Some(idx) = segment_at_or_after(segments, at_ms) {
        return Some(idx);
    }
    Some(segments.len().saturating_sub(1))
}

fn segment_at_or_after(segments: &[Segment], at_ms: u64) -> Option<usize> {
    if let Some(idx) = segment_at_slice(segments, at_ms) {
        return Some(idx);
    }
    segments
        .iter()
        .position(|seg| at_ms < seg.start_ms.saturating_add(seg.duration_ms))
}

/// Nothing is left to fetch for segment `idx`: it is buffered, given up on, or
/// the final segment, appended, with data that stops short of its midpoint.
fn is_settled(state: &TrackState, idx: usize, buffered: &[(u64, u64)]) -> bool {
    let Some(seg) = state.segments.get(idx) else {
        return true;
    };
    if is_segment_covered(seg, buffered) || state.given_up.contains(&idx) {
        return true;
    }
    let seg_end = seg.start_ms.saturating_add(seg.duration_ms);
    state.last_appended
        && idx.saturating_add(1) == state.segments.len()
        && buffered
            .iter()
            .any(|&(start, end)| start < seg_end && end > seg.start_ms)
}

fn is_segment_covered(seg: &Segment, buffered: &[(u64, u64)]) -> bool {
    let mid_ms = seg.start_ms.saturating_add(seg.duration_ms / 2);
    buffered.iter().any(|&(start, end)| {
        start <= mid_ms && (mid_ms < end || (seg.duration_ms == 0 && mid_ms == end))
    })
}

fn buffered_end_at(buffered: &[(u64, u64)], play_ms: u64) -> u64 {
    let mut end_ms = play_ms;
    for &(start, end) in buffered {
        if start <= end_ms && end > end_ms {
            end_ms = end;
        }
    }
    end_ms
}

fn is_track_covered_from_playhead(
    state: &TrackState,
    play_ms: u64,
    track_buffered: &[(u64, u64)],
) -> bool {
    if state.segments.is_empty() {
        return true;
    }
    let start_idx = match state.seek_target_ms {
        Some(target_ms) => clamped_segment_at(&state.segments, target_ms),
        None => segment_at_or_after(&state.segments, play_ms),
    };
    let Some(idx) = start_idx else {
        return is_settled(
            state,
            state.segments.len().saturating_sub(1),
            track_buffered,
        );
    };
    (idx..state.segments.len()).all(|i| is_settled(state, i, track_buffered))
}

fn track_ready_for_eos(state: &TrackState, play_ms: u64, track_buffered: &[(u64, u64)]) -> bool {
    !state.needs_init
        && state.in_flight.is_none()
        && state.last_appended
        && is_track_covered_from_playhead(state, play_ms, track_buffered)
}

#[cfg(test)]
mod tests {
    // Panicking on unexpected planner states is the intended failure mode in tests.
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use proptest::prelude::*;

    use super::{Action, Buffered, Config, Note, Planner, Request, Track};
    use crate::playlist::Segment;
    use crate::timeline::segments_duration_ms;

    fn make_segments(durations_ms: &[u64]) -> Vec<Segment> {
        let mut start_ms = 0u64;
        let mut out = Vec::with_capacity(durations_ms.len());
        for (i, &duration_ms) in durations_ms.iter().enumerate() {
            out.push(Segment {
                uri: format!("{i}.m4s"),
                start_ms,
                duration_ms,
            });
            start_ms = start_ms.saturating_add(duration_ms);
        }
        out
    }

    fn merge_range(ranges: &mut Vec<(u64, u64)>, new_start: u64, new_end: u64) {
        if new_start >= new_end {
            return;
        }
        ranges.push((new_start, new_end));
        ranges.sort_by_key(|&(s, _)| s);
        let mut merged: Vec<(u64, u64)> = Vec::with_capacity(ranges.len());
        for (s, e) in ranges.drain(..) {
            if let Some(last) = merged.last_mut() {
                if s <= last.1 {
                    last.1 = last.1.max(e);
                    continue;
                }
            }
            merged.push((s, e));
        }
        *ranges = merged;
    }

    fn evict_range(ranges: &mut Vec<(u64, u64)>, rem_start: u64, rem_end: u64) {
        if rem_start >= rem_end {
            return;
        }
        let mut out = Vec::with_capacity(ranges.len());
        for &(s, e) in ranges.iter() {
            if e <= rem_start || s >= rem_end {
                out.push((s, e));
            } else {
                if s < rem_start {
                    out.push((s, rem_start));
                }
                if e > rem_end {
                    out.push((rem_end, e));
                }
            }
        }
        *ranges = out;
    }

    /// Drives a planner like the adapter: every fetch is appended at once and
    /// buffers `real_ms(track, index)` of the segment. Returns the fetches made
    /// and whether `EndOfStream` came, over `ticks` ticks at `play_ms`.
    fn drive(
        planner: &mut Planner,
        segs: &[Segment],
        play_ms: u64,
        ticks: usize,
        real_ms: impl Fn(Track, usize) -> u64,
    ) -> (Vec<(Track, usize)>, bool, Buffered) {
        let mut buffered = Buffered::default();
        let mut fetches = Vec::new();
        let mut eos = false;
        for _ in 0..ticks {
            for action in planner.tick(play_ms, &buffered) {
                match action {
                    Action::Fetch { track, index } => {
                        fetches.push((track, index));
                        let seg = &segs[index];
                        let end = seg.start_ms + real_ms(track, index).min(seg.duration_ms);
                        let ranges = match track {
                            Track::Video => &mut buffered.video,
                            Track::Audio => &mut buffered.audio,
                        };
                        merge_range(ranges, seg.start_ms, end);
                        planner.appended(track);
                    }
                    Action::FetchInit { track } => planner.appended(track),
                    Action::EndOfStream => eos = true,
                    _ => {}
                }
            }
        }
        (fetches, eos, buffered)
    }

    // B1392: a last segment that buffers less than its playlist duration (a
    // short final GOP) never reached its midpoint, so the track never counted
    // as covered and EndOfStream was never emitted: playback hung at the end.
    #[test]
    fn a_last_segment_that_buffers_short_still_ends_the_stream() {
        let segs = make_segments(&[6_000, 6_000, 6_000]);
        let mut planner = Planner::new(segs.clone(), false, None, Config::default(), 0);
        let (fetches, eos, _) = drive(&mut planner, &segs, 1_000, 20, |_, i| {
            if i == 2 {
                1_000
            } else {
                6_000
            }
        });
        assert!(eos, "EndOfStream never emitted; fetches={fetches:?}");
        assert_eq!(fetches.len(), 3, "{fetches:?}");
    }

    // B1393: a segment whose append never covers its midpoint was fetched on
    // every tick, forever (a storm of requests for one index).
    #[test]
    fn a_segment_that_never_buffers_is_given_up_after_two_appends() {
        let segs = make_segments(&[6_000; 6]);
        let mut planner = Planner::new(
            segs.clone(),
            false,
            Some((segs.clone(), false)),
            Config::default(),
            0,
        );
        let (fetches, eos, _) = drive(&mut planner, &segs, 10_000, 200, |t, i| {
            if t == Track::Audio && i == 2 {
                0
            } else {
                6_000
            }
        });
        let of_it = fetches.iter().filter(|&&f| f == (Track::Audio, 2)).count();
        assert!(of_it <= 2, "audio segment 2 fetched {of_it} times");
        assert!(fetches.contains(&(Track::Audio, 5)), "{fetches:?}");
        assert!(eos);
    }

    // B1394: with nothing of video buffered, audio ran two segments (12 s)
    // ahead, starving video of the bandwidth and of the play start.
    #[test]
    fn audio_cannot_run_ahead_of_an_empty_video_buffer() {
        let segs = make_segments(&[6_000; 10]);
        let mut planner = Planner::new(
            segs.clone(),
            false,
            Some((segs, false)),
            Config::default(),
            0,
        );
        let mut buf = Buffered::default();
        planner.tick(0, &buf);
        planner.appended(Track::Audio);
        buf.audio.push((0, 6_000));
        // Video segment 0 is still in flight: audio segment 1 must wait.
        assert_eq!(planner.tick(0, &buf), Vec::new());
    }

    // B1394: a track whose first segment buffers short (settled by its
    // midpoint) and whose next lands past the gap left the lead gate measuring
    // the other track's contiguous end at the gap, holding both tracks forever.
    #[test]
    fn a_gap_inside_a_settled_segment_does_not_hold_both_tracks() {
        let segs = make_segments(&[6_000; 10]);
        let mut planner = Planner::new(
            segs.clone(),
            false,
            Some((segs, false)),
            Config::default(),
            0,
        );
        let buf = Buffered {
            video: vec![(0, 6_000)],
            audio: vec![(0, 3_500), (6_000, 12_000)],
        };
        let fetched: Vec<Action> = planner.tick(0, &buf);
        assert!(
            fetched.contains(&Action::Fetch {
                track: Track::Video,
                index: 1
            }),
            "{fetched:?}"
        );
    }

    // B1393: giving up on the seek-target segment left the seek target set, so
    // the planning anchor stayed at the target while the play head moved on and
    // fetching stopped one forward window past the target.
    #[test]
    fn giving_up_the_seek_target_segment_releases_the_anchor() {
        let segs = make_segments(&[6_000; 10]);
        let mut planner = Planner::new(segs.clone(), false, None, Config::default(), 0);
        let (fetches, _, _) = drive(&mut planner, &segs, 10_000, 50, |_, i| {
            if i == 0 {
                0
            } else {
                6_000
            }
        });
        assert!(fetches.contains(&(Track::Video, 6)), "{fetches:?}");
    }

    // B1393: a given-up segment leaves a gap wider than the stall watchdog
    // jumps, and at play head 0 the seek anchor was never released: the
    // planner now skips the play head to the data after it.
    #[test]
    fn a_given_up_start_segment_is_skipped_and_fetching_goes_on() {
        let segs = make_segments(&[6_000; 10]);
        let mut planner = Planner::new(segs.clone(), false, None, Config::default(), 0);
        let mut buffered = Buffered::default();
        let mut play_ms = 0;
        let mut fetches = Vec::new();
        let mut skipped_to = None;
        for _ in 0..40 {
            for action in planner.tick(play_ms, &buffered) {
                match action {
                    Action::Fetch { track, index } => {
                        fetches.push(index);
                        if index != 0 {
                            let seg = &segs[index];
                            merge_range(
                                &mut buffered.video,
                                seg.start_ms,
                                seg.start_ms + seg.duration_ms,
                            );
                        }
                        planner.appended(track);
                    }
                    Action::Skip { to_ms } => {
                        skipped_to = Some(to_ms);
                        play_ms = to_ms;
                    }
                    _ => {}
                }
            }
        }
        assert_eq!(skipped_to, Some(6_100), "{fetches:?}");
        assert!(fetches.iter().any(|&i| i >= 6), "{fetches:?}");
        assert!(planner.take_notes().contains(&Note::Skipped {
            track: Track::Video,
            index: 0,
            from_ms: 0,
            to_ms: 6_100
        }));
    }

    // B1394: a segment appended on two successive rounds that was covered in
    // between (then evicted) is fetched again, not given up without a request.
    #[test]
    fn appends_are_forgotten_once_the_segment_is_seen_covered() {
        let segs = make_segments(&[6_000]);
        let mut planner = Planner::new(segs, false, None, Config::default(), 0);
        let empty = Buffered::default();
        let covered = Buffered {
            video: vec![(0, 6_000)],
            audio: Vec::new(),
        };
        for _ in 0..2 {
            assert_eq!(
                planner.tick(1_000, &empty),
                vec![Action::Fetch {
                    track: Track::Video,
                    index: 0
                }]
            );
            planner.appended(Track::Video);
            planner.tick(1_000, &covered);
        }
        assert_eq!(
            planner.tick(1_000, &empty),
            vec![Action::Fetch {
                track: Track::Video,
                index: 0
            }]
        );
    }

    // B1394: when both tracks may fetch, the one further behind goes first.
    #[test]
    fn the_lagging_track_is_fetched_first() {
        let segs = make_segments(&[6_000; 10]);
        let mut planner = Planner::new(
            segs.clone(),
            false,
            Some((segs, false)),
            Config::default(),
            0,
        );
        let buf = Buffered {
            video: vec![(0, 6_000)],
            audio: vec![(0, 5_000)],
        };
        let tracks: Vec<Track> = planner
            .tick(0, &buf)
            .into_iter()
            .filter_map(|a| match a {
                Action::Fetch { track, .. } => Some(track),
                _ => None,
            })
            .collect();
        assert_eq!(tracks.first(), Some(&Track::Audio));
    }

    // Each decision the planner takes silently is a note the adapter logs.
    #[test]
    fn end_of_stream_is_noted_with_the_play_head_and_buffered_end() {
        let segs = make_segments(&[6_000, 6_000, 6_000]);
        let mut planner = Planner::new(segs.clone(), false, None, Config::default(), 0);
        drive(&mut planner, &segs, 1_000, 20, |_, i| {
            if i == 2 {
                1_000
            } else {
                6_000
            }
        });
        assert_eq!(
            planner.take_notes(),
            vec![Note::EndOfStream {
                play_ms: 1_000,
                buffered_end_ms: 13_000
            }]
        );
        assert_eq!(planner.take_notes(), Vec::new());
    }

    #[test]
    fn giving_up_on_a_segment_is_noted_once() {
        let segs = make_segments(&[6_000; 6]);
        let mut planner = Planner::new(segs.clone(), false, None, Config::default(), 0);
        drive(&mut planner, &segs, 10_000, 50, |_, i| {
            if i == 2 {
                0
            } else {
                6_000
            }
        });
        let gave_up: Vec<Note> = planner
            .take_notes()
            .into_iter()
            .filter(|n| n.label() == "gave_up")
            .collect();
        assert_eq!(
            gave_up,
            vec![Note::GaveUp {
                track: Track::Video,
                index: 2,
                appends: 2
            }]
        );
    }

    #[test]
    fn holding_a_track_back_is_noted_once_per_wait() {
        let segs = make_segments(&[6_000; 10]);
        let mut planner = Planner::new(
            segs.clone(),
            false,
            Some((segs, false)),
            Config::default(),
            0,
        );
        let mut buf = Buffered::default();
        planner.tick(0, &buf);
        planner.appended(Track::Audio);
        buf.audio.push((0, 6_000));
        planner.tick(0, &buf);
        planner.tick(0, &buf);
        assert_eq!(
            planner.take_notes(),
            vec![Note::Held {
                track: Track::Audio,
                lead_ms: 6_000,
                limit_ms: 2_000
            }]
        );
    }

    // B733: eviction leaves the audio track partly holding a segment; a seek
    // just short of the remaining data used to find the segment "buffered" by
    // its midpoint, fetch nothing, and stall the play.
    #[test]
    fn a_seek_short_of_a_partly_removed_segment_fetches_it_again_once() {
        let segs = make_segments(&[6_000; 20]);
        let mut planner = Planner::new(
            segs.clone(),
            false,
            Some((segs, false)),
            Config::default(),
            34_000,
        );
        let buf = Buffered {
            video: vec![(24_000, 60_000)],
            audio: vec![(19_000, 60_000)],
        };
        planner.seek(18_440, &buf);
        assert_eq!(
            planner.tick(18_440, &buf),
            vec![
                Action::Fetch {
                    track: Track::Video,
                    index: 3
                },
                Action::Fetch {
                    track: Track::Audio,
                    index: 3
                },
            ]
        );
        planner.appended(Track::Video);
        planner.appended(Track::Audio);

        // The segment's data still starts after the target: no second refetch.
        let still_short = Buffered {
            video: vec![(18_000, 60_000)],
            audio: vec![(18_500, 60_000)],
        };
        assert_eq!(planner.tick(18_440, &still_short), Vec::new());
    }

    #[test]
    fn eviction_removes_whole_segments() {
        let segs = make_segments(&[6_000; 10]);
        let mut planner = Planner::new(segs, false, None, Config::default(), 34_000);
        let buf = Buffered {
            video: vec![(0, 60_000)],
            audio: Vec::new(),
        };
        // Cutoff 34_000 - 15_000 = 19_000 lies in segment 3 (18_000..24_000).
        assert_eq!(
            planner.tick(34_000, &buf)[0],
            Action::Evict {
                track: Track::Video,
                start_ms: 0,
                end_ms: 18_000
            }
        );
    }

    #[test]
    fn track_labels_and_default_config() {
        assert_eq!(Track::Video.label(), "video");
        assert_eq!(Track::Audio.label(), "audio");
        assert_eq!(
            Config::default(),
            Config {
                forward_ms: 30_000,
                back_ms: 15_000,
                min_forward_ms: 8_000,
            }
        );
    }

    #[test]
    fn reinit_fetches_the_init_again_before_the_next_segment() {
        let segs = make_segments(&[6_000, 6_000, 6_000]);
        let mut planner = Planner::new(segs, true, None, Config::default(), 0);
        let mut buffered = Buffered::default();
        assert_eq!(
            planner.tick(0, &buffered),
            vec![Action::FetchInit {
                track: Track::Video
            }]
        );
        planner.appended(Track::Video);
        assert_eq!(
            planner.tick(0, &buffered),
            vec![Action::Fetch {
                track: Track::Video,
                index: 0
            }]
        );
        assert!(
            !planner.reinit(Track::Video),
            "refused with a segment in flight"
        );
        planner.appended(Track::Video);
        buffered.video = vec![(0, 6_000)];

        assert!(planner.reinit(Track::Video));
        assert_eq!(
            planner.tick(0, &buffered),
            vec![Action::FetchInit {
                track: Track::Video
            }]
        );
        planner.appended(Track::Video);
        assert_eq!(
            planner.tick(0, &buffered),
            vec![Action::Fetch {
                track: Track::Video,
                index: 1
            }],
            "the buffered segment stays; fetching carries on after it"
        );
    }

    #[test]
    fn reinit_is_refused_after_the_last_segment_and_for_a_missing_track() {
        let segs = make_segments(&[6_000]);
        let mut planner = Planner::new(segs, false, None, Config::default(), 0);
        let buffered = Buffered::default();
        assert!(!planner.reinit(Track::Audio), "no audio track");
        let _ = planner.tick(0, &buffered);
        planner.appended(Track::Video);
        assert!(!planner.reinit(Track::Video), "nothing left to fetch");
    }

    #[test]
    fn at_most_one_request_in_flight_per_track() {
        let segs = make_segments(&[6_000, 6_000, 6_000]);
        let mut planner = Planner::new(
            segs.clone(),
            false,
            Some((segs, false)),
            Config::default(),
            0,
        );
        let buf = Buffered::default();

        let first = planner.tick(0, &buf);
        assert_eq!(
            first,
            vec![
                Action::Fetch {
                    track: Track::Video,
                    index: 0
                },
                Action::Fetch {
                    track: Track::Audio,
                    index: 0
                },
            ]
        );
        assert_eq!(planner.in_flight(Track::Video), Some(Request::Media(0)));
        assert_eq!(planner.in_flight(Track::Audio), Some(Request::Media(0)));

        // Calling tick again while both are in flight emits nothing.
        assert_eq!(planner.tick(0, &buf), Vec::new());
    }

    #[test]
    fn init_fetched_once_before_media_and_not_refetched_after_seek() {
        let segs = make_segments(&[6_000, 6_000, 6_000]);
        let mut planner =
            Planner::new(segs.clone(), true, Some((segs, true)), Config::default(), 0);
        let mut buf = Buffered::default();

        assert_eq!(
            planner.tick(0, &buf),
            vec![
                Action::FetchInit {
                    track: Track::Video
                },
                Action::FetchInit {
                    track: Track::Audio
                },
            ]
        );
        assert_eq!(planner.in_flight(Track::Video), Some(Request::Init));
        assert_eq!(planner.in_flight(Track::Audio), Some(Request::Init));

        // Seeking while init is in flight does not abort init.
        assert_eq!(planner.seek(12_000, &buf), Vec::new());
        assert_eq!(planner.in_flight(Track::Video), Some(Request::Init));

        planner.appended(Track::Video);
        planner.appended(Track::Audio);

        // Next tick fetches segment 2 (at 12_000 ms), not FetchInit.
        assert_eq!(
            planner.tick(12_000, &buf),
            vec![
                Action::Fetch {
                    track: Track::Video,
                    index: 2
                },
                Action::Fetch {
                    track: Track::Audio,
                    index: 2
                },
            ]
        );

        planner.appended(Track::Video);
        planner.appended(Track::Audio);
        buf.video.push((12_000, 18_000));
        buf.audio.push((12_000, 18_000));

        // Seeking back to 0 fetches media segment 0 directly without re-fetching init.
        assert_eq!(planner.seek(0, &buf), Vec::new());
        assert_eq!(
            planner.tick(0, &buf),
            vec![
                Action::Fetch {
                    track: Track::Video,
                    index: 0
                },
                Action::Fetch {
                    track: Track::Audio,
                    index: 0
                },
            ]
        );
    }

    #[test]
    fn next_media_segment_skips_covered_midpoints_and_respects_forward_target() {
        let segs = make_segments(&[6_000, 6_000, 6_000, 6_000]);
        let config = Config {
            forward_ms: 12_000,
            back_ms: 15_000,
            min_forward_ms: 8_000,
        };
        let mut planner = Planner::new(segs, false, None, config, 0);

        // Segment 0 midpoint is 3_000; range (0, 4_000) covers its midpoint.
        let mut buf = Buffered {
            video: vec![(0, 4_000)],
            audio: Vec::new(),
        };
        assert_eq!(
            planner.tick(0, &buf),
            vec![Action::Fetch {
                track: Track::Video,
                index: 1
            }]
        );
        planner.appended(Track::Video);
        buf.video = vec![(0, 12_000)];

        // Segment 2 starts at 12_000, which is not < play_ms (0) + forward_ms (12_000).
        assert_eq!(planner.tick(0, &buf), Vec::new());

        // Advancing play_ms to 1 ms brings segment 2 (start 12_000 < 12_001) into range.
        assert_eq!(
            planner.tick(1, &buf),
            vec![Action::Fetch {
                track: Track::Video,
                index: 2
            }]
        );
    }

    #[test]
    fn lockstep_keeps_video_and_audio_within_the_lead_limit() {
        let segs = make_segments(&[6_000, 6_000, 6_000, 6_000]);
        let mut planner = Planner::new(
            segs.clone(),
            false,
            Some((segs, false)),
            Config::default(),
            0,
        );
        let mut buf = Buffered::default();

        // Both tracks start fetching segment 0.
        assert_eq!(
            planner.tick(0, &buf),
            vec![
                Action::Fetch {
                    track: Track::Video,
                    index: 0
                },
                Action::Fetch {
                    track: Track::Audio,
                    index: 0
                },
            ]
        );

        // Video finishes segment 0; audio is still in flight on segment 0.
        // Video segment 1 starts 6_000 past audio's end (0): over the limit.
        planner.appended(Track::Video);
        buf.video.push((0, 6_000));
        assert_eq!(planner.tick(0, &buf), Vec::new());

        // Once audio finishes segment 0, both may fetch segment 1.
        planner.appended(Track::Audio);
        buf.audio.push((0, 6_000));
        assert_eq!(
            planner.tick(0, &buf),
            vec![
                Action::Fetch {
                    track: Track::Video,
                    index: 1
                },
                Action::Fetch {
                    track: Track::Audio,
                    index: 1
                },
            ]
        );
    }

    #[test]
    fn evicts_behind_back_buffer_only_when_not_in_flight() {
        let segs = make_segments(&[6_000, 6_000, 6_000, 6_000, 6_000]);
        let mut planner = Planner::new(segs, false, None, Config::default(), 0);
        let buf = Buffered {
            video: vec![(0, 24_000)],
            audio: Vec::new(),
        };

        // At play_ms = 15_000, play_ms - back_ms = 0: nothing to evict; fetches seg 4.
        assert_eq!(
            planner.tick(15_000, &buf),
            vec![Action::Fetch {
                track: Track::Video,
                index: 4
            }]
        );

        // At play_ms = 22_000 while seg 4 is in flight: no Evict may be emitted.
        assert_eq!(planner.tick(22_000, &buf), Vec::new());

        // Once the in-flight request completes, the cutoff (7_000) is floored
        // to its segment's start: Evict [0, 6_000) removes whole segments only.
        planner.failed(Track::Video);
        assert_eq!(
            planner.tick(22_000, &buf),
            vec![
                Action::Evict {
                    track: Track::Video,
                    start_ms: 0,
                    end_ms: 6_000,
                },
                Action::Fetch {
                    track: Track::Video,
                    index: 4,
                },
            ]
        );
    }

    #[test]
    fn seek_aborts_non_matching_media_and_clamps_past_end() {
        let segs = make_segments(&[6_000, 6_000, 6_000]);
        let mut planner = Planner::new(
            segs.clone(),
            false,
            Some((segs, false)),
            Config::default(),
            0,
        );
        let buf = Buffered::default();

        planner.tick(0, &buf);
        // Video is on seg 0; advance video to seg 1 so video is on seg 1 and audio is on seg 0.
        planner.appended(Track::Video);
        // Audio's data stops short of the segment end but past its midpoint, so
        // it is within the lead limit of video's segment 1.
        let buf_v0 = Buffered {
            video: vec![(0, 6_000)],
            audio: vec![(0, 4_000)],
        };
        planner.tick(0, &buf_v0);
        assert_eq!(planner.in_flight(Track::Video), Some(Request::Media(1)));
        assert_eq!(planner.in_flight(Track::Audio), Some(Request::Media(0)));

        // Seek to 7_000 ms (held by seg 1): video keeps Media(1), audio aborts Media(0).
        assert_eq!(
            planner.seek(7_000, &buf_v0),
            vec![Action::Abort {
                track: Track::Audio
            }]
        );
        assert_eq!(planner.in_flight(Track::Video), Some(Request::Media(1)));
        assert_eq!(planner.in_flight(Track::Audio), None);

        // Next tick fetches seg 1 for audio while video still has seg 1 in flight.
        assert_eq!(
            planner.tick(7_000, &buf_v0),
            vec![Action::Fetch {
                track: Track::Audio,
                index: 1
            }]
        );

        // Seek past the end (50_000 ms > 18_000 ms) aborts both and clamps to last segment (2).
        assert_eq!(
            planner.seek(50_000, &buf_v0),
            vec![
                Action::Abort {
                    track: Track::Video
                },
                Action::Abort {
                    track: Track::Audio
                },
            ]
        );
        assert_eq!(
            planner.tick(50_000, &buf_v0),
            vec![
                // Back buffer is anchored at the clamped play head (17_999), not
                // 50_000; its cutoff (2_999) floors to segment 0's start, so nothing goes.
                Action::Fetch {
                    track: Track::Video,
                    index: 2
                },
                Action::Fetch {
                    track: Track::Audio,
                    index: 2
                },
            ]
        );
    }

    #[test]
    fn end_of_stream_emitted_once_and_rearmed_by_seek_before_end() {
        let segs = make_segments(&[5_000, 5_000]);
        let mut planner = Planner::new(segs, false, None, Config::default(), 0);
        let mut buf = Buffered::default();

        assert_eq!(
            planner.tick(0, &buf),
            vec![Action::Fetch {
                track: Track::Video,
                index: 0
            }]
        );
        planner.appended(Track::Video);
        buf.video = vec![(0, 5_000)];

        assert_eq!(
            planner.tick(0, &buf),
            vec![Action::Fetch {
                track: Track::Video,
                index: 1
            }]
        );
        planner.appended(Track::Video);
        buf.video = vec![(0, 10_000)];

        assert_eq!(planner.tick(0, &buf), vec![Action::EndOfStream]);
        assert_eq!(planner.tick(5_000, &buf), Vec::new());

        // Simulate evicting segment 0 and seeking back to 0: re-arms EndOfStream.
        buf.video = vec![(5_000, 10_000)];
        assert_eq!(planner.seek(0, &buf), Vec::new());
        assert_eq!(
            planner.tick(0, &buf),
            vec![Action::Fetch {
                track: Track::Video,
                index: 0
            }]
        );
        planner.appended(Track::Video);
        buf.video = vec![(0, 10_000)];

        assert_eq!(planner.tick(0, &buf), vec![Action::EndOfStream]);
        assert_eq!(planner.tick(0, &buf), Vec::new());
    }

    #[test]
    fn start_ms_positions_first_fetch_on_holding_segment() {
        let segs = make_segments(&[5_000, 5_000, 5_000, 5_000]);
        let mut planner = Planner::new(segs, true, None, Config::default(), 12_000);
        let buf = Buffered::default();

        assert_eq!(
            planner.tick(0, &buf),
            vec![Action::FetchInit {
                track: Track::Video
            }]
        );
        planner.appended(Track::Video);

        assert_eq!(
            planner.tick(0, &buf),
            vec![Action::Fetch {
                track: Track::Video,
                index: 2
            }]
        );
    }

    #[test]
    fn empty_segment_lists_emit_end_of_stream_after_init() {
        let mut with_init = Planner::new(
            Vec::new(),
            true,
            Some((Vec::new(), true)),
            Config::default(),
            0,
        );
        let buf = Buffered::default();

        assert_eq!(
            with_init.tick(0, &buf),
            vec![
                Action::FetchInit {
                    track: Track::Video
                },
                Action::FetchInit {
                    track: Track::Audio
                },
            ]
        );
        with_init.appended(Track::Video);
        assert_eq!(with_init.tick(0, &buf), Vec::new());
        with_init.appended(Track::Audio);
        assert_eq!(with_init.tick(0, &buf), vec![Action::EndOfStream]);
        assert_eq!(with_init.tick(0, &buf), Vec::new());

        let mut without_init = Planner::new(Vec::new(), false, None, Config::default(), 0);
        assert_eq!(without_init.tick(0, &buf), vec![Action::EndOfStream]);
        assert_eq!(without_init.tick(0, &buf), Vec::new());
    }

    #[test]
    fn quota_exceeded_halves_forward_target_down_to_min_forward_ms() {
        let segs = make_segments(&[6_000, 6_000]);
        let mut planner = Planner::new(segs, false, None, Config::default(), 0);
        let buf = Buffered::default();

        planner.tick(0, &buf);
        assert_eq!(planner.in_flight(Track::Video), Some(Request::Media(0)));

        assert_eq!(planner.quota_exceeded(Track::Video), 15_000);
        assert_eq!(planner.forward_target_ms(), 15_000);
        assert_eq!(planner.in_flight(Track::Video), None);

        assert_eq!(planner.quota_exceeded(Track::Video), 8_000);
        assert_eq!(planner.quota_exceeded(Track::Video), 8_000);
        assert_eq!(planner.forward_target_ms(), 8_000);
    }

    #[test]
    fn failed_request_is_retried_on_next_tick() {
        let segs = make_segments(&[6_000, 6_000]);
        let mut planner = Planner::new(segs, false, None, Config::default(), 0);
        let buf = Buffered::default();

        assert_eq!(
            planner.tick(0, &buf),
            vec![Action::Fetch {
                track: Track::Video,
                index: 0
            }]
        );
        planner.failed(Track::Video);
        assert_eq!(planner.in_flight(Track::Video), None);
        assert_eq!(
            planner.tick(0, &buf),
            vec![Action::Fetch {
                track: Track::Video,
                index: 0
            }]
        );
    }

    #[derive(Debug, Clone)]
    enum Event {
        Tick(f64),
        Seek(f64),
        Appended(Track),
        Failed(Track),
        Quota(Track),
    }

    fn event_strategy() -> impl Strategy<Value = Event> {
        prop_oneof![
            4 => (0.0f64..1.0f64).prop_map(Event::Tick),
            2 => (0.0f64..1.1f64).prop_map(Event::Seek),
            3 => prop_oneof![Just(Track::Video), Just(Track::Audio)].prop_map(Event::Appended),
            1 => prop_oneof![Just(Track::Video), Just(Track::Audio)].prop_map(Event::Failed),
            1 => prop_oneof![Just(Track::Video), Just(Track::Audio)].prop_map(Event::Quota),
        ]
    }

    proptest! {
        #[test]
        fn planner_state_machine_invariants_hold(
            seg_durations in prop::collection::vec(2_000u64..=8_000u64, 1..=200),
            video_init in any::<bool>(),
            has_audio in any::<bool>(),
            audio_init in any::<bool>(),
            start_frac in 0.0f64..1.0f64,
            events in prop::collection::vec(event_strategy(), 1..100),
        ) {
            let segments = make_segments(&seg_durations);
            let total_ms = segments_duration_ms(&segments);
            let start_ms = ((start_frac * (total_ms as f64)).floor() as u64)
                .min(total_ms.saturating_sub(1));
            let config = Config::default();

            let mut planner = Planner::new(
                segments.clone(),
                video_init,
                has_audio.then(|| (segments.clone(), audio_init)),
                config,
                start_ms,
            );

            let mut buffered = Buffered::default();
            let mut play_ms = start_ms;
            let mut shadow_video: Option<Request> = None;
            let mut shadow_audio: Option<Request> = None;
            let mut pending_seek_video: Option<usize> = None;
            let mut pending_seek_audio: Option<usize> = None;

            for event in events {
                match event {
                    Event::Tick(frac) => {
                        if pending_seek_video.is_none() && pending_seek_audio.is_none() {
                            play_ms = ((frac * (total_ms as f64)).floor() as u64)
                                .min(total_ms.saturating_sub(1));
                        }
                        let had_video = shadow_video;
                        let had_audio = shadow_audio;
                        let actions = planner.tick(play_ms, &buffered);

                        let mut video_fetches = 0usize;
                        let mut audio_fetches = 0usize;

                        for action in actions {
                            match action {
                                Action::FetchInit { track: Track::Video } => {
                                    prop_assert!(had_video.is_none());
                                    video_fetches += 1;
                                    shadow_video = Some(Request::Init);
                                }
                                Action::FetchInit { track: Track::Audio } => {
                                    prop_assert!(has_audio);
                                    prop_assert!(had_audio.is_none());
                                    audio_fetches += 1;
                                    shadow_audio = Some(Request::Init);
                                }
                                Action::Fetch { track: Track::Video, index } => {
                                    prop_assert!(had_video.is_none());
                                    prop_assert!(index < segments.len());
                                    video_fetches += 1;
                                    shadow_video = Some(Request::Media(index));
                                    if let Some(expected_idx) = pending_seek_video.take() {
                                        prop_assert_eq!(index, expected_idx);
                                    }
                                }
                                Action::Fetch { track: Track::Audio, index } => {
                                    prop_assert!(has_audio);
                                    prop_assert!(had_audio.is_none());
                                    prop_assert!(index < segments.len());
                                    audio_fetches += 1;
                                    shadow_audio = Some(Request::Media(index));
                                    if let Some(expected_idx) = pending_seek_audio.take() {
                                        prop_assert_eq!(index, expected_idx);
                                    }
                                }
                                Action::Evict { track, start_ms: ev_start, end_ms: ev_end } => {
                                    prop_assert!(ev_start < ev_end);
                                    prop_assert!(ev_end <= play_ms.saturating_sub(config.back_ms));
                                    match track {
                                        Track::Video => {
                                            prop_assert!(had_video.is_none());
                                            evict_range(&mut buffered.video, ev_start, ev_end);
                                        }
                                        Track::Audio => {
                                            prop_assert!(had_audio.is_none());
                                            evict_range(&mut buffered.audio, ev_start, ev_end);
                                        }
                                    }
                                }
                                Action::Abort { .. } => {
                                    prop_assert!(false, "tick must never emit Abort");
                                }
                                Action::EndOfStream => {
                                    prop_assert!(shadow_video.is_none());
                                    prop_assert!(shadow_audio.is_none());
                                }
                                Action::Skip { .. } => {
                                    prop_assert!(false, "no segment is given up in this model");
                                }
                            }
                        }

                        prop_assert!(video_fetches <= 1);
                        prop_assert!(audio_fetches <= 1);
                    }
                    Event::Seek(frac) => {
                        let to_ms = (frac * (total_ms as f64)).floor() as u64;
                        play_ms = to_ms;
                        let target_idx = super::clamped_segment_at(&segments, to_ms).unwrap();
                        let target_seg = &segments[target_idx];
                        let target_end = target_seg.start_ms.saturating_add(target_seg.duration_ms);

                        // Ensure the seek target is unbuffered so the seek-target fetch invariant is exercised.
                        evict_range(&mut buffered.video, target_seg.start_ms, target_end);
                        evict_range(&mut buffered.audio, target_seg.start_ms, target_end);

                        let actions = planner.seek(to_ms, &buffered);
                        for action in actions {
                            match action {
                                Action::Abort { track: Track::Video } => {
                                    prop_assert!(matches!(shadow_video, Some(Request::Media(i)) if i != target_idx));
                                    shadow_video = None;
                                }
                                Action::Abort { track: Track::Audio } => {
                                    prop_assert!(matches!(shadow_audio, Some(Request::Media(i)) if i != target_idx));
                                    shadow_audio = None;
                                }
                                other => prop_assert!(false, "unexpected action from seek: {other:?}"),
                            }
                        }

                        pending_seek_video = match shadow_video {
                            Some(Request::Media(i)) if i == target_idx => None,
                            _ => Some(target_idx),
                        };
                        pending_seek_audio = if has_audio {
                            match shadow_audio {
                                Some(Request::Media(i)) if i == target_idx => None,
                                _ => Some(target_idx),
                            }
                        } else {
                            None
                        };
                    }
                    Event::Appended(track) => {
                        match track {
                            Track::Video => {
                                if let Some(req) = shadow_video.take() {
                                    planner.appended(Track::Video);
                                    if let Request::Media(idx) = req {
                                        let seg = &segments[idx];
                                        merge_range(
                                            &mut buffered.video,
                                            seg.start_ms,
                                            seg.start_ms.saturating_add(seg.duration_ms),
                                        );
                                    }
                                }
                            }
                            Track::Audio if has_audio => {
                                if let Some(req) = shadow_audio.take() {
                                    planner.appended(Track::Audio);
                                    if let Request::Media(idx) = req {
                                        let seg = &segments[idx];
                                        merge_range(
                                            &mut buffered.audio,
                                            seg.start_ms,
                                            seg.start_ms.saturating_add(seg.duration_ms),
                                        );
                                    }
                                }
                            }
                            Track::Audio => {}
                        }
                    }
                    Event::Failed(track) => {
                        planner.failed(track);
                        match track {
                            Track::Video => shadow_video = None,
                            Track::Audio => shadow_audio = None,
                        }
                    }
                    Event::Quota(track) => {
                        let new_target = planner.quota_exceeded(track);
                        prop_assert!(new_target >= config.min_forward_ms);
                        match track {
                            Track::Video => shadow_video = None,
                            Track::Audio => shadow_audio = None,
                        }
                    }
                }

                prop_assert_eq!(planner.in_flight(Track::Video), shadow_video);
                prop_assert_eq!(planner.in_flight(Track::Audio), shadow_audio);
                prop_assert!(planner.forward_target_ms() >= config.min_forward_ms);
            }
        }
    }
}
