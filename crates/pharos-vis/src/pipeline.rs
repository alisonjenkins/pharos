//! Decoder, analyser and ring as one: makes the frames ahead of a playhead
//! and moves on a seek. No threads here; the engine drives it.

use std::sync::{Arc, Mutex, PoisonError};

use crate::decode::{DecodeError, Decoder, Step};
use crate::frame::{FFT_SIZE, FPS};
use crate::ring::FrameRing;
use crate::spectrum::Analyser;

pub type SharedRing = Arc<Mutex<FrameRing>>;

pub fn lock(ring: &SharedRing) -> std::sync::MutexGuard<'_, FrameRing> {
    ring.lock().unwrap_or_else(PoisonError::into_inner)
}

pub struct Pipeline {
    decoder: Decoder,
    analyser: Analyser,
    ring: SharedRing,
    /// Mono samples decoded and not yet behind every future window.
    samples: Vec<f32>,
    /// The mono sample index of `samples[0]`.
    samples_from: u64,
    /// The next frame to make.
    next_frame: u64,
    ended: bool,
}

impl Pipeline {
    pub fn new(decoder: Decoder, ring: SharedRing) -> Self {
        Self {
            decoder,
            analyser: Analyser::new(),
            ring,
            samples: Vec::new(),
            samples_from: 0,
            next_frame: 0,
            ended: false,
        }
    }

    pub fn sample_rate(&self) -> u32 {
        self.decoder.sample_rate()
    }

    pub fn codec(&self) -> &'static str {
        self.decoder.codec()
    }

    pub fn next_frame(&self) -> u64 {
        self.next_frame
    }

    /// Whether the song has no more frames to make.
    pub fn ended(&self) -> bool {
        self.ended
    }

    /// The sample index frame `k`'s window ends at.
    fn window_end(&self, k: u64) -> u64 {
        k.saturating_mul(u64::from(self.decoder.sample_rate())) / FPS
    }

    /// Make frames until `ahead` of `playhead` are held or the song ends.
    /// Frames the playhead already passed are stepped over, not analysed.
    pub fn fill(&mut self, playhead: u64, ahead: u64) -> Result<(), DecodeError> {
        lock(&self.ring).discard_before(playhead);
        while self.next_frame <= playhead.saturating_add(ahead) {
            let end = self.window_end(self.next_frame);
            while self.samples_from + (self.samples.len() as u64) < end && !self.ended {
                if self.decoder.read(&mut self.samples)? == Step::End {
                    self.ended = true;
                }
            }
            if self.samples_from + (self.samples.len() as u64) < end {
                break;
            }
            if self.next_frame >= playhead {
                let from = end.saturating_sub(FFT_SIZE as u64).max(self.samples_from);
                let window = self
                    .samples
                    .get(usize::try_from(from - self.samples_from).unwrap_or(0)..)
                    .and_then(|s| s.get(..usize::try_from(end - from).unwrap_or(0)))
                    .unwrap_or_default();
                let frame = self.analyser.frame(window);
                lock(&self.ring).push(self.next_frame, frame);
            }
            self.next_frame += 1;
            self.trim();
        }
        Ok(())
    }

    /// Forget the samples no later window reaches back to.
    fn trim(&mut self) {
        let keep_from = self
            .window_end(self.next_frame)
            .saturating_sub(FFT_SIZE as u64);
        let drop = usize::try_from(keep_from.saturating_sub(self.samples_from))
            .unwrap_or(usize::MAX)
            .min(self.samples.len());
        self.samples.drain(..drop);
        self.samples_from += drop as u64;
    }

    /// Continue from `ms`: the ring is emptied and the next frame is the one
    /// at `ms`.
    pub fn seek(&mut self, ms: u64) -> Result<(), DecodeError> {
        self.decoder.seek(ms)?;
        self.samples.clear();
        self.samples_from = self.decoder.position();
        self.ended = false;
        self.next_frame = ms.saturating_mul(FPS) / 1000;
        lock(&self.ring).clear();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;
    use crate::fixture;
    use crate::frame::{dequantise_db, BINS};
    use crate::ring::{AHEAD_FRAMES, RING_CAPACITY};

    const RATE: u32 = 16_000;

    /// 1 kHz for the first second, 3 kHz for the next two: bins 128 and 384.
    fn two_tones() -> Vec<i16> {
        let mut s = fixture::sine(1000.0, RATE, 1.0, 0.5);
        s.extend(fixture::sine(3000.0, RATE, 2.0, 0.5));
        s
    }

    fn pipeline() -> (Pipeline, SharedRing) {
        let bytes = fixture::wav(&two_tones(), RATE, 1);
        let dec = Decoder::open(Box::new(Cursor::new(bytes)), Some("wav")).unwrap();
        let ring: SharedRing = Arc::default();
        (Pipeline::new(dec, ring.clone()), ring)
    }

    fn peak(ring: &SharedRing, k: u64) -> usize {
        let r = lock(ring);
        let f = r.get(k).unwrap();
        (0..BINS).min_by_key(|&i| f.bins[i]).unwrap()
    }

    #[test]
    fn it_makes_two_seconds_ahead_and_no_more() {
        let (mut p, ring) = pipeline();
        p.fill(0, AHEAD_FRAMES).unwrap();
        let r = lock(&ring);
        assert_eq!(r.next_index(), AHEAD_FRAMES + 1);
        assert!(r.len() <= RING_CAPACITY);
    }

    #[test]
    fn frames_carry_the_sound_at_their_own_time() {
        let (mut p, ring) = pipeline();
        p.fill(15, 30).unwrap();
        assert!(
            (126..=130).contains(&peak(&ring, 15)),
            "0.5 s is the 1 kHz tone"
        );
        let db = dequantise_db(lock(&ring).get(15).unwrap().bins[128]);
        assert!(
            db > -20.0 && db < -15.0,
            "a 0.5 sine sits near -19.6 dBFS: {db}"
        );
        p.fill(75, 10).unwrap();
        assert!(
            (382..=386).contains(&peak(&ring, 75)),
            "2.5 s is the 3 kHz tone"
        );
    }

    #[test]
    fn a_seek_empties_the_ring_and_continues_from_there() {
        let (mut p, ring) = pipeline();
        p.fill(0, AHEAD_FRAMES).unwrap();
        p.seek(2000).unwrap();
        assert!(lock(&ring).is_empty());
        assert_eq!(p.next_frame(), 60);
        p.fill(60, 10).unwrap();
        let r = lock(&ring);
        assert!(r.get(59).is_none() && r.get(60).is_some());
        assert_eq!(r.next_index(), 71);
        drop(r);
        assert!((382..=386).contains(&peak(&ring, 65)));
    }

    #[test]
    fn frames_the_playhead_passed_are_stepped_over() {
        let (mut p, ring) = pipeline();
        p.fill(45, 5).unwrap();
        let r = lock(&ring);
        assert!(r.get(44).is_none());
        assert!(r.get(45).is_some() && r.get(50).is_some());
        assert_eq!(r.len(), 6);
    }

    #[test]
    fn the_song_ending_stops_the_frames_without_an_error() {
        let (mut p, ring) = pipeline();
        p.fill(80, 1000).unwrap();
        assert!(p.ended());
        let last = lock(&ring).next_index();
        assert!((90..=91).contains(&last), "3 s is 90 frames, got {last}");
    }

    #[test]
    fn the_sample_buffer_stays_small_over_a_whole_song() {
        let (mut p, _ring) = pipeline();
        p.fill(0, 1000).unwrap();
        assert!(p.samples.len() < 2 * FFT_SIZE + 4096, "{}", p.samples.len());
    }
}
