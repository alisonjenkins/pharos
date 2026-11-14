//! A whole song's emission from the letter model (spec 042 stage 5): the
//! song cut into 30 s passes, each heard with a second either side, and the
//! frames each keeps joined. One pass over a whole song would need memory
//! for its every frame at once.

use std::ops::Range;

use crate::forward::cpu::FE_HOP;
use crate::tokens::CLASSES;

/// Samples of one forward pass: 30 s, about as long as the widest offset
/// probe, so a pass costs no more memory than the song's other windows.
pub const CHUNK_SAMPLES: usize = 30 * 16_000;

/// Audio either side of a chunk the model hears but whose frames are
/// dropped, so a word across a chunk boundary is heard whole: 1 s.
pub const CONTEXT_SAMPLES: usize = 16_000;

/// One forward pass of a long window: the samples it hears (`input`) and
/// those whose frames it keeps (`keep`). The keeps tile the window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    pub input: Range<usize>,
    pub keep: Range<usize>,
}

/// `n` samples cut into [`CHUNK_SAMPLES`] keeps, each heard with up to
/// [`CONTEXT_SAMPLES`] either side.
pub fn chunks(n: usize) -> Vec<Chunk> {
    (0..n)
        .step_by(CHUNK_SAMPLES)
        .map(|start| {
            let end = (start + CHUNK_SAMPLES).min(n);
            Chunk {
                input: start.saturating_sub(CONTEXT_SAMPLES)..(end + CONTEXT_SAMPLES).min(n),
                keep: start..end,
            }
        })
        .collect()
}

/// The emission of `pcm`, `[frames x CLASSES]`, run through `forward` a
/// chunk at a time and the kept frames joined. Chunks start on a frame
/// boundary, so a chunk's frame `f` is the whole window's frame
/// `input.start / FE_HOP + f`; it belongs to the chunk whose keep holds its
/// first sample, and the last chunk keeps every frame to the end. `None` as
/// soon as `forward` gives none.
pub fn stitched<E>(
    pcm: &[f32],
    mut forward: impl FnMut(&[f32]) -> Result<Option<Vec<f32>>, E>,
) -> Result<Option<Vec<f32>>, E> {
    let mut out = Vec::new();
    for chunk in chunks(pcm.len()) {
        let Some(input) = pcm.get(chunk.input.clone()) else {
            continue;
        };
        let Some(rows) = forward(input)? else {
            return Ok(None);
        };
        let first = chunk.input.start / FE_HOP;
        let last = chunk.keep.end == pcm.len();
        for (f, row) in rows.chunks_exact(CLASSES).enumerate() {
            let at = (first + f) * FE_HOP;
            if at >= chunk.keep.start && (last || at < chunk.keep.end) {
                out.extend_from_slice(row);
            }
        }
    }
    Ok(Some(out))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::forward::cpu::CpuModel;

    #[test]
    fn chunks_tile_the_window_and_hear_context_inside_it() {
        assert!(chunks(0).is_empty());
        let n = CHUNK_SAMPLES * 2 + CHUNK_SAMPLES / 2;
        let got = chunks(n);
        assert_eq!(got.len(), 3);
        assert_eq!(got[0].keep, 0..CHUNK_SAMPLES);
        assert_eq!(got[0].input, 0..CHUNK_SAMPLES + CONTEXT_SAMPLES);
        assert_eq!(
            got[1].input,
            CHUNK_SAMPLES - CONTEXT_SAMPLES..2 * CHUNK_SAMPLES + CONTEXT_SAMPLES
        );
        assert_eq!(got[2].keep, 2 * CHUNK_SAMPLES..n);
        assert_eq!(got[2].input, 2 * CHUNK_SAMPLES - CONTEXT_SAMPLES..n);
        for pair in got.windows(2) {
            assert_eq!(pair[0].keep.end, pair[1].keep.start);
        }
        assert_eq!(
            chunks(100),
            vec![Chunk {
                input: 0..100,
                keep: 0..100
            }]
        );
    }

    /// A fake model that makes as many frames as the real extractor, each
    /// row holding its frame's index in the whole window: stitched, they
    /// count up once, to the frames the whole window would make.
    #[test]
    fn stitched_frames_follow_on_without_gaps_or_repeats() {
        let n = CHUNK_SAMPLES * 2 + FE_HOP * 40 + 123;
        let pcm: Vec<f32> = (0..n).map(|i| i as f32).collect();
        let got = stitched(&pcm, |input: &[f32]| {
            let first = input[0] as usize / FE_HOP;
            let rows: Vec<f32> = (0..CpuModel::frames(input.len()))
                .flat_map(|f| [(first + f) as f32; CLASSES])
                .collect();
            Ok::<_, ()>(Some(rows))
        })
        .unwrap()
        .unwrap();
        let frames: Vec<usize> = got.chunks_exact(CLASSES).map(|r| r[0] as usize).collect();
        assert_eq!(frames, (0..CpuModel::frames(n)).collect::<Vec<_>>());
        let none = stitched(&pcm, |_: &[f32]| Ok::<_, ()>(None)).unwrap();
        assert_eq!(none, None, "a model with no emission gives none");
        let err = stitched(&pcm, |_: &[f32]| Err::<Option<Vec<f32>>, _>("lost"));
        assert_eq!(err, Err("lost"));
    }
}
