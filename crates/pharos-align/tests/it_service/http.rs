use std::net::SocketAddr;
use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

use pharos_align::audio::DecodeError;
use pharos_align::service::{Decoder, ALIGNER_VERSION};
use serde_json::{json, Value};

use crate::common::{self, post, raw, serve, silent};

fn get(addr: SocketAddr, path: &str) -> (u16, Value) {
    raw(addr, &format!("GET {path} HTTP/1.1\r\nHost: t"), b"")
}

fn one_line() -> Value {
    json!({"path": "song.mp3", "lines": [{"start_ms": 1000, "end_ms": 2000, "text": "Hello world"}]})
}

#[test]
fn healthz_and_unknown_routes() {
    let s = serve("routes", silent(), None);
    assert_eq!(get(s.addr, "/healthz"), (200, json!({"ok": true})));
    assert_eq!(get(s.addr, "/nope"), (404, json!({"error": "not found"})));
    assert_eq!(get(s.addr, "/align").0, 404);
    assert_eq!(get(s.addr, "/metrics").0, 200);
    let head = "POST /healthz HTTP/1.1\r\nHost: t\r\nContent-Length: 0";
    assert_eq!(raw(s.addr, head, b"").0, 404);
}

#[test]
fn a_song_answers_with_its_words_offset_and_version() {
    let s = serve("ok", silent(), None);
    let (status, body) = post(s.addr, &one_line());
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body,
        json!({
            "lines": [{"words": [{"at": 0, "start_ms": 250}, {"at": 6, "start_ms": 350}, {"at": 11, "start_ms": 450}], "score": 0.813}],
            "offset_ms": 0,
            "aligner_version": ALIGNER_VERSION,
        })
    );
}

#[test]
fn each_line_answers_its_score_to_three_places_and_null_without_words() {
    let s = serve("score", silent(), None);
    let lines = json!({"path": "song.mp3", "lines": [
        {"start_ms": 1000, "end_ms": 2000, "text": "Hello world"},
        {"start_ms": 3000, "end_ms": 4000, "text": "..."},
    ]});
    let (status, body) = post(s.addr, &lines);
    assert_eq!(status, 200, "{body}");
    let scores: Vec<&Value> = body["lines"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| &l["score"])
        .collect();
    // The fake scores 0.8126: rounded, not cut to 0.812.
    assert_eq!(common::SCORE, 0.8126);
    assert_eq!(scores, [&json!(0.813), &Value::Null], "{body}");
}

#[test]
fn bad_bodies_and_paths_are_400_and_a_missing_file_404() {
    let s = serve("bad", silent(), None);
    assert_eq!(
        post(s.addr, &json!({"bad": "data"})),
        (400, json!({"error": "path is required"}))
    );
    let escape = json!({"path": "../x.mp3", "lines": []});
    assert_eq!(
        post(s.addr, &escape),
        (400, json!({"error": "path escapes the media root"}))
    );
    let missing = json!({"path": "gone.mp3", "lines": []});
    assert_eq!(
        post(s.addr, &missing),
        (404, json!({"error": "file not found: gone.mp3"}))
    );
    let head = "POST /align HTTP/1.1\r\nHost: t\r\nContent-Length: 3";
    let (status, body) = raw(s.addr, head, b"{x}");
    assert_eq!(status, 400);
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .starts_with("invalid JSON: "),
        "{body}"
    );
}

#[test]
fn a_timestamp_past_the_limit_is_a_400_through_the_http_path() {
    use pharos_align::plan::MAX_TIMESTAMP_MS;
    let s = serve("late", silent(), None);
    // No song duration is sent, so nothing but the check bounds the window.
    let body = |start: i64| {
        json!({"path": "song.mp3", "lines": [
            {"start_ms": start, "end_ms": start, "text": "test song A"}
        ]})
    };
    let late = MAX_TIMESTAMP_MS + 1;
    assert_eq!(
        post(s.addr, &body(late)),
        (
            400,
            json!({"error": format!("start_ms cannot exceed {MAX_TIMESTAMP_MS} ms, got {late}")})
        )
    );
    let (status, answer) = post(s.addr, &body(MAX_TIMESTAMP_MS));
    assert_eq!(status, 200, "a timestamp at the limit is aligned: {answer}");
}

#[test]
fn content_length_is_required_and_capped() {
    let s = serve("length", silent(), None);
    let chunked = "POST /align HTTP/1.1\r\nHost: t\r\nTransfer-Encoding: chunked";
    assert_eq!(
        raw(s.addr, chunked, b"2\r\n{}\r\n0\r\n\r\n"),
        (400, json!({"error": "Content-Length header required"}))
    );
    let big = format!(
        "POST /align HTTP/1.1\r\nHost: t\r\nContent-Length: {}",
        1024 * 1024 + 1
    );
    assert_eq!(
        raw(s.addr, &big, b""),
        (413, json!({"error": "payload too large"}))
    );
    // Python answers 413 for a negative length; actix's parser refuses it first.
    let negative = "POST /align HTTP/1.1\r\nHost: t\r\nContent-Length: -1";
    assert_eq!(raw(s.addr, negative, b"").0, 400);
}

#[test]
fn ffmpeg_failing_on_a_line_is_422_and_missing_ffmpeg_500() {
    let failing: Decoder = Box::new(|_: &Path, s, e| {
        Err(DecodeError::Failed {
            status: std::os::unix::process::ExitStatusExt::from_raw(256),
            start_ms: s,
            end_ms: e,
            stderr: "Invalid data found when processing input".into(),
        })
    });
    let s = serve("ffmpeg", failing, None);
    let (status, body) = post(s.addr, &one_line());
    assert_eq!(status, 422);
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .starts_with("ffmpeg failed: "),
        "{body}"
    );

    let missing: Decoder = Box::new(|_: &Path, _, _| {
        Err(DecodeError::Spawn {
            program: "ffmpeg".into(),
            source: std::io::ErrorKind::NotFound.into(),
        })
    });
    let s = serve("noffmpeg", missing, None);
    assert_eq!(
        post(s.addr, &one_line()),
        (500, json!({"error": "internal error"}))
    );
}

#[test]
fn healthz_answers_while_a_song_holds_the_lock_and_songs_queue() {
    let (open, gate) = mpsc::channel();
    let s = serve("busy", silent(), Some(gate));
    let addr = s.addr;
    let first = std::thread::spawn(move || post(addr, &one_line()));
    let second = std::thread::spawn(move || post(addr, &one_line()));
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(get(s.addr, "/healthz"), (200, json!({"ok": true})));
    assert!(!first.is_finished() && !second.is_finished());
    // One model call each: the second song can only start once the first ends.
    open.send(()).unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !first.is_finished() && !second.is_finished() {
        assert!(
            std::time::Instant::now() < deadline,
            "no song finished after its model call"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    std::thread::sleep(Duration::from_millis(100));
    assert!(
        first.is_finished() != second.is_finished(),
        "exactly one song done after one model call"
    );
    open.send(()).unwrap();
    assert_eq!(first.join().unwrap().0, 200);
    assert_eq!(second.join().unwrap().0, 200);
}
