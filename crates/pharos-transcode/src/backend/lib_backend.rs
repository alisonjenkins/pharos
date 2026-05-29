//! P49-P51 — `LibBackend`. ffmpeg-next-backed implementation.
//!
//! P48 ships only the type + every method returning
//! `BackendError::NotImplemented`. Per-operation FFI lands in
//! follow-up bundles so the trait + spawn fallback can be merged
//! and CI-tested without bringing in the libav* link surface.

use crate::backend::{BackendError, FfmpegBackend, ProbeJson, SubtitleFormat, WaveformPoint};
use crate::{TranscodeOptions, TranscodeStream};
use std::future::Future;
use std::path::Path;
use std::pin::Pin;

#[derive(Debug, Default, Clone)]
pub struct LibBackend;

impl LibBackend {
    pub fn new() -> Self {
        Self
    }
}

impl FfmpegBackend for LibBackend {
    fn probe<'a>(
        &'a self,
        _path: &'a Path,
    ) -> Pin<Box<dyn Future<Output = Result<ProbeJson, BackendError>> + Send + 'a>> {
        Box::pin(async { Err(BackendError::NotImplemented) })
    }

    fn extract_image<'a>(
        &'a self,
        _src: &'a Path,
        _seek_ms: u64,
        _width: u32,
        _out: &'a Path,
    ) -> Pin<Box<dyn Future<Output = Result<(), BackendError>> + Send + 'a>> {
        Box::pin(async { Err(BackendError::NotImplemented) })
    }

    fn extract_subtitle<'a>(
        &'a self,
        _src: &'a Path,
        _stream_idx: u32,
        _format: SubtitleFormat,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<u8>, BackendError>> + Send + 'a>> {
        Box::pin(async { Err(BackendError::NotImplemented) })
    }

    fn convert_srt_to_webvtt<'a>(
        &'a self,
        _src: &'a Path,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<u8>, BackendError>> + Send + 'a>> {
        Box::pin(async { Err(BackendError::NotImplemented) })
    }

    fn transcode_image<'a>(
        &'a self,
        _src: &'a Path,
        _target_ext: &'a str,
        _out: &'a Path,
    ) -> Pin<Box<dyn Future<Output = Result<(), BackendError>> + Send + 'a>> {
        Box::pin(async { Err(BackendError::NotImplemented) })
    }

    fn waveform_rms<'a>(
        &'a self,
        _src: &'a Path,
        _samples_per_bin: u64,
        _target_bins: u32,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<WaveformPoint>, BackendError>> + Send + 'a>> {
        Box::pin(async { Err(BackendError::NotImplemented) })
    }

    fn transcode_stream<'a>(
        &'a self,
        _src: &'a Path,
        _opts: &'a TranscodeOptions,
    ) -> Pin<Box<dyn Future<Output = Result<TranscodeStream, BackendError>> + Send + 'a>> {
        Box::pin(async { Err(BackendError::NotImplemented) })
    }
}
