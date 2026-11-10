use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use actix_web::{web, App, HttpServer};
use pharos_align::audio::DecodeError;
use pharos_align::http::{configure, AppState};
use pharos_align::service::{Decoder, Engine, Model, ModelError, ALIGNER_VERSION};
use serde_json::{json, Value};

struct Even {
    /// When set, each align waits for a message first, holding the song lock.
    gate: Option<Arc<Mutex<mpsc::Receiver<()>>>>,
}

impl Model for Even {
    fn align(
        &mut self,
        _: &[f32],
        tokens: &[String],
        _: bool,
    ) -> Result<Vec<(u64, u64)>, ModelError> {
        if let Some(gate) = &self.gate {
            gate.lock().unwrap().recv().unwrap();
        }
        Ok((0..tokens.len() as u64)
            .map(|i| (i * 100, (i + 1) * 100))
            .collect())
    }
}

pub struct Server {
    pub addr: SocketAddr,
    root: PathBuf,
    handle: actix_web::dev::ServerHandle,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
        drop(self.handle.stop(false));
    }
}

pub fn serve(name: &str, decoder: Decoder, gate: Option<mpsc::Receiver<()>>) -> Server {
    let root =
        std::env::temp_dir().join(format!("pharos-align-http-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("song.mp3"), b"x").unwrap();
    let gate = gate.map(|g| Arc::new(Mutex::new(g)));
    let loader = Box::new(move || Ok(Box::new(Even { gate: gate.clone() }) as Box<dyn Model>));
    let state = web::Data::new(AppState {
        engine: Arc::new(Engine::new(loader, decoder, None)),
        media_root: root.clone(),
        metrics: None,
    });
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        actix_web::rt::System::new().block_on(async move {
            let server = HttpServer::new(move || App::new().configure(configure(state.clone())))
                .workers(2)
                .bind(("127.0.0.1", 0))
                .unwrap();
            let addr = server.addrs()[0];
            let running = server.run();
            tx.send((addr, running.handle())).unwrap();
            running.await.unwrap();
        });
    });
    let (addr, handle) = rx.recv_timeout(Duration::from_secs(10)).unwrap();
    Server { addr, root, handle }
}

pub fn silent() -> Decoder {
    Box::new(|_: &Path, _, _| Ok(vec![0.0; 16]))
}

/// Sends `head` (request line and headers, no blank line) and `body`; the status and body back.
pub fn raw(addr: SocketAddr, head: &str, body: &[u8]) -> (u16, Value) {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    s.write_all(format!("{head}\r\nConnection: close\r\n\r\n").as_bytes())
        .unwrap();
    s.write_all(body).unwrap();
    let mut out = Vec::new();
    s.read_to_end(&mut out).unwrap();
    let text = String::from_utf8_lossy(&out);
    let status = text.split(' ').nth(1).unwrap().parse().unwrap();
    let body = text.split_once("\r\n\r\n").map_or("", |(_, b)| b);
    (status, serde_json::from_str(body).unwrap_or(Value::Null))
}

pub fn post(addr: SocketAddr, body: &Value) -> (u16, Value) {
    let bytes = serde_json::to_vec(body).unwrap();
    let head = format!(
        "POST /align HTTP/1.1\r\nHost: t\r\nContent-Length: {}",
        bytes.len()
    );
    raw(addr, &head, &bytes)
}

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
            "lines": [{"words": [{"at": 0, "start_ms": 250}, {"at": 6, "start_ms": 350}, {"at": 11, "start_ms": 450}]}],
            "offset_ms": 0,
            "aligner_version": ALIGNER_VERSION,
        })
    );
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
