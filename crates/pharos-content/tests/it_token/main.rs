//! Integration tests of the token file and its in-process fence, linked
//! once.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod fence_cross_thread;
mod fence_every_file;

use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard, PoisonError};

/// Held by each test of the group: a process-wide fence one sets must not
/// land under another's reads.
pub fn serial() -> MutexGuard<'static, ()> {
    static SERIAL: Mutex<()> = Mutex::new(());
    SERIAL.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A folder of this test's own, removed when it drops (a failed assertion
/// included).
pub struct Scratch(pub PathBuf);

impl Scratch {
    pub fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "pharos-content-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// An address nothing listens on: a link attempt fails at once.
pub fn dead_server() -> String {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    format!("http://{}", l.local_addr().unwrap())
}
