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

/// An address nothing listens on: a link attempt fails at once.
pub fn dead_server() -> String {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    format!("http://{}", l.local_addr().unwrap())
}
