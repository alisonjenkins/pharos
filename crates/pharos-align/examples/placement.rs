//! Spec 042 slice 1 check: places lyric lines from a saved transcript and
//! a saved emission, so the Rust port can be compared with the prototype.
//!
//! usage: placement REQ.json TRANSCRIPT.json EMISSION TODAY.json [--no-cap]
//!
//! - REQ.json: a list of `{start_ms, end_ms, text}`.
//! - TRANSCRIPT.json: the recogniser's `{"words": [{w, s, e, p}]}`.
//! - EMISSION: `EMISSION.em` (f32 little-endian, frames x classes) beside
//!   `EMISSION.meta.json` (`{frames, n_samples, lines: [{i, words}]}`, the
//!   label ids of each line's words, checked against our own tokenizer).
//! - TODAY.json: today's answer, last line `{lines: [{i, score, words:
//!   [start_ms.., end_ms]}]}`.
//!
//! Prints `{share, fallback, lines: [{i, method, words, anchor}]}`.

use pharos_align::anchor::{match_lines, Options, Pass};
use pharos_align::ctc::Emission;
use pharos_align::plan::{Line, Stamp};
use pharos_align::refine::{place, Song};
use pharos_align::service::LineWords;
use pharos_align::text;
use pharos_align::tokens::{tokenize, CLASSES};
use pharos_align::transcript::Transcript;
use serde_json::{json, Value};

type Error = Box<dyn std::error::Error>;

const USAGE: &str = "usage: placement REQ.json TRANSCRIPT.json EMISSION TODAY.json [--no-cap]";

fn read_json(path: &str) -> Result<Value, Error> {
    let s = std::fs::read_to_string(path).map_err(|e| format!("reading {path}: {e}"))?;
    Ok(serde_json::from_str(&s).map_err(|e| format!("parsing {path}: {e}"))?)
}

fn lines(req: &Value) -> Result<Vec<Line>, Error> {
    Ok(req
        .as_array()
        .ok_or("request is not a list")?
        .iter()
        .map(|l| Line {
            start_ms: l["start_ms"].as_i64().unwrap_or_default(),
            end_ms: l["end_ms"].as_i64().unwrap_or_default(),
            text: l["text"].as_str().unwrap_or_default().to_owned(),
        })
        .collect())
}

/// Today's words per line; a line today left out has no words.
fn today(lines: &[Line], path: &str) -> Result<Vec<LineWords>, Error> {
    let s = std::fs::read_to_string(path).map_err(|e| format!("reading {path}: {e}"))?;
    let last = s.lines().last().ok_or("today's answer is empty")?;
    let v: Value = serde_json::from_str(last)?;
    let mut out = vec![
        LineWords {
            words: Vec::new(),
            score: None,
        };
        lines.len()
    ];
    for l in v["lines"].as_array().ok_or("today has no lines")? {
        let i = l["i"].as_u64().ok_or("today's line has no i")? as usize;
        let line = lines.get(i).ok_or("today's line index past the request")?;
        let ats: Vec<usize> = text::words(&line.text)
            .iter()
            .map(|w| w.at)
            .chain([line.text.chars().count()])
            .collect();
        let starts: Vec<i64> = l["words"]
            .as_array()
            .ok_or("today's line has no words")?
            .iter()
            .filter_map(Value::as_i64)
            .collect();
        if starts.is_empty() {
            continue;
        }
        if starts.len() != ats.len() {
            return Err(format!(
                "line {i}: today has {} times for {} words",
                starts.len(),
                ats.len()
            )
            .into());
        }
        out[i] = LineWords {
            words: ats
                .into_iter()
                .zip(starts)
                .map(|(at, start_ms)| Stamp { at, start_ms })
                .collect(),
            score: l["score"].as_f64(),
        };
    }
    Ok(out)
}

fn emission(prefix: &str, lines: &[Line]) -> Result<(Vec<f32>, usize, usize), Error> {
    let meta = read_json(&format!("{prefix}.meta.json"))?;
    let frames = meta["frames"].as_u64().ok_or("meta has no frames")? as usize;
    let n_samples = meta["n_samples"].as_u64().ok_or("meta has no n_samples")? as usize;
    for m in meta["lines"].as_array().ok_or("meta has no lines")? {
        let i = m["i"].as_u64().ok_or("meta line has no i")? as usize;
        let ours = tokenize(
            &text::words(&lines[i].text)
                .iter()
                .map(|w| w.token.clone())
                .collect::<Vec<_>>(),
        )?;
        let theirs: Vec<Vec<u32>> = serde_json::from_value(m["words"].clone())?;
        if ours != theirs {
            return Err(
                format!("line {i}: our tokens {ours:?} differ from the dump's {theirs:?}").into(),
            );
        }
    }
    let bytes = std::fs::read(format!("{prefix}.em"))?;
    let data: Vec<f32> = bytes
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect();
    Ok((data, frames, n_samples))
}

fn main() -> Result<(), Error> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [req, tr, em, td, rest @ ..] = args.as_slice() else {
        return Err(USAGE.into());
    };
    let opts = Options {
        slot_cap: !rest.iter().any(|a| a == "--no-cap"),
    };
    let lines = lines(&read_json(req)?)?;
    let t = Transcript::read(std::path::Path::new(tr))?;
    let m = match_lines(&lines, &t.words, opts);
    let today = today(&lines, td)?;
    let (data, frames, n_samples) = emission(em, &lines)?;
    let song = Song {
        emission: Emission::new(&data, frames, CLASSES)?,
        n_samples,
    };
    let r = place(&lines, &today, &m, &song);
    let out: Vec<Value> = r
        .lines
        .iter()
        .enumerate()
        .map(|(i, l)| {
            let anchor = m.lines[i].anchor.as_ref().map(|a| {
                json!({"words": a.starts_ms, "end_ms": a.end_ms,
                    "soft": match a.pass { Pass::Letters { ratio } => Some(ratio), Pass::Words => None }})
            });
            json!({
                "i": i,
                "method": r.methods[i].map(|m| m.label()),
                "words": l.words.iter().map(|s| s.start_ms).collect::<Vec<_>>(),
                "score": l.score,
                "anchor": anchor,
            })
        })
        .collect();
    println!(
        "{}",
        json!({"share": m.share(), "fallback": r.placement.fallback_label(), "lines": out})
    );
    Ok(())
}
