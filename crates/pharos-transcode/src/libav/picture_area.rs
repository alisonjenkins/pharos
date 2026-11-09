//! In-process black-bar check of one video file.
//!
//! On the persistent libav worker (V6: a decoder fault must kill a worker, not
//! the server): seek to [`SAMPLE_PERCENTS`] of the duration, decode the first
//! keyframe at each, take its luma plane at 8 bits, and hand the planes to the
//! pure [`crate::picture_area::analyse`]. Seven keyframes are read, not the
//! file, so it is bounded by seeks rather than by length.

use super::frames::{filter_video_seeked, FrameError};
use crate::picture_area::{analyse, LumaPlane, PictureAnalysis};
use ffmpeg::format::{self, Pixel};
use ffmpeg_the_third as ffmpeg;
use std::path::Path;

/// Where to sample, as a percentage of the duration. Starts past the first 5%
/// (logos, cold opens) and stops before the credits.
const SAMPLE_PERCENTS: [u64; 7] = [10, 20, 30, 40, 50, 60, 70];

/// Keep only the luma plane. `extractplanes` does not touch the values, where
/// converting a limited-range frame to gray would stretch them; a deeper
/// source reaches the 8-bit sink by the plain depth reduction.
const LUMA_FILTER: &str = "extractplanes=y";

/// `duration_ms` is the runtime the scan read, so the file is opened once;
/// with 0 the container is opened first to read its own.
pub fn picture_area(src: &Path, duration_ms: u64) -> Result<PictureAnalysis, FrameError> {
    crate::libav::init().map_err(|e| FrameError::Other(format!("libav init: {e}")))?;
    // The sampler's own open calls every failure the file's fault, which
    // would store a passing NFS error as "undecodable". A file that cannot
    // even be stat'ed says nothing about its picture: `Other`, retried.
    std::fs::metadata(src).map_err(|e| FrameError::Other(format!("stat: {e}")))?;
    let duration_ms = if duration_ms > 0 {
        duration_ms
    } else {
        container_duration_ms(src)?
    };
    if duration_ms == 0 {
        return Ok(PictureAnalysis::nothing_decoded());
    }
    let targets = SAMPLE_PERCENTS
        .iter()
        .map(|p| duration_ms.saturating_mul(*p) / 100);

    let mut planes: Vec<LumaPlane> = Vec::with_capacity(SAMPLE_PERCENTS.len());
    filter_video_seeked(src, targets, LUMA_FILTER, Pixel::GRAY8, |f| {
        planes.push(LumaPlane {
            width: f.width(),
            height: f.height(),
            stride: f.stride(0),
            data: f.data(0).to_vec(),
        });
        Ok(true)
    })?;
    sampled_all(planes.len())?;
    Ok(analyse(&planes))
}

/// Every sample point inside the runtime lands on a keyframe, so fewer
/// frames than points means a seek or a read stopped the sampler early: it
/// keeps what it has, which would store a verdict from a partial sample. Not
/// the file's fault as far as anyone can tell: `Other`, retried next pass.
fn sampled_all(frames: usize) -> Result<(), FrameError> {
    if frames < SAMPLE_PERCENTS.len() {
        return Err(FrameError::Other(format!(
            "sampled {frames} of {} frames: a seek or read stopped early",
            SAMPLE_PERCENTS.len()
        )));
    }
    Ok(())
}

/// The container's own runtime, for a file the scan has no runtime for.
fn container_duration_ms(src: &Path) -> Result<u64, FrameError> {
    // Only a container libav cannot make sense of is the file's fault; an I/O
    // error says nothing about the file, so it stays `Other` and is retried.
    let duration_us = {
        let ictx = format::input(src).map_err(|e| match e {
            ffmpeg::Error::InvalidData
            | ffmpeg::Error::DemuxerNotFound
            | ffmpeg::Error::StreamNotFound => FrameError::BadInput(format!("open: {e}")),
            other => FrameError::Other(format!("open: {other}")),
        })?;
        ictx.duration()
    };
    Ok(u64::try_from(duration_us / 1000).unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sample_cut_short_is_retried_not_judged() {
        assert!(sampled_all(SAMPLE_PERCENTS.len()).is_ok());
        for short in [0, 1, SAMPLE_PERCENTS.len() - 1] {
            assert!(
                matches!(sampled_all(short), Err(FrameError::Other(_))),
                "{short} frames"
            );
        }
    }
}
