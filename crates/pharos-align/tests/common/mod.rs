//! The aligner's HTTP service on an ephemeral port, with a model that times
//! each token at 100 ms; shared by `it_service` and the standalone `spans`.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use actix_web::{web, App, HttpServer};
use pharos_align::http::{configure, AppState};
use pharos_align::line::LineSpans;
use pharos_align::service::{Decoder, Engine, Model, ModelError};
use serde_json::Value;

/// Every line's score from the fake model; the answer rounds it to 0.813.
pub const SCORE: f64 = 0.8126;

struct Even {
    /// When set, each align waits for a message first, holding the song lock.
    gate: Option<Arc<Mutex<mpsc::Receiver<()>>>>,
}

impl Model for Even {
    fn align(&mut self, _: &[f32], tokens: &[String], _: bool) -> Result<LineSpans, ModelError> {
        if let Some(gate) = &self.gate {
            gate.lock().unwrap().recv().unwrap();
        }
        Ok(LineSpans {
            times: (0..tokens.len() as u64)
                .map(|i| (i * 100, (i + 1) * 100))
                .collect(),
            score: Some(SCORE),
        })
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
