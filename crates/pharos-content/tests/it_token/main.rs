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
        let stem = std::env::temp_dir().join(format!(
            "pharos-content-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        Self(create_exclusive(&stem))
    }
}

/// Creates a folder that did not exist before: `stem-0`, else `stem-1`, and
/// so on. One that is already there is never taken (nor, so, removed by
/// [`Scratch`]'s drop).
pub fn create_exclusive(stem: &std::path::Path) -> PathBuf {
    for n in 0u32.. {
        let mut name = stem.as_os_str().to_owned();
        name.push(format!("-{n}"));
        let dir = PathBuf::from(name);
        match std::fs::create_dir(&dir) {
            Ok(()) => return dir,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => panic!("scratch folder {}: {e}", dir.display()),
        }
    }
    unreachable!("no free scratch folder name")
}

#[test]
fn a_scratch_folder_is_never_one_that_was_already_there() {
    let root = Scratch::new("exclusive");
    let stem = root.0.join("s");
    std::fs::create_dir(root.0.join("s-0")).unwrap();
    std::fs::write(root.0.join("s-0").join("kept"), "x").unwrap();
    let made = create_exclusive(&stem);
    assert_eq!(made, root.0.join("s-1"));
    assert!(root.0.join("s-0").join("kept").exists());
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A loopback listener held for the test's whole run, so no other process
/// can take its port: it counts each connection and closes it without a
/// word, so a link attempt fails at once and is seen to have been made.
pub struct Probe {
    pub url: String,
    accepted: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Probe {
    pub fn start() -> Self {
        use std::sync::atomic::Ordering;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let accepted = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (a, s) = (accepted.clone(), stop.clone());
        let thread = std::thread::spawn(move || {
            while !s.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((conn, _)) => {
                        a.fetch_add(1, Ordering::SeqCst);
                        drop(conn);
                    }
                    Err(_) => std::thread::sleep(std::time::Duration::from_millis(2)),
                }
            }
        });
        Self {
            url,
            accepted,
            stop,
            thread: Some(thread),
        }
    }

    /// Connections accepted so far.
    pub fn accepted(&self) -> usize {
        self.accepted.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Waits up to 5 s for at least `n` connections; whether they came.
    pub fn saw(&self, n: usize) -> bool {
        let until = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while self.accepted() < n {
            if std::time::Instant::now() > until {
                return false;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        true
    }
}

impl Drop for Probe {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}
