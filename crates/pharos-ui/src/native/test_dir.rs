//! A scope-owned test folder, shared by the native tests.
#![allow(clippy::unwrap_used)]

/// A folder only this test made, removed when the guard drops, a
/// failed assertion's unwinding included. Its name is new each time,
/// and it is made with `create_dir`, so a guard never takes over (or
/// later removes) a folder something else made.
pub(crate) struct TmpDir(std::path::PathBuf);

impl TmpDir {
    pub(crate) fn new(tag: &str) -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        loop {
            let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.subsec_nanos());
            let path = std::env::temp_dir().join(format!(
                "pharos-test-{tag}-{}-{n}-{nanos}",
                std::process::id()
            ));
            match std::fs::create_dir(&path) {
                Ok(()) => return Self(path),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => panic!("no test folder: {e}"),
            }
        }
    }

    pub(crate) fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

mod tests {
    use super::TmpDir;

    /// A test that fails part way leaves no folder behind.
    #[test]
    fn a_test_folder_goes_when_its_test_panics() {
        let made = std::sync::Mutex::new(None);
        let outcome = std::panic::catch_unwind(|| {
            let d = TmpDir::new("guard");
            std::fs::write(d.path().join("f"), b"x").unwrap();
            *made.lock().unwrap() = Some(d.path().to_path_buf());
            panic!("an assertion failed");
        });
        assert!(outcome.is_err());
        let made = made.lock().unwrap().clone().unwrap();
        assert!(!made.exists(), "{} was left behind", made.display());
    }
}
