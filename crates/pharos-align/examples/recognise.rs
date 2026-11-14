//! Spec 042 slice 2 check: recognises saved vocals with the Rust recogniser,
//! so its words can be compared with the prototype's.
//!
//! usage: recognise MODEL IN.f32 OUT.json [IN.f32 OUT.json ...]
//!
//! - IN.f32: 16 kHz mono vocals, f32 little-endian.
//! - OUT.json: `{words: [{w, s, e, p}], segments: [{s, e, t}], retries:
//!   [{s, e, first, words, distinct, outcome}], sec}`, the words in the transcript
//!   format the `placement` example reads.
//!
//! Set `CPU` to decode without the GPU.

use std::path::Path;
use std::time::Instant;

use pharos_align::recognise::whisper::Recogniser;
use serde_json::json;

type Error = Box<dyn std::error::Error>;

const USAGE: &str = "usage: recognise MODEL IN.f32 OUT.json [IN.f32 OUT.json ...]";

fn read_f32(path: &str) -> Result<Vec<f32>, Error> {
    let b = std::fs::read(path).map_err(|e| format!("reading {path}: {e}"))?;
    Ok(b.chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect())
}

fn main() -> Result<(), Error> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (model, pairs) = args.split_first().ok_or(USAGE)?;
    if pairs.is_empty() || pairs.len() % 2 != 0 {
        return Err(USAGE.into());
    }
    let r = Recogniser::load(Path::new(model), std::env::var_os("CPU").is_none())?;
    for pair in pairs.chunks(2) {
        let [inp, outp] = pair else {
            return Err(USAGE.into());
        };
        let audio = read_f32(inp)?;
        let t = Instant::now();
        let heard = r.transcribe(&audio)?;
        let sec = t.elapsed().as_secs_f64();
        let words: Vec<_> = heard
            .transcript
            .words
            .iter()
            .map(|w| json!({"w": w.text, "s": w.start_s, "e": w.end_s, "p": w.prob}))
            .collect();
        let segments: Vec<_> = heard
            .segments
            .iter()
            .map(|s| json!({"s": s.start_s, "e": s.end_s, "t": s.text}))
            .collect();
        let retries: Vec<_> = heard
            .retries
            .iter()
            .map(|r| {
                json!({"s": r.start_s, "e": r.end_s, "first": r.first,
                    "words": r.words, "distinct": r.distinct, "outcome": r.outcome.label()})
            })
            .collect();
        let v = json!({"words": words, "segments": segments, "retries": retries, "sec": sec});
        std::fs::write(outp, v.to_string()).map_err(|e| format!("writing {outp}: {e}"))?;
        eprintln!(
            "{inp} {sec:.1}s {} words {} retries",
            heard.transcript.words.len(),
            heard.retries.len()
        );
    }
    Ok(())
}
