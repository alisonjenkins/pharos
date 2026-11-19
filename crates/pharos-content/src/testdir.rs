//! A test's own folder: made with `create_dir` under a name no other test
//! or run uses, so a guard never takes over (or later removes) a folder
//! something else made, and removed when the guard drops, a failed
//! assertion's unwinding included.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};

pub(crate) struct TmpDir(PathBuf);

impl TmpDir {
    pub(crate) fn new(tag: &str) -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        loop {
            let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.subsec_nanos());
            let path = std::env::temp_dir().join(format!(
                "pharos-content-{tag}-{}-{n}-{nanos}",
                std::process::id()
            ));
            match std::fs::create_dir(&path) {
                Ok(()) => return Self(path),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => panic!("no test folder: {e}"),
            }
        }
    }
}

impl std::ops::Deref for TmpDir {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.0
    }
}

impl AsRef<Path> for TmpDir {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::TmpDir;

    /// Two folders with one tag are two folders, each made new, and each
    /// goes with its guard.
    #[test]
    fn each_test_folder_is_its_own_and_goes_with_its_guard() {
        let a = TmpDir::new("same");
        let b = TmpDir::new("same");
        assert_ne!(&*a, &*b);
        std::fs::write(a.join("f"), b"x").unwrap();
        let kept = a.to_path_buf();
        drop(a);
        assert!(!kept.exists(), "the folder outlived its guard");
        assert!(b.exists());
    }
}
