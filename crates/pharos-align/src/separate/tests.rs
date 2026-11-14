#![allow(clippy::unwrap_used)]

use super::*;

const PLANE: usize = BINS * FRAMES;

/// LCG noise plus a slow tone and a DC offset, so a shifted or mis-scaled
/// segment shows.
fn song(n: usize, seed: u64) -> Stereo {
    let mut state = seed;
    let mut next = move || {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (state >> 40) as f32 / (1u32 << 24) as f32 - 0.5
    };
    let mut left = Vec::with_capacity(n);
    let mut right = Vec::with_capacity(n);
    for i in 0..n {
        let tone = (2.0 * std::f32::consts::PI * 440.0 * i as f32 / RATE as f32).sin();
        left.push(0.1 + 0.3 * tone + 0.2 * next());
        right.push(0.1 - 0.2 * tone + 0.2 * next());
    }
    [left, right]
}

/// Tones only: the spectrogram branch loses each segment's edges, so the
/// noise in [`song`] would swamp a check of it.
fn tones(n: usize) -> Stereo {
    let t = |i: usize| i as f32 / RATE as f32;
    let tau = 2.0 * std::f32::consts::PI;
    [
        (0..n).map(|i| 0.4 * (tau * 440.0 * t(i)).sin()).collect(),
        (0..n).map(|i| 0.3 * (tau * 660.0 * t(i)).cos()).collect(),
    ]
}

/// A network that puts the segment in `source`'s time branch and nothing
/// in its spectrogram branch.
fn time_into(source: usize) -> impl FnMut(&[f32], &[f32]) -> Result<Output, String> {
    move |mix, _| {
        let mut xt = vec![0.0; SOURCES * CHANNELS * SEGMENT];
        xt[source * CHANNELS * SEGMENT..(source + 1) * CHANNELS * SEGMENT].copy_from_slice(mix);
        Ok((vec![0.0; SOURCES * 2 * CHANNELS * PLANE], xt))
    }
}

/// A network that puts the segment's spectrogram in `source`'s
/// spectrogram branch and nothing in its time branch.
fn spec_into(source: usize) -> impl FnMut(&[f32], &[f32]) -> Result<Output, String> {
    move |_, mag| {
        let mut x = vec![0.0; SOURCES * 2 * CHANNELS * PLANE];
        x[source * 2 * CHANNELS * PLANE..(source + 1) * 2 * CHANNELS * PLANE].copy_from_slice(mag);
        Ok((x, vec![0.0; SOURCES * CHANNELS * SEGMENT]))
    }
}

fn max_diff(a: &[f32], b: &[f32]) -> f32 {
    assert_eq!(a.len(), b.len());
    a.iter()
        .zip(b)
        .map(|(x, y)| (x - y).abs())
        .fold(0.0, f32::max)
}

#[test]
fn segments_blend_back_into_the_song() {
    // Three segments, the last one short.
    let mix = song(2 * SEGMENT + 12_345, 3);
    let mut calls = 0;
    let mut net = time_into(VOCALS);
    let out = vocals(&mix, |m, s| {
        calls += 1;
        net(m, s)
    })
    .unwrap();
    assert_eq!(calls, 3);
    for (o, m) in out.iter().zip(&mix) {
        assert!(max_diff(o, m) < 1e-5, "{}", max_diff(o, m));
    }
}

#[test]
fn a_song_shorter_than_a_segment_is_padded_with_silence() {
    let mix = song(100_000, 5);
    let out = vocals(&mix, time_into(VOCALS)).unwrap();
    for (o, m) in out.iter().zip(&mix) {
        assert!(max_diff(o, m) < 1e-5, "{}", max_diff(o, m));
    }
}

#[test]
fn the_spectrogram_branch_is_inverted_and_added() {
    let mix = tones(SEGMENT + 50_000);
    let out = vocals(&mix, spec_into(VOCALS)).unwrap();
    // The song's own first and last 1.5 hops have no segment overlapping
    // them, and come back quieter as in demucs.
    let edge = 2 * stft::HOP;
    let n = mix[0].len();
    for (o, m) in out.iter().zip(&mix) {
        let d = max_diff(&o[edge..n - edge], &m[edge..n - edge]);
        assert!(d < 1e-2, "{d}");
    }
}

#[test]
fn only_the_vocals_are_kept() {
    let mix = tones(SEGMENT);
    let (mean, _) = stats(&mix);
    for source in (0..SOURCES).filter(|&s| s != VOCALS) {
        for out in [
            vocals(&mix, time_into(source)).unwrap(),
            vocals(&mix, spec_into(source)).unwrap(),
        ] {
            assert!(
                out.iter().flatten().all(|v| (v - mean).abs() < 1e-6),
                "source {source}"
            );
        }
    }
}

#[test]
fn an_empty_song_has_empty_vocals() {
    let out = vocals(&[Vec::new(), Vec::new()], |_, _| Err("called".into())).unwrap();
    assert!(out.iter().all(Vec::is_empty));
}

#[test]
fn channels_of_different_lengths_are_refused() {
    let err = vocals(&[vec![0.0; 10], vec![0.0; 9]], time_into(VOCALS)).unwrap_err();
    assert!(matches!(
        err,
        SeparateError::Channels { left: 10, right: 9 }
    ));
}

#[test]
fn a_network_of_the_wrong_shape_is_refused() {
    let err = vocals(&song(SEGMENT, 1), |_, _| Ok((vec![0.0; 3], vec![0.0; 4]))).unwrap_err();
    assert!(
        matches!(
            err,
            SeparateError::Shape {
                what: "x",
                len: 3,
                ..
            }
        ),
        "{err}"
    );
}

#[test]
fn a_network_error_names_its_segment() {
    let err = vocals(&song(SEGMENT + 1, 1), |_, _| Err("lost the device".into())).unwrap_err();
    assert_eq!(
        err.to_string(),
        "running the network on the segment at sample 0: lost the device"
    );
}

#[test]
fn a_short_last_segment_is_centred_on_its_audio() {
    let n = SEGMENT + 100_000;
    let mix = song(n, 9);
    let (mean, std) = stats(&mix);
    let mut last = Vec::new();
    let mut net = time_into(VOCALS);
    vocals(&mix, |m, s| {
        last = m.to_vec();
        net(m, s)
    })
    .unwrap();
    // demucs's TensorChunk.padded: the 185,995 samples left after the
    // second offset sit in the middle, real audio before them, silence after.
    let offset = STRIDE;
    let lead = (SEGMENT - (n - offset)) / 2;
    let want: Vec<f32> = (0..SEGMENT)
        .map(|i| {
            mix[0]
                .get(offset - lead + i)
                .map_or(0.0, |v| (v - mean) / std)
        })
        .collect();
    assert_eq!(&last[..SEGMENT], &want[..]);
}
