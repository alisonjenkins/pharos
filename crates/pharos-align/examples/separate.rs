//! Spec 042 slice 3 check: separates songs' vocals with the Rust htdemucs,
//! so its stems can be compared with demucs's.
//!
//! usage: separate IN OUT.f32 [IN OUT.f32 ...]
//!
//! - IN: any audio ffmpeg reads (`PHAROS_ALIGNER_FFMPEG`, else `ffmpeg`).
//! - OUT.f32: 44.1 kHz stereo, interleaved f32 little-endian: the vocals, or
//!   the mix when there is no GPU. Nothing is written when separation fails.
//!
//! Prints one JSON line per song: `{in, outcome, reason, sec}`. Set `CPU` to
//! take the no-GPU path, and `KERNEL_CACHE` to keep compiled kernels.

use std::path::Path;
use std::process::Command;

use pharos_align::device::{self, Device, DeviceMode};
use pharos_align::separate::htdemucs::{Separator, BPK};
use pharos_align::separate::{vocals_or_mix, Stereo, RATE};
use serde_json::json;

type Error = Box<dyn std::error::Error>;

const USAGE: &str = "usage: separate IN OUT.f32 [IN OUT.f32 ...]";

fn decode(ffmpeg: &str, path: &str) -> Result<Stereo, Error> {
    let out = Command::new(ffmpeg)
        .args([
            "-v", "error", "-nostdin", "-i", path, "-f", "f32le", "-ac", "2",
        ])
        .args(["-ar", &RATE.to_string(), "-"])
        .output()
        .map_err(|e| format!("running {ffmpeg}: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "decoding {path}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )
        .into());
    }
    let mut stereo: Stereo = [Vec::new(), Vec::new()];
    for frame in out.stdout.chunks_exact(8) {
        for (c, s) in stereo.iter_mut().zip(frame.chunks_exact(4)) {
            c.push(f32::from_le_bytes([s[0], s[1], s[2], s[3]]));
        }
    }
    Ok(stereo)
}

fn write(path: &str, audio: &Stereo) -> Result<(), Error> {
    let mut b = Vec::with_capacity(audio[0].len() * 8);
    for (l, r) in audio[0].iter().zip(&audio[1]) {
        b.extend_from_slice(&l.to_le_bytes());
        b.extend_from_slice(&r.to_le_bytes());
    }
    std::fs::write(path, b).map_err(|e| format!("writing {path}: {e}").into())
}

fn main() -> Result<(), Error> {
    let pairs: Vec<String> = std::env::args().skip(1).collect();
    if pairs.is_empty() || pairs.len() % 2 != 0 {
        return Err(USAGE.into());
    }
    let ffmpeg = std::env::var("PHAROS_ALIGNER_FFMPEG").unwrap_or_else(|_| "ffmpeg".into());
    let mode = if std::env::var_os("CPU").is_some() {
        DeviceMode::Cpu
    } else {
        DeviceMode::Auto
    };
    let choice = device::choose(mode, &device::probe())?;
    if let Some(dir) = std::env::var_os("KERNEL_CACHE") {
        std::fs::create_dir_all(&dir)?;
        pharos_align::forward::vulkan::use_kernel_cache(Path::new(&dir));
    }
    let separator = match &choice.device {
        Device::Vulkan(a) => Some(Separator::load(Path::new(BPK), a)?),
        Device::Cpu => None,
    };
    for pair in pairs.chunks(2) {
        let [inp, outp] = pair else {
            return Err(USAGE.into());
        };
        let mix = decode(&ffmpeg, inp)?;
        let (audio, s) = vocals_or_mix(mix, &choice, |_, mix| match &separator {
            Some(sep) => sep.vocals(mix),
            None => Err(pharos_align::separate::SeparateError::Model {
                path: BPK.into(),
                message: "not loaded".into(),
            }),
        });
        if let Some(audio) = &audio {
            write(outp, audio)?;
        }
        println!(
            "{}",
            json!({"in": inp, "outcome": s.outcome.label(), "reason": s.reason, "sec": s.seconds})
        );
    }
    Ok(())
}
