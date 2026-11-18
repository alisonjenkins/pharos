//! The device token and the user it was issued to, in ONE file.
//!
//! `pharos_content::token` owns the file whole (ADR 0145): its two-line
//! format, the process-wide lock every write takes, and the atomic
//! replacement. The car's sign-in writes through the same module, so it can
//! never land between a read and a write here. These are thin calls that
//! give the app's string errors. Never logs a value.

use std::path::{Path, PathBuf};

pub use pharos_content::token::{Pairing, Stored};

/// The stored token and its user; `None` when the file is absent or holds no
/// token.
pub fn read(path: &Path) -> Result<Option<Stored>, String> {
    pharos_content::token::read(path).map_err(|e| e.to_string())
}

/// A new sign-in's token. Whoever the file held it for is gone with it: the
/// new token's user is unknown until `store_user`.
pub fn store(path: &Path, token: &str) -> Result<(), String> {
    pharos_content::token::store(path, token).map_err(|e| e.to_string())
}

/// Pairs `user` with the token on file, under the file's lock. Refused when
/// there is none: an id with no token to belong to is nobody's.
pub fn store_user(path: &Path, user: &str) -> Result<(), String> {
    pharos_content::token::store_user(path, user).map_err(|e| e.to_string())
}

/// Pairs `user` with the token on file only while it is `seen`, under the
/// file's lock: a token replaced since is left unpaired.
pub fn store_user_if(path: &Path, seen: &str, user: &str) -> Result<Pairing, String> {
    pharos_content::token::store_user_if(path, seen, user).map_err(|e| e.to_string())
}

/// Removes the token and its user, under the file's lock; absent is fine.
pub fn forget(path: &Path) -> Result<(), String> {
    pharos_content::token::forget(path).map_err(|e| e.to_string())
}

/// The file the older layout kept the user id in. Never read; removed when
/// found, because the id it holds is not known to belong to the token.
pub fn legacy_user_path(path: &Path) -> PathBuf {
    pharos_content::token::legacy_user_path(path)
}
