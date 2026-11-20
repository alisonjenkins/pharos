//! The device token file, owned here whole (ADR 0145): the token alone,
//! as the build before it reads the file, and beside it a pairing file
//! naming the user the token was issued to, with the token's fingerprint. The car's sign-in and the phone app both go through this module,
//! and every write of the file takes one process-wide lock, so a sign-in can
//! never land between another writer's read and its write (a user id would
//! otherwise sit beside a token it does not belong to, or a new token be
//! replaced by the old one).
//!
//! The file is in a directory the caller names (the app's private files
//! directory), replaced whole by a temp file of its own, fsynced and renamed
//! (mode 0600), with the directory synced after, so a crash leaves the old
//! file or the new one, never a mixture. A token whose pairing file is
//! missing or names another token's fingerprint is a token whose user is
//! unknown. Never logs a value.

use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};

use super::error::ContentError;

/// Held across every replacement and read-modify-write of a token file in
/// this process.
static FILE: Mutex<()> = Mutex::new(());

fn hold() -> MutexGuard<'static, ()> {
    FILE.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The in-process sign-out fence of one token file: set when a session
/// ended and its token could be neither removed nor marked revoked, so
/// nothing in this process resumes from it. It names those tokens by
/// fingerprint, as a set; with no token to name it refuses every token
/// until a store installs one. Kept per path, so one file's sign-out refuses nothing at
/// another. Set and replaced only under the file's lock, by the operation
/// that decided it ([`end_session`], [`store`]), and honored by [`read`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Fence {
    /// No sign-out left a token behind.
    #[default]
    Open,
    /// The tokens with these fingerprints are refused (never empty: no
    /// name is [`Fence::Open`]).
    Tokens(std::collections::BTreeSet<String>),
    /// The token left behind could not be named: every token is refused.
    Every,
    /// A store installed the token with this fingerprint over an unnamed
    /// fence: that token is allowed, and every other one, an older link's
    /// included, stays refused.
    AllBut(String),
}

impl Fence {
    /// The fence refusing the one token fingerprinted `fp`.
    pub fn token(fp: &str) -> Self {
        Self::Tokens(std::iter::once(fp.to_owned()).collect())
    }

    /// The fence for a session end that left the token fingerprinted
    /// `target` on file (`None`: it could not be named).
    pub fn for_target(target: Option<&str>) -> Self {
        target.map_or(Self::Every, Self::token)
    }

    /// The fence for a sign-out that left `held` on file.
    pub fn for_token(held: Option<&str>) -> Self {
        held.map_or(Self::Every, |t| Self::token(&fingerprint(t)))
    }

    /// Whether `token` is refused.
    pub fn refuses(&self, token: &str) -> bool {
        match self {
            Self::Open => false,
            Self::Tokens(fps) => fps.contains(&fingerprint(token)),
            Self::Every => true,
            Self::AllBut(fp) => fingerprint(token) != *fp,
        }
    }

    /// The verdict word for logs.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Tokens(_) => "token",
            Self::Every => "every",
            Self::AllBut(_) => "all_but",
        }
    }

    /// The fence once a store installed the token fingerprinted `stored`
    /// (over the one fingerprinted `replaced`, when that could be read).
    /// The fence's names lose the stored token's and keep the others: they
    /// refuse only those tokens. One left with no name, or none, opens. An unnamed one allows the
    /// stored token and nothing else: the write proves what is on disk
    /// now, not which token an older live link still holds, so every
    /// other token, the replaced one included, stays refused.
    pub fn after_store(&self, stored: &str, replaced: Option<&str>) -> Self {
        let _ = replaced;
        match self {
            Self::Open => Self::Open,
            Self::Tokens(fps) => {
                let left: std::collections::BTreeSet<String> =
                    fps.iter().filter(|fp| *fp != stored).cloned().collect();
                if left.is_empty() {
                    Self::Open
                } else {
                    Self::Tokens(left)
                }
            }
            Self::Every | Self::AllBut(_) => Self::AllBut(stored.to_owned()),
        }
    }

    /// A fence that refuses whatever `self` or `other` refuses: a sign-out
    /// that fails never lifts an earlier refusal. [`Fence::Every`] stays
    /// every; named tokens join into one set; a name that is the allowed
    /// token of an [`Fence::AllBut`], or two different allowed tokens,
    /// become [`Fence::Every`].
    pub fn union(&self, other: &Self) -> Self {
        match (self, other) {
            (Self::Open, x) | (x, Self::Open) => x.clone(),
            (a, b) if a == b => a.clone(),
            (Self::Every, _) | (_, Self::Every) => Self::Every,
            (Self::Tokens(a), Self::Tokens(b)) => Self::Tokens(a.union(b).cloned().collect()),
            (Self::Tokens(t), Self::AllBut(a)) | (Self::AllBut(a), Self::Tokens(t)) => {
                if t.contains(a) {
                    Self::Every
                } else {
                    Self::AllBut(a.clone())
                }
            }
            _ => Self::Every,
        }
    }
}

/// Every token file's fence ([`Fence`]), by path. Changed only while the
/// file's lock ([`FILE`]) is held.
static FENCES: Mutex<std::collections::BTreeMap<PathBuf, Fence>> =
    Mutex::new(std::collections::BTreeMap::new());

/// Set by a sign-out that could not even find its token file: it cannot
/// name the file, so every token at every path is refused until a store
/// installs one. Changed only while the file lock ([`FILE`]) is held.
static EVERY_FILE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Fences every token at every path in this process, under the file lock:
/// for a sign-out that could not find its token file (`reason` for the
/// log). The next store, at any path, replaces it.
pub fn fence_every_file(reason: &str) {
    let _held = hold();
    EVERY_FILE.store(true, std::sync::atomic::Ordering::SeqCst);
    log::warn!("token fence verdict=every scope=process reason={reason}");
    changed("fence");
}

fn fence_held(path: &Path) -> Fence {
    if EVERY_FILE.load(std::sync::atomic::Ordering::SeqCst) {
        return Fence::Every;
    }
    FENCES
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(path)
        .cloned()
        .unwrap_or_default()
}

/// Adds `more` to the fence of `path` ([`Fence::union`]): it refuses what
/// it refused before and what `more` refuses. Every fence change but a
/// confirmed store's goes through it, so a failure never narrows a fence.
/// The caller holds the file's lock.
fn add_fence_held(path: &Path, more: &Fence, reason: &str) {
    let was = FENCES
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(path)
        .cloned()
        .unwrap_or_default();
    replace_fence_held(path, was.union(more), reason);
}

/// Replaces the fence of `path`: only for a store confirmed on disk
/// ([`Fence::after_store`]), and for tests. The caller holds the file's
/// lock.
fn replace_fence_held(path: &Path, next: Fence, reason: &str) {
    let was = {
        let mut all = FENCES.lock().unwrap_or_else(PoisonError::into_inner);
        let was = all.get(path).cloned().unwrap_or_default();
        if next == Fence::Open {
            all.remove(path);
        } else {
            all.insert(path.to_owned(), next.clone());
        }
        was
    };
    if was != next {
        log::warn!(
            "token fence verdict={} reason={reason} was={}",
            next.label(),
            was.label()
        );
        changed("fence");
    }
}

/// The fence of the token file at `path`, read under the file's lock.
pub fn fence(path: &Path) -> Fence {
    let _held = hold();
    fence_held(path)
}

/// Adds `more` to the fence of the token file at `path`, under the file's
/// lock: for a caller that decided it from a read of its own (a test, or
/// a host whose sign-out was refused before it reached the file). It
/// never lifts what the fence already refuses; only a store does.
pub fn add_fence(path: &Path, more: &Fence, reason: &str) {
    let _held = hold();
    add_fence_held(path, more, reason);
}

/// Clears the fence of `path`, as a restart does.
#[cfg(test)]
pub(crate) fn clear_fence(path: &Path) {
    let _held = hold();
    replace_fence_held(path, Fence::Open, "test_clear");
}

fn err(path: &Path, e: impl std::fmt::Display) -> ContentError {
    ContentError::TokenFile {
        path: path.to_owned(),
        why: e.to_string(),
    }
}

/// What the file holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stored {
    pub token: String,
    pub user: Option<String>,
}

/// `None` for no token. The token is the first line. A second line is the
/// user id of an earlier layout of this build, which kept the pair in one
/// file: [`read_raw`] moves it to the pairing file.
fn decode(raw: &str) -> Option<Stored> {
    let mut lines = raw.lines();
    let token = lines.next()?.trim().to_owned();
    if token.is_empty() {
        return None;
    }
    let user = lines
        .next()
        .map(|u| u.trim().to_owned())
        .filter(|u| !u.is_empty());
    Some(Stored { token, user })
}

/// The stored token and its user; `None` when the file is absent, holds no
/// token, or holds a token a revocation marker names ([`end_session`]): a
/// session that ended and could not remove its token never resumes from it,
/// in this process or after a restart. A replacement is a rename, so a read
/// sees one whole file.
pub fn read(path: &Path) -> Result<Option<Stored>, ContentError> {
    // The token and the marker are read under the file's lock, as the
    // writers change them: never one from before a change and the other
    // from after it.
    let _held = hold();
    let Some(stored) = read_raw(path)? else {
        return Ok(None);
    };
    #[cfg(test)]
    tests::AFTER_RAW_READ.with(|h| {
        if let Some(f) = h.borrow_mut().take() {
            f();
        }
    });
    match marker(path, &fingerprint(&stored.token)) {
        Marker::Present(_) => {
            log::debug!("token read verdict=refused reason=revoked_marker");
            Ok(None)
        }
        // A marker that is there but cannot be read or understood may name
        // this token: fail closed, until a new sign-in replaces both.
        Marker::Unreadable(why) => {
            log::warn!(
                "token read verdict=refused reason=revocation_marker_unreadable error={why}"
            );
            Ok(None)
        }
        Marker::Absent => {
            let fence = fence_held(path);
            if fence.refuses(&stored.token) {
                log::info!(
                    "token read verdict=refused reason=sign_out_fence fence={}",
                    fence.label()
                );
                return Ok(None);
            }
            Ok(Some(stored))
        }
    }
}

/// What a download run's read of the token file found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunRead {
    /// The token, with the user it is paired with.
    Token(Stored),
    /// The sign-out fence refuses it (or every token): nothing is sent.
    Fenced,
    /// No token to send: none on file, or one a marker revokes.
    NoToken,
}

/// The token for a download run and the fence decision on it, from one
/// hold of the file's lock: the fence is the one in force when the token
/// is read, never a snapshot from before it. A token stored after an older
/// fence was set is allowed; one the fence names when it is read is not.
pub fn read_for_run(path: &Path) -> Result<RunRead, ContentError> {
    let _held = hold();
    let fence = fence_held(path);
    if fence == Fence::Every {
        return Ok(RunRead::Fenced);
    }
    #[cfg(test)]
    tests::between_read_and_write();
    let Some(stored) = read_raw(path)? else {
        return Ok(RunRead::NoToken);
    };
    if !matches!(marker(path, &fingerprint(&stored.token)), Marker::Absent) {
        return Ok(RunRead::NoToken);
    }
    if fence.refuses(&stored.token) {
        return Ok(RunRead::Fenced);
    }
    Ok(RunRead::Token(stored))
}

/// The file as it is, marker or not, with the user its pairing file names
/// for it. The writers compare against this. Called with the lock held.
///
/// The token file holds the token alone, as the build before ADR 0145
/// wrote and reads it whole: a rollback to it reads a usable token. The
/// user lives in the pairing file ([`user_path`]) with the fingerprint of
/// the token it was paired with, and counts only while that is the token
/// on file; otherwise the token is unpaired (fail closed). A token file in
/// the earlier two-line layout is split on read: the pairing file first,
/// then the token file rewritten to the token alone.
fn read_raw(path: &Path) -> Result<Option<Stored>, ContentError> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(err(path, e)),
    };
    let Some(stored) = decode(&raw) else {
        return Ok(None);
    };
    if let Some(user) = &stored.user {
        let migrated =
            write_pair(path, user, &stored.token).and_then(|()| write(path, &stored.token));
        match migrated {
            Ok(()) => log::info!("token file verdict=migrated reason=paired_layout_split"),
            Err(e) => log::warn!("token file verdict=not_migrated reason=split_failed error={e}"),
        }
        return Ok(Some(stored));
    }
    Ok(Some(Stored {
        user: paired_user(path, &stored.token),
        ..stored
    }))
}

/// The user the pairing file names for `token`: `None` when there is no
/// pairing file, it cannot be read, or it names another token.
fn paired_user(path: &Path, token: &str) -> Option<String> {
    let raw = std::fs::read_to_string(user_path(path)).ok()?;
    let mut lines = raw.lines().map(str::trim);
    let user = lines.next().filter(|u| !u.is_empty())?;
    if lines.next() != Some(fingerprint(token).as_str()) {
        log::debug!("token pairing verdict=unpaired reason=fingerprint_differs");
        return None;
    }
    Some(user.to_owned())
}

/// The revocation marker beside the token file: written when a session
/// ended and its token could not be removed. It names the token's user and
/// a fingerprint of the token, never the token.
pub fn revoked_path(path: &Path, fingerprint: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".revoked-");
    name.push(fingerprint);
    PathBuf::from(name)
}

/// What a revocation marker holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Revoked {
    /// The user the revoked token was paired with, when it was.
    pub user: Option<String>,
    /// Written by a store that installed its token but could not confirm
    /// it (its folder would not sync), not by a sign-out or a session end:
    /// a store of that same approved token may retry the install.
    pub install_unconfirmed: bool,
}

/// The revocation marker of one token, as a read finds it. Markers are
/// keyed by the fingerprint in their file name, one per token: what a
/// marker refuses is its name, never its contents, so one that cannot be
/// parsed still refuses its own token and no other.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Marker {
    /// No marker: the token is not revoked.
    Absent,
    /// A marker for the token (the user it names, when it can be read).
    Present(Revoked),
    /// Whether there is a marker could not be told (why): the read refuses
    /// the token (fail closed).
    Unreadable(String),
}

/// The marker of the token fingerprinted `fingerprint`, when there is one.
pub fn revoked(path: &Path, fingerprint: &str) -> Option<Revoked> {
    match marker(path, fingerprint) {
        Marker::Present(r) => Some(r),
        Marker::Absent | Marker::Unreadable(_) => None,
    }
}

/// The marker of the token fingerprinted `fingerprint` beside `path`.
pub fn marker(path: &Path, fingerprint: &str) -> Marker {
    #[cfg(debug_assertions)]
    if faults::MARKER_READ.with(std::cell::Cell::get) {
        return Marker::Unreadable("marker read failed".to_owned());
    }
    let at = revoked_path(path, fingerprint);
    match std::fs::symlink_metadata(&at) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Marker::Absent,
        Err(e) => return Marker::Unreadable(e.to_string()),
    }
    let raw = std::fs::read_to_string(&at).unwrap_or_default();
    // A cleared marker is no marker (see `clear_marker`).
    if raw.trim_end() == CLEARED {
        return Marker::Absent;
    }
    let mut lines = raw.lines();
    let user = lines
        .next()
        .map(str::trim)
        .filter(|u| !u.is_empty())
        .map(str::to_owned);
    let install_unconfirmed = lines.nth(1).map(str::trim) == Some(INSTALL_UNCONFIRMED);
    Marker::Present(Revoked {
        user,
        install_unconfirmed,
    })
}

/// A fingerprint of a token (64-bit FNV-1a, spelled out here so no
/// toolchain update changes it), so the marker can name it without holding
/// it.
pub fn fingerprint(token: &str) -> String {
    let h = token
        .trim()
        .bytes()
        .fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
            (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
        });
    format!("{h:016x}")
}

/// The stored token alone; `None` when the file is absent or empty.
pub fn load(path: &Path) -> Result<Option<String>, ContentError> {
    Ok(read(path)?.map(|s| s.token))
}

/// A new sign-in's token, the file replaced whole. Whoever the file held it
/// for is gone with it: the new token's user is unknown until `store_user`.
/// Surrounding whitespace is not part of a token.
/// What runs after a store installed its token, with the new token's
/// fingerprint and the one it overwrote (`None` when there was none, or it
/// could not be read). Runs under the file's lock: it must not touch the
/// token file, nor take a lock a token-file caller holds.
static ON_STORED: std::sync::OnceLock<fn(&str, Option<&str>)> = std::sync::OnceLock::new();

/// Registers the store hook ([`ON_STORED`]). Set once; later calls are
/// ignored. Every sign-in stores through [`store`], whichever process or
/// path runs it (the phone's, or the car's QuickConnect), so the hook sees
/// them all.
pub fn on_stored(hook: fn(&str, Option<&str>)) {
    let _ = ON_STORED.set(hook);
}

/// What runs whenever an operation changed what a read of a token file
/// returns: a store, a removal, a revocation marker written, or a fence
/// set or lifted. Called from inside that operation, under the file's
/// lock, so a cache of the token cannot miss a change: the same rules as
/// [`ON_STORED`] apply.
static ON_CHANGED: std::sync::OnceLock<fn(&str)> = std::sync::OnceLock::new();

/// Registers the change hook ([`ON_CHANGED`]), once; it gets the reason.
pub fn on_changed(hook: fn(&str)) {
    let _ = ON_CHANGED.set(hook);
}

fn changed(reason: &str) {
    if let Some(hook) = ON_CHANGED.get() {
        hook(reason);
    }
}

pub fn store(path: &Path, token: &str) -> Result<(), ContentError> {
    let _held = hold();
    let token = token.trim();
    if token.is_empty() {
        log::warn!("token store verdict=refused reason=empty_token");
        return Err(err(path, "an empty token"));
    }
    // The token this one replaces, for the fence. A read that fails does
    // not matter here: store never touches another token's marker.
    #[cfg(test)]
    let read_previous = if tests::FAIL_PREVIOUS_READ.with(std::cell::Cell::get) {
        Err(err(path, "previous token unreadable"))
    } else {
        read_raw(path)
    };
    #[cfg(not(test))]
    let read_previous = read_raw(path);
    let previous_unknown = read_previous.is_err();
    let previous = read_previous.ok().flatten();
    let replaced = previous.as_ref().map(|s| fingerprint(&s.token));
    // Markers are keyed by fingerprint and, for store, append-only: it
    // never removes or rewrites a marker naming any token but this one. One
    // naming a token no longer on disk refuses only that token, so it is
    // harmless. The marker naming THIS token is cleared first, durably
    // (`clear_marker`: renamed over, never unlinked, its folder synced); one
    // that will not clear refuses the sign-in
    // before anything is written, so no token is ever installed while its
    // own marker is pending. A write that fails leaves the old file and
    // every other marker as they were.
    let new = fingerprint(token);
    // A sign-in that yields the very token a marker revokes is refused
    // before the marker is touched: removing it and then failing to write
    // would leave that token on file with nothing refusing it. A token
    // file that could not be read may hold it, so it is refused too.
    let same = previous
        .as_ref()
        .map_or(previous_unknown, |s| s.token == token);
    let found = marker(path, &new);
    // A marker this store's own unconfirmed install left is no revocation:
    // the same approved token installs again once the folder syncs.
    let retry = matches!(
        &found,
        Marker::Present(Revoked {
            install_unconfirmed: true,
            ..
        })
    );
    if same && retry {
        log::info!("token store verdict=retry reason=install_unconfirmed");
    }
    if same && !retry && !matches!(found, Marker::Absent) {
        log::warn!(
            "token store verdict=refused reason=same_token_revoked previous={}",
            if previous_unknown {
                "unreadable"
            } else {
                "same"
            }
        );
        return Err(err(
            path,
            "a revocation marker refuses this token, and it is the one on file",
        ));
    }
    // A retry's marker names the token already on file: cleared only once
    // this write, its fsync and rename and the folder's sync have all held
    // (below), so a write that fails leaves that token refused after a
    // restart. Any other marker naming this token names one not on file:
    // cleared first, durably, so a write that fails leaves nothing
    // readable and the old token as it was.
    let clear_after = same && retry;
    if clear_after {
        // Nothing to remove before the write.
    } else if let Err(e) = clear_marker(path, &new) {
        log::warn!(
            "token store verdict=refused reason=revocation_marker_blocks_token step=before_write error={e}"
        );
        return Err(err(
            path,
            format!("the revocation marker refuses the new token and cannot be removed: {e}"),
        ));
    }
    // A pairing that survived from an earlier install of this very token
    // (A, then B, then A again, its unlink failing each time) would name
    // a user this sign-in has not confirmed once the file holds A again:
    // cleared durably first, or the sign-in is refused before the rename.
    if !matches!(&previous, Some(s) if s.token == token) && paired_user(path, token).is_some() {
        if let Err(e) = remove_pairing(path) {
            log::warn!("token store verdict=refused reason=earlier_pairing_not_cleared error={e}");
            return Err(err(
                path,
                format!("a pairing from an earlier install of this token cannot be cleared: {e}"),
            ));
        }
        log::info!("token pairing verdict=cleared reason=earlier_install_of_this_token");
    }
    write(path, token).map_err(|e| err(path, e))?;
    // Installed: a token that replaces another starts unpaired. A pairing
    // left from an earlier install of this same token (A, then B, then A
    // again) would name a user this sign-in has not confirmed.
    if !matches!(&previous, Some(s) if s.token == token) {
        match unlink_pairing(path) {
            Ok(()) => log::info!("token pairing verdict=cleared reason=token_replaced"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                log::warn!("token pairing verdict=not_cleared reason=token_replaced error={e}")
            }
        }
    }
    let dir = path.parent().ok_or_else(|| err(path, "no directory"))?;
    match durable_dir(dir) {
        Ok(()) => {}
        // A folder that cannot be synced at all (EINVAL, ENOTSUP) offers
        // nothing better: refusing would leave no way to sign in on it.
        Err(why) if why.starts_with(DIR_SYNC_UNSUPPORTED) => log::warn!(
            "token store verdict=installed_not_durable reason=dir_sync_unsupported error={why}"
        ),
        Err(why) => {
            // Installed, but a crash may undo or keep the rename: the new token
            // is refused here, and by a marker of its own after a restart.
            // Added to the fence, never in its place: an older link's token
            // that it refused stays refused.
            add_fence_held(path, &Fence::token(&new), "store_not_durable");
            let marked = write_marker(path, None, &new, true);
            // The rename landed: a cached token is stale, whether or not the
            // fence moved (it may name this token already).
            changed("store_not_durable");
            log::warn!(
                "token store verdict=refused reason=folder_not_synced marker={} error={why}",
                if marked.is_ok() {
                    "written"
                } else {
                    "attempted"
                }
            );
            return Err(err(
                path,
                format!("the token's folder could not be synced: {why}"),
            ));
        }
    }
    // The retry's token is on disk, durably: its marker clears last. One
    // that will not go keeps the token refused, here by the fence and
    // after a restart by the marker; a later retry clears it.
    if clear_after {
        if let Err(e) = clear_marker(path, &new) {
            add_fence_held(path, &Fence::token(&new), "store_marker_not_cleared");
            log::warn!(
                "token store verdict=refused reason=revocation_marker_blocks_token step=after_write error={e}"
            );
            return Err(err(
                path,
                format!("the token is written but its unconfirmed-install marker cannot be removed: {e}"),
            ));
        }
        log::info!("token store verdict=retried reason=install_unconfirmed marker=cleared");
    }
    // The hook hears of the store only once the file is read back holding
    // the new token: a fence it lifts is for a token really gone. A store
    // that cannot confirm it is not a success: the file may have changed,
    // but the fence and the hook have not followed, so the caller is told
    // the sign-in is incomplete ([`is_store_incomplete`]).
    #[cfg(test)]
    let fail_read_back = tests::FAIL_READ_BACK.with(std::cell::Cell::get);
    #[cfg(not(test))]
    let fail_read_back = false;
    #[cfg(debug_assertions)]
    let fail_read_back = fail_read_back || faults::READ_BACK.with(std::cell::Cell::get);
    let back = if fail_read_back {
        Err(err(path, "read back failed"))
    } else {
        read_raw(path)
    };
    match back {
        Ok(Some(s)) if s.token == token => {
            let stored = fingerprint(token);
            let next = fence_held(path).after_store(&stored, replaced.as_deref());
            replace_fence_held(path, next, "token_stored");
            if EVERY_FILE.swap(false, std::sync::atomic::Ordering::SeqCst) {
                log::info!("token fence verdict=open scope=process reason=token_stored");
            }
            if let Some(hook) = ON_STORED.get() {
                hook(&fingerprint(token), replaced.as_deref());
            }
            changed("stored");
        }
        Ok(other) => {
            let found = if other.is_some() {
                "another_token"
            } else {
                "none"
            };
            // The file may hold the new token, unconfirmed: refused, as a
            // store that could not sync its folder refuses it, until a
            // store confirms it.
            add_fence_held(path, &Fence::token(&new), "store_incomplete");
            log::warn!(
                "token store verdict=incomplete reason=read_back_differs found={found} fenced=new_token"
            );
            // The rename may have landed: a cached token is stale now, and
            // reloads through the fence above.
            changed("store_incomplete");
            return Err(err(
                path,
                format!("{STORE_INCOMPLETE}: the file read back holds {found}"),
            ));
        }
        Err(e) => {
            add_fence_held(path, &Fence::token(&new), "store_incomplete");
            log::warn!(
                "token store verdict=incomplete reason=read_back_failed fenced=new_token error={}",
                e.redacted()
            );
            changed("store_incomplete");
            return Err(err(
                path,
                format!("{STORE_INCOMPLETE}: the file could not be read back"),
            ));
        }
    }
    Ok(())
}

/// How [`store`] names a store whose file may have changed but could not be
/// confirmed: the sign-in is not complete.
const STORE_INCOMPLETE: &str = "token store incomplete";

/// Whether `e` is a [`store`] that wrote but could not confirm the token:
/// the file may hold it, but this process has not taken it up, so a
/// sign-in must stop rather than go on with it.
pub fn is_store_incomplete(e: &ContentError) -> bool {
    matches!(e, ContentError::TokenFile { why, .. } if why.starts_with(STORE_INCOMPLETE))
}

/// Pairs `user` with the token on file, read and written under the lock.
/// Refused when there is none: an id with no token to belong to is nobody's.
pub fn store_user(path: &Path, user: &str) -> Result<(), ContentError> {
    let _held = hold();
    let stored = read_raw(path)?;
    #[cfg(test)]
    tests::between_read_and_write();
    match stored {
        Some(s) => write_pair(path, user, &s.token).map_err(|e| err(path, e)),
        None => Err(err(path, "no token to keep the user with")),
    }
}

/// What [`store_user_if`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pairing {
    /// The user is now paired with the token.
    Paired,
    /// The token on file is not the one the user was learned with: nothing
    /// was written, and the token stays unpaired.
    TokenChanged,
}

/// Pairs `user` with the token on file only while that token is `seen`, the
/// one the request that named `user` was sent with. The compare and the
/// write are one step under the file's lock. A different token (a new
/// sign-in landed in between) is left as it is, unpaired: its user is not
/// known, so a check that needs one fails closed. No token is refused, as
/// in [`store_user`].
pub fn store_user_if(path: &Path, seen: &str, user: &str) -> Result<Pairing, ContentError> {
    let _held = hold();
    match read_raw(path)? {
        Some(s) if s.token == seen.trim() => write_pair(path, user, &s.token)
            .map(|()| Pairing::Paired)
            .map_err(|e| err(path, e)),
        Some(_) => Ok(Pairing::TokenChanged),
        None => Err(err(path, "no token to keep the user with")),
    }
}

/// What a [`forget`] that removed the file left on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Forgotten {
    /// Gone for good: the removal reached the disk, or there was nothing.
    Durable,
    /// Gone, so signed out now, but the folder's entry could not be made
    /// durable: a crash may bring the token back. Why, for the caller to
    /// report.
    NotDurable(String),
}

/// Removes the token and its user; absent is fine. Once removed the token
/// is gone for every reader, whatever the folder sync after it does.
pub fn forget(path: &Path) -> Result<Forgotten, ContentError> {
    let _held = hold();
    remove(path)
}

/// What [`forget_if`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForgetIf {
    /// The token on file was the refused one, and it is gone (or there was
    /// none).
    Forgotten(Forgotten),
    /// A sign-in replaced the token since it was read: the replacement is
    /// kept, and nothing was removed.
    Changed,
}

/// Removes the token and its user only while the token on file is `seen`,
/// the one a refused resume was sent with. The compare and the removal are
/// one step under the file's lock, so a sign-in that stored a new token in
/// between keeps it. No token is nothing to remove.
pub fn forget_if(path: &Path, seen: &str) -> Result<ForgetIf, ContentError> {
    let _held = hold();
    match read_raw(path)? {
        Some(s) if s.token != seen.trim() => Ok(ForgetIf::Changed),
        Some(_) => remove(path).map(ForgetIf::Forgotten),
        None => Ok(ForgetIf::Forgotten(absent(path))),
    }
}

/// What [`end_session`] did to the token of a session that ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ended {
    /// The token is gone (`Forgotten::NotDurable` when its folder could not
    /// be synced), or there was none.
    Forgotten(Forgotten),
    /// A sign-in replaced the token since: the replacement is kept.
    Changed,
    /// The token could not be removed (why), but a synced marker now names
    /// it: no read returns it, here or after a restart.
    Marked(String),
    /// Neither the removal nor the marker could be written: a restart may
    /// resume the ended session's account from the token on file. `target`
    /// is the fingerprint of the token this call tried to end, read under
    /// the file's lock (`None` when none could be named), for an in-process
    /// fence to name.
    Unsafe {
        removal: String,
        marker: String,
        target: Option<String>,
    },
}

impl Ended {
    /// The log verdict.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Forgotten(Forgotten::Durable) => "forgotten",
            Self::Forgotten(Forgotten::NotDurable(_)) => "forgotten_not_durable",
            Self::Changed => "kept_replacement",
            Self::Marked(_) => "marked_revoked",
            Self::Unsafe { .. } => "restart_unsafe",
        }
    }
}

/// Ends a session's hold on the token file, under the file's lock: removes
/// the token while it is still `seen` (any token with `None`, a sign-out),
/// and keeps a replacement. A token that will not go is marked revoked by a
/// synced file beside it naming its user (`user`, else the one paired with
/// it) and its fingerprint, so it never resumes; when the marker cannot be
/// written either, says so.
///
/// A session end that is `Unsafe` fences the token it targeted in this
/// process ([`Fence`]), in the same hold of the lock: by fingerprint, or
/// every token when it could not be named. No store can land between the
/// operation and its fence, so the fence names what the operation saw.
pub fn end_session(path: &Path, seen: Option<&str>, user: Option<&str>) -> Ended {
    let _held = hold();
    let ended = end_session_held(path, seen, user);
    if let Ended::Unsafe { target, .. } = &ended {
        // Added to what is already refused, never in its place: an earlier
        // sign-out's token may still be held by a live link.
        add_fence_held(
            path,
            &Fence::for_target(target.as_deref()),
            "token_neither_removed_nor_marked",
        );
    }
    ended
}

/// [`end_session`]'s work, under the lock its caller holds.
fn end_session_held(path: &Path, seen: Option<&str>, user: Option<&str>) -> Ended {
    let read = {
        #[cfg(debug_assertions)]
        let failed = faults::TOKEN_READ.with(std::cell::Cell::get);
        #[cfg(not(debug_assertions))]
        let failed = false;
        if failed {
            Err(err(path, "token read failed"))
        } else {
            read_raw(path)
        }
    };
    let stored = match read {
        Ok(Some(s)) if seen.is_some_and(|seen| s.token != seen.trim()) => return Ended::Changed,
        Ok(None) => {
            return match absent(path) {
                Forgotten::Durable => Ended::Forgotten(Forgotten::Durable),
                // Gone, but its absence may not be on disk yet (an earlier
                // unlink whose sync failed): the token the session ended
                // with is marked, when one is named.
                Forgotten::NotDurable(why) => match seen {
                    Some(seen) => mark(
                        path,
                        user,
                        &fingerprint(seen),
                        format!("absence not durable: {why}"),
                    ),
                    None => Ended::Forgotten(Forgotten::NotDurable(why)),
                },
            };
        }
        Ok(Some(s)) => s,
        // Unreadable: the compare cannot be made, so nothing is removed (it
        // may be a replacement). The token the session ended with is still
        // marked, for the user expected, when the caller names it.
        Err(e) => {
            let removal = format!("not attempted: the token could not be read to compare ({e})");
            let Some(seen) = seen else {
                log::warn!("token revocation marker verdict=not_written reason=token_unreadable_no_token_named error={e}");
                return Ended::Unsafe {
                    removal,
                    marker: "no token named to fingerprint".to_owned(),
                    target: None,
                };
            };
            // Markers are one per token: the ended token's goes beside any
            // other's, which stays.
            let target = fingerprint(seen);
            return mark(path, user, &target, removal);
        }
    };
    let removal = match remove(path) {
        Ok(Forgotten::Durable) => return Ended::Forgotten(Forgotten::Durable),
        // Gone, but a crash may bring it back: marked too, so it never
        // resumes; only when the marker fails as well is it unsafe.
        Ok(Forgotten::NotDurable(why)) => format!("removal not durable: {why}"),
        Err(e) => e.to_string(),
    };
    let named = user.map(str::to_owned).or(stored.user.clone());
    mark(path, named.as_deref(), &fingerprint(&stored.token), removal)
}

/// Writes the marker for `user` and `token_fingerprint` after a removal
/// that did not hold (`removal` says why): `Marked`, or `Unsafe` when the
/// marker fails too.
fn mark(path: &Path, named: Option<&str>, token_fingerprint: &str, removal: String) -> Ended {
    match write_marker(path, named, token_fingerprint, false) {
        Ok(()) => {
            changed("marked");
            log::warn!(
                "token revocation marker verdict=written reason=token_not_removed user={} error={removal}",
                named.unwrap_or("")
            );
            Ended::Marked(removal)
        }
        Err(marker) => Ended::Unsafe {
            removal,
            marker: marker.to_string(),
            target: Some(token_fingerprint.to_owned()),
        },
    }
}

/// What a cleared marker holds: a reader takes it as no marker.
const CLEARED: &str = "cleared";

/// Clears the revocation marker naming `fingerprint` beside `path` (absent
/// is fine), so the store may write the token it named. A marker is never
/// unlinked: its state changes only by an atomic rename over it. The
/// cleared content (`CLEARED`) goes to a temp file, fsynced; the folder is
/// synced; the temp file is renamed over the marker; the folder is synced
/// again. At every moment the file on disk is the old marker or the cleared
/// one, so no failure leaves the token with no marker:
///
/// - the folder will not sync before the rename, or the rename fails: the
///   old marker stays as it was, and the error is returned;
/// - the folder will not sync after the rename: the cleared marker is in
///   place but a crash may bring the old one back, which refuses the
///   token (the safe side); the error is returned, and the caller keeps
///   its in-process fence.
///
/// A folder that cannot be synced at all (EINVAL, ENOTSUP) is the one
/// exception, logged as not durable: refusing there would leave no way to
/// sign in. Cleared markers are left in place; they cost one small file.
fn clear_marker(path: &Path, fingerprint: &str) -> std::io::Result<()> {
    let at = revoked_path(path, fingerprint);
    match std::fs::symlink_metadata(&at) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    }
    if is_cleared(&at) {
        return Ok(());
    }
    let dir = path
        .parent()
        .ok_or_else(|| std::io::Error::other("no directory"))?;
    let mut tmp_name = at.as_os_str().to_owned();
    tmp_name.push(".tmp");
    let tmp = PathBuf::from(tmp_name);
    let sync = || -> Result<(), String> {
        #[cfg(test)]
        if tests::FAIL_MARKER_SYNC.with(std::cell::Cell::get) {
            return Err("directory sync failed".to_owned());
        }
        durable_dir(dir)
    };
    let kept = |why: &str, step: &str| {
        let _ = std::fs::remove_file(&tmp);
        log::warn!("token revocation marker verdict=kept reason={step} error={why}");
        std::io::Error::other(format!("the marker could not be cleared ({step}): {why}"))
    };
    let staged = (|| -> std::io::Result<()> {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(format!("{CLEARED}\n").as_bytes())?;
        f.sync_all()
    })();
    if let Err(e) = staged {
        return Err(kept(&e.to_string(), "temp_not_written"));
    }
    let unsupported = |why: &str| why.starts_with(DIR_SYNC_UNSUPPORTED);
    match sync() {
        Ok(()) => {}
        Err(why) if unsupported(&why) => {}
        Err(why) => return Err(kept(&why, "folder_not_synced")),
    }
    #[cfg(debug_assertions)]
    let renamed = if faults::MARKER_RENAME.with(std::cell::Cell::get) {
        Err(std::io::Error::other("marker rename failed"))
    } else {
        std::fs::rename(&tmp, &at)
    };
    #[cfg(not(debug_assertions))]
    let renamed = std::fs::rename(&tmp, &at);
    if let Err(e) = renamed {
        return Err(kept(&e.to_string(), "rename_failed"));
    }
    match sync() {
        Ok(()) => {
            log::info!("token revocation marker verdict=cleared reason=signed_in");
            Ok(())
        }
        Err(why) if unsupported(&why) => {
            log::warn!(
                "token revocation marker verdict=cleared_not_durable reason=dir_sync_unsupported error={why}"
            );
            Ok(())
        }
        Err(why) => {
            log::warn!(
                "token revocation marker verdict=cleared_not_synced reason=folder_not_synced error={why}"
            );
            Err(std::io::Error::other(format!(
                "the marker's clearing could not be synced: {why}"
            )))
        }
    }
}

/// Whether the marker at `at` is one [`clear_marker`] cleared.
fn is_cleared(at: &Path) -> bool {
    std::fs::read_to_string(at).is_ok_and(|raw| raw.trim_end() == CLEARED)
}

/// Writes the marker whole (temp file, fsync, rename, folder sync). The
/// folder sync must succeed too: a marker a crash can lose is no marker.
/// The third line of a marker an unconfirmed install wrote.
const INSTALL_UNCONFIRMED: &str = "install_unconfirmed";

fn write_marker(
    path: &Path,
    user: Option<&str>,
    token: &str,
    install_unconfirmed: bool,
) -> std::io::Result<()> {
    #[cfg(debug_assertions)]
    if faults::MARKER.with(std::cell::Cell::get) {
        return Err(std::io::Error::other("marker write failed"));
    }
    let user = user.unwrap_or("");
    if user.contains('\n') {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "a value holds a line break",
        ));
    }
    let kind = if install_unconfirmed {
        INSTALL_UNCONFIRMED
    } else {
        "revoked"
    };
    put_marker(path, token, format!("{user}\n{token}\n{kind}\n").as_bytes())
}

/// Writes the marker naming `token` whole: temp file, fsync, rename,
/// folder sync.
fn put_marker(path: &Path, token: &str, contents: &[u8]) -> std::io::Result<()> {
    let target = revoked_path(path, token);
    let dir = path
        .parent()
        .ok_or_else(|| std::io::Error::other("no directory"))?;
    let mut tmp_name = target.as_os_str().to_owned();
    tmp_name.push(".new");
    let tmp = PathBuf::from(tmp_name);
    let go = || -> std::io::Result<()> {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(contents)?;
        f.sync_all()?;
        std::fs::rename(&tmp, &target)?;
        sync_dir(dir)
    };
    go().inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// Faults a test can set on its own thread, in debug builds only: the
/// token file's removal and read, and the marker's write and read.
#[cfg(debug_assertions)]
#[doc(hidden)]
pub mod faults {
    thread_local! {
        pub static REMOVE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
        pub static MARKER: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
        /// The rename that clears a marker fails.
        pub static MARKER_RENAME: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
        /// The marker cannot be read (it is there).
        pub static MARKER_READ: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
        /// The token file cannot be read in `end_session`.
        pub static TOKEN_READ: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
        /// A store cannot read its token back.
        pub static READ_BACK: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
        /// The token file's folder cannot be synced.
        pub static DIR_SYNC: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }
}

/// No token file: durable only once its folder syncs, since the absence may
/// be an earlier unlink whose sync failed (a crash could still bring the
/// file back). Called with the lock held.
fn absent(path: &Path) -> Forgotten {
    let Some(dir) = path.parent() else {
        return Forgotten::NotDurable("no directory".to_owned());
    };
    match durable_dir(dir) {
        Ok(()) => Forgotten::Durable,
        Err(reason) => {
            log::warn!(
                "token file absent but not synced dir={} reason={reason} verdict=absence_not_durable",
                dir.display()
            );
            Forgotten::NotDurable(reason)
        }
    }
}

/// Removes the file and syncs its folder. Called with the lock held.
fn remove(path: &Path) -> Result<Forgotten, ContentError> {
    #[cfg(debug_assertions)]
    if faults::REMOVE.with(std::cell::Cell::get) {
        return Err(err(path, "removal failed"));
    }
    match std::fs::remove_file(path) {
        Ok(()) => changed("removed"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(absent(path)),
        Err(e) => return Err(err(path, e)),
    }
    // The pairing goes with its token. One left behind pairs nothing but
    // that same token, so a failure here is only logged.
    match std::fs::remove_file(user_path(path)) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => log::warn!("token pairing verdict=not_removed error={e}"),
    }
    let Some(dir) = path.parent() else {
        return Ok(Forgotten::NotDurable("no directory".to_owned()));
    };
    Ok(match durable_dir(dir) {
        Ok(()) => Forgotten::Durable,
        Err(reason) => {
            log::warn!(
                "token file removed but not synced dir={} reason={reason} verdict=signed_out_not_durable",
                dir.display()
            );
            Forgotten::NotDurable(reason)
        }
    })
}

/// Syncs `dir`, once more if the first sync fails. Err with the reason when
/// the second sync fails too. EINVAL or ENOTSUP (a file system that cannot
/// sync a folder) is a failure here, named as unsupported: the removal
/// never reached the disk, so a sign-out is not claimed durable.
/// How [`durable_dir`] names a folder that cannot be synced at all.
const DIR_SYNC_UNSUPPORTED: &str = "directory sync unsupported";

fn durable_dir(dir: &Path) -> Result<(), String> {
    let Err(first) = sync_dir(dir) else {
        return Ok(());
    };
    let unsupported = |e: &std::io::Error| {
        matches!(
            e.kind(),
            std::io::ErrorKind::InvalidInput | std::io::ErrorKind::Unsupported
        )
    };
    match sync_dir(dir) {
        Ok(()) => {
            log::info!("token file dir sync verdict=retried first_error={first}");
            Ok(())
        }
        // Unsupported only when both attempts say so: any other first error
        // is a real failure, whatever the retry reports.
        Err(e) if unsupported(&first) && unsupported(&e) => {
            Err(format!("{DIR_SYNC_UNSUPPORTED}: {e}"))
        }
        Err(e) => Err(format!("{e} (first attempt: {first})")),
    }
}

/// The pairing file beside the token file: the user id on its first line
/// and the fingerprint of the token it was paired with on its second
/// ([`read_raw`]). One without a fingerprint (an earlier layout's) pairs
/// nothing.
pub fn user_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".user");
    PathBuf::from(name)
}

/// Writes the file whole to a temp file of its own (not one any other writer
/// of the directory uses), fsyncs it, renames it over `path` and syncs the
/// directory. Called with the lock held.
///
/// Once the rename has installed the new file it is the file: every reader
/// sees it, so it is the active token. A directory sync that fails after
/// that only leaves the rename less durable (a crash may bring the old file
/// back); it is logged with its reason and the write succeeds, so the
/// caller's memory and hooks follow the file. A failure before the install
/// changes nothing and is an error.
fn write(path: &Path, token: &str) -> std::io::Result<()> {
    #[cfg(test)]
    if tests::FAIL_WRITE.with(std::cell::Cell::get) {
        return Err(std::io::Error::other("token write failed"));
    }
    install(path, token, &format!("{token}\n"))
}

/// Pairs `user` with `token` in the pairing file, written whole as the
/// token file is. Called with the lock held, after the token is on file.
/// Unlinks the pairing file. Called with the lock held.
fn unlink_pairing(path: &Path) -> std::io::Result<()> {
    #[cfg(test)]
    if tests::FAIL_PAIR_REMOVE.with(std::cell::Cell::get) {
        return Err(std::io::Error::other("pairing removal failed"));
    }
    std::fs::remove_file(user_path(path))
}

/// Unlinks the pairing file and syncs its folder, so a crash cannot bring
/// it back. Called with the lock held.
fn remove_pairing(path: &Path) -> Result<(), String> {
    match unlink_pairing(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.to_string()),
    }
    let dir = path.parent().ok_or("no directory")?;
    durable_dir(dir)
}

fn write_pair(path: &Path, user: &str, token: &str) -> std::io::Result<()> {
    #[cfg(test)]
    if tests::FAIL_WRITE.with(std::cell::Cell::get) {
        return Err(std::io::Error::other("pairing write failed"));
    }
    install(
        &user_path(path),
        user,
        &format!("{user}\n{}\n", fingerprint(token)),
    )
}

/// Installs `contents` at `path` whole (see [`write`]); `value` is what a
/// line break may not appear in.
fn install(path: &Path, value: &str, contents: &str) -> std::io::Result<()> {
    if value.contains('\n') {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "a value holds a line break",
        ));
    }
    let dir = path
        .parent()
        .ok_or_else(|| std::io::Error::other("no directory"))?;
    std::fs::create_dir_all(dir)?;
    let mut tmp_name = path.as_os_str().to_owned();
    tmp_name.push(".new");
    let tmp = PathBuf::from(tmp_name);
    let go = || -> std::io::Result<()> {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(contents.as_bytes())?;
        f.sync_all()?;
        if f.metadata()?.permissions().mode() & 0o077 != 0 {
            f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        std::fs::rename(&tmp, path)
    };
    go().inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })?;
    if let Err(e) = sync_dir(dir) {
        log::warn!(
            "token file installed but not synced dir={} reason={e}",
            dir.display()
        );
    }
    Ok(())
}

/// Flushes the directory entry of a rename to disk.
fn sync_dir(dir: &Path) -> std::io::Result<()> {
    #[cfg(debug_assertions)]
    if faults::DIR_SYNC.with(std::cell::Cell::get) {
        return Err(std::io::Error::other("directory sync failed"));
    }
    #[cfg(test)]
    if tests::FAIL_DIR_SYNC.with(std::cell::Cell::get) {
        return Err(std::io::Error::other("directory sync failed"));
    }
    #[cfg(test)]
    if let Some(kind) = tests::DIR_SYNC_ERRORS.with(|q| {
        let mut q = q.borrow_mut();
        (!q.is_empty()).then(|| q.remove(0))
    }) {
        return Err(std::io::Error::new(kind, "directory sync failed"));
    }
    std::fs::File::open(dir)?.sync_all()
}

#[cfg(test)]
pub(crate) mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::cell::RefCell;

    use super::*;

    thread_local! {
        /// While set, store cannot read the token it replaces.
        pub(super) static FAIL_PREVIOUS_READ: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
        /// While set, a store's marker clearing reports its folder sync failed.
        pub(super) static FAIL_MARKER_SYNC: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
        /// While set, a store cannot read back the token it wrote.
        pub(crate) static FAIL_READ_BACK: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
        /// While set, writing the token file fails before anything changes.
        pub(super) static FAIL_WRITE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
        /// While set, syncing the directory after a rename fails.
        /// Unlinking the pairing file fails.
        pub(super) static FAIL_PAIR_REMOVE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
        pub(super) static FAIL_DIR_SYNC: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
        /// The next directory syncs fail with these, one each.
        pub(crate) static DIR_SYNC_ERRORS: RefCell<Vec<std::io::ErrorKind>> = const { RefCell::new(Vec::new()) };
        /// Runs inside `read`, between the token's read and the marker's.
        pub(super) static AFTER_RAW_READ: RefCell<Option<Box<dyn FnOnce()>>> = const { RefCell::new(None) };
        /// Runs inside `store_user`, between its read and its write.
        static BETWEEN: RefCell<Option<Box<dyn Fn()>>> = const { RefCell::new(None) };
    }

    pub(super) fn between_read_and_write() {
        BETWEEN.with(|b| {
            if let Some(f) = b.borrow().as_ref() {
                f();
            }
        });
    }

    /// A folder of the test's own, removed when the test ends.
    fn dir(tag: &str) -> crate::testdir::TmpDir {
        crate::testdir::TmpDir::new(&format!("token-{tag}"))
    }

    /// Body item android.rs:221: a run's fence and token come from one
    /// hold of the lock. A sign-in that stores B and a sign-out that fences
    /// B, both landing while the run reads, wait for it: the run sends A,
    /// the token on file when it read, never B under the fence it read
    /// before B was stored.
    #[test]
    fn a_runs_fence_and_token_are_read_together() {
        let d = dir("run-read");
        let p = d.join("device-token");
        store(&p, "tokA").unwrap();
        store_user(&p, "A").unwrap();
        let other: std::sync::Arc<Mutex<Option<std::thread::JoinHandle<()>>>> =
            std::sync::Arc::default();
        {
            let (p, other) = (p.clone(), other.clone());
            BETWEEN.with(|b| {
                *b.borrow_mut() = Some(Box::new(move || {
                    let p = p.clone();
                    let t = std::thread::spawn(move || {
                        store(&p, "tokB").unwrap();
                        add_fence(&p, &Fence::token(&fingerprint("tokB")), "test");
                    });
                    // Long enough for an unlocked sign-in and sign-out.
                    std::thread::sleep(std::time::Duration::from_millis(150));
                    *other.lock().unwrap() = Some(t);
                }));
            });
        }
        let read = read_for_run(&p).unwrap();
        BETWEEN.with(|b| *b.borrow_mut() = None);
        other.lock().unwrap().take().unwrap().join().unwrap();
        assert_eq!(
            read,
            RunRead::Token(Stored {
                token: "tokA".into(),
                user: Some("A".into())
            }),
            "the run mixed a fence read before a store with the token after it"
        );
        assert_eq!(read_for_run(&p).unwrap(), RunRead::Fenced);
    }

    /// A sign-in that lands while a user id is being paired with the old
    /// token waits for the pairing: the new token survives, with no user
    /// (its own is not known yet), never the old token or the old user.
    /// The competing sign-in then pairs B with its token, and that pairing
    /// is what a read finds (CR 4642574379): an unlocked pairing of A
    /// landing after it would lose it.
    #[test]
    fn a_sign_in_during_a_user_pairing_keeps_the_new_token() {
        let d = dir("interleave");
        let p = d.join("device-token");
        store(&p, "tokA").unwrap();
        let sign_in: std::sync::Arc<Mutex<Option<std::thread::JoinHandle<()>>>> =
            std::sync::Arc::default();
        {
            let (p, sign_in) = (p.clone(), sign_in.clone());
            BETWEEN.with(|b| {
                *b.borrow_mut() = Some(Box::new(move || {
                    let p = p.clone();
                    let t = std::thread::spawn(move || {
                        store(&p, "tokB").unwrap();
                        assert_eq!(store_user_if(&p, "tokB", "B").unwrap(), Pairing::Paired);
                    });
                    // Long enough for an unlocked sign-in to finish first.
                    std::thread::sleep(std::time::Duration::from_millis(150));
                    *sign_in.lock().unwrap() = Some(t);
                }));
            });
        }
        store_user(&p, "A").unwrap();
        BETWEEN.with(|b| *b.borrow_mut() = None);
        sign_in.lock().unwrap().take().unwrap().join().unwrap();
        assert_eq!(
            read(&p).unwrap(),
            Some(Stored {
                token: "tokB".into(),
                user: Some("B".into())
            }),
            "the sign-in or its pairing was overwritten by A's pairing"
        );
    }

    /// Body item token.rs:527: A's token, replaced by B's and then stored
    /// again, comes back unpaired: the pairing from its first install does
    /// not name A for a sign-in that has not confirmed A. A's own pairing
    /// then works as before.
    #[test]
    fn a_re_stored_token_does_not_keep_its_earlier_pairing() {
        let d = dir("re-store-pairing");
        let p = d.join("device-token");
        store(&p, "tokA").unwrap();
        store_user(&p, "A").unwrap();
        assert_eq!(read(&p).unwrap().unwrap().user.as_deref(), Some("A"));
        store(&p, "tokB").unwrap();
        assert_eq!(read(&p).unwrap().unwrap().user, None);
        store(&p, "tokA").unwrap();
        assert_eq!(
            read(&p).unwrap().unwrap().user,
            None,
            "the re-stored token kept the user of its first install"
        );
        assert_eq!(store_user_if(&p, "tokA", "A").unwrap(), Pairing::Paired);
        assert_eq!(read(&p).unwrap().unwrap().user.as_deref(), Some("A"));
        // Storing the token already on file keeps its pairing.
        store(&p, "tokA").unwrap();
        assert_eq!(read(&p).unwrap().unwrap().user.as_deref(), Some("A"));
    }

    /// A new token stored between the request that named the user and the
    /// pairing (another account's Quick Connect approved meanwhile) is not
    /// paired with that user: the pairing is refused and the new token stays
    /// with no user.
    #[test]
    fn a_pairing_for_a_replaced_token_is_refused() {
        let d = dir("pair-if");
        let p = d.join("device-token");
        store(&p, "tokA").unwrap();
        assert_eq!(store_user_if(&p, "tokA", "A").unwrap(), Pairing::Paired);
        assert_eq!(read(&p).unwrap().unwrap().user.as_deref(), Some("A"));
        store(&p, "tokB").unwrap();
        assert_eq!(
            read(&p).unwrap().unwrap().user,
            None,
            "a new token kept the old user"
        );
        assert_eq!(
            store_user_if(&p, "tokA", "A").unwrap(),
            Pairing::TokenChanged
        );
        assert_eq!(
            read(&p).unwrap(),
            Some(Stored {
                token: "tokB".into(),
                user: None
            }),
            "the new token was paired with the old user"
        );
    }

    /// CR 4639801671: a store whose folder sync fails is refused, the new
    /// token with it: no token is read that was not synced in full. A folder
    /// that cannot be synced at all (ENOTSUP) is no failure of this store:
    /// it installs, and says so.
    #[test]
    fn a_failed_directory_sync_refuses_the_new_token_unless_unsupported() {
        let d = dir("dirsync");
        let path = d.join("device-token");
        store(&path, "old").unwrap();
        crate::testlog::start();
        FAIL_DIR_SYNC.with(|f| f.set(true));
        let stored = store(&path, "new");
        FAIL_DIR_SYNC.with(|f| f.set(false));
        assert!(stored.is_err(), "{stored:?}");
        assert_eq!(load(&path).unwrap(), None);
        let lines = crate::testlog::lines();
        assert!(
            lines.iter().any(|l| l.contains("reason=folder_not_synced")),
            "{lines:?}"
        );
        let unsupported = d.join("other-token");
        DIR_SYNC_ERRORS.with(|q| {
            q.borrow_mut().extend([std::io::ErrorKind::Unsupported; 3]);
        });
        let stored = store(&unsupported, "new");
        DIR_SYNC_ERRORS.with(|q| q.borrow_mut().clear());
        assert!(stored.is_ok(), "{stored:?}");
        assert_eq!(load(&unsupported).unwrap().as_deref(), Some("new"));
        // A real first failure is not unsupported, whatever the retry says
        // (the write's own sync takes the first error).
        let mixed = d.join("mixed-token");
        DIR_SYNC_ERRORS.with(|q| {
            q.borrow_mut().extend([
                std::io::ErrorKind::Other,
                std::io::ErrorKind::Other,
                std::io::ErrorKind::Unsupported,
            ]);
        });
        let stored = store(&mixed, "new");
        DIR_SYNC_ERRORS.with(|q| q.borrow_mut().clear());
        assert!(stored.is_err(), "{stored:?}");
        assert_eq!(load(&mixed).unwrap(), None);
    }

    /// A sign-out's removal is synced to the folder: a failed sync is tried
    /// once more. A sync that still fails, EINVAL or ENOTSUP included (the
    /// folder cannot be synced, so the removal never reached the disk),
    /// leaves the token gone (signed out) and reports why, naming an
    /// unsupported sync as such.
    /// A refused token is removed only while it is still the one on file:
    /// a sign-in that stored another since keeps it, with its user.
    #[test]
    fn a_refused_token_is_forgotten_only_while_it_is_on_file() {
        let d = dir("forget-if");
        let p = d.join("device-token");
        store(&p, "tokB").unwrap();
        store_user(&p, "B").unwrap();
        assert_eq!(forget_if(&p, "tokA").unwrap(), ForgetIf::Changed);
        assert_eq!(
            read(&p).unwrap(),
            Some(Stored {
                token: "tokB".to_owned(),
                user: Some("B".to_owned())
            }),
            "the replacement was removed with the refused token"
        );
        assert_eq!(
            forget_if(&p, "tokB").unwrap(),
            ForgetIf::Forgotten(Forgotten::Durable)
        );
        assert_eq!(read(&p).unwrap(), None);
        assert_eq!(
            forget_if(&p, "tokB").unwrap(),
            ForgetIf::Forgotten(Forgotten::Durable)
        );
    }

    #[test]
    fn a_forgotten_token_is_synced_and_a_failed_sync_is_reported() {
        use std::io::ErrorKind::{InvalidInput, Other, Unsupported};
        let d = dir("forget-sync");
        let p = d.join("device-token");
        for (errors, reason) in [
            (vec![], None),
            (vec![Other], None),
            (
                vec![InvalidInput, InvalidInput],
                Some("directory sync unsupported"),
            ),
            // CR 4640059241: unsupported only when both attempts say so.
            (vec![Other, Unsupported], Some("directory sync failed")),
            (vec![Unsupported, Other], Some("directory sync failed")),
            (vec![Other, Other], Some("directory sync failed")),
        ] {
            store(&p, "tok").unwrap();
            crate::testlog::start();
            DIR_SYNC_ERRORS.with(|q| *q.borrow_mut() = errors.clone());
            let forgot = forget(&p).unwrap();
            let left = DIR_SYNC_ERRORS.with(|q| q.borrow().len());
            DIR_SYNC_ERRORS.with(|q| q.borrow_mut().clear());
            assert_eq!(read(&p).unwrap(), None, "the token outlived the sign-out");
            assert_eq!(left, 0, "{errors:?}: not every sync was tried");
            match reason {
                None => assert_eq!(forgot, Forgotten::Durable, "{errors:?}"),
                Some(reason) => {
                    assert!(
                        matches!(&forgot, Forgotten::NotDurable(why) if why.contains(reason)),
                        "{errors:?}: {forgot:?}"
                    );
                    let named_unsupported = matches!(
                        &forgot,
                        Forgotten::NotDurable(why) if why.contains(DIR_SYNC_UNSUPPORTED)
                    );
                    assert_eq!(
                        named_unsupported,
                        reason == DIR_SYNC_UNSUPPORTED,
                        "{errors:?}: {forgot:?}"
                    );
                    let lines = crate::testlog::lines();
                    assert!(
                        lines
                            .iter()
                            .any(|l| l.contains("token file removed but not synced")
                                && l.contains(reason)
                                && l.contains("verdict=signed_out_not_durable")),
                        "{errors:?}: {lines:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn the_token_and_its_user_round_trip_privately_and_forget() {
        let d = dir("round");
        let p = d.join("files/device-token");
        assert_eq!(read(&p).unwrap(), None);
        store(&p, "secret-1").unwrap();
        store(&p, "secret-2\n").unwrap();
        assert_eq!(load(&p).unwrap().as_deref(), Some("secret-2"));
        assert_eq!(read(&p).unwrap().unwrap().user, None);
        store_user(&p, "u1").unwrap();
        assert_eq!(
            read(&p).unwrap(),
            Some(Stored {
                token: "secret-2".into(),
                user: Some("u1".into())
            })
        );
        assert_eq!(load(&p).unwrap().as_deref(), Some("secret-2"));
        let mode = std::fs::metadata(&p).unwrap().permissions().mode();
        assert_eq!(mode & 0o077, 0, "{mode:o}");
        forget(&p).unwrap();
        forget(&p).unwrap();
        assert_eq!(load(&p).unwrap(), None);
    }

    #[test]
    fn a_plain_token_file_has_no_user() {
        // The older layout, and a file written by hand.
        for raw in ["tok", "tok\n", "tok\n\n", "  tok  \n"] {
            let s = decode(raw).unwrap();
            assert_eq!((s.token.as_str(), s.user), ("tok", None), "{raw:?}");
        }
        assert_eq!(decode(""), None);
        assert_eq!(decode("\nu1\n"), None, "a user with no token is nobody");
    }

    #[test]
    fn a_user_cannot_be_kept_with_no_token() {
        let d = dir("none");
        assert!(store_user(&d.join("device-token"), "u1").is_err());
    }

    #[test]
    fn a_legacy_sidecar_is_never_trusted() {
        let d = dir("legacy");
        let p = d.join("device-token");
        std::fs::write(&p, "tokB").unwrap();
        std::fs::write(user_path(&p), "A").unwrap();
        assert_eq!(read(&p).unwrap().unwrap().user, None);
    }

    #[test]
    fn a_failed_write_leaves_the_old_pair() {
        let d = dir("failed");
        let p = d.join("device-token");
        store(&p, "tokA").unwrap();
        store_user(&p, "A").unwrap();
        // A line break would corrupt the format: refused, nothing changed.
        assert!(store(&p, "to\nk").is_err());
        assert_eq!(read(&p).unwrap().unwrap().user.as_deref(), Some("A"));
        assert!(!d.join("device-token.new").exists());
    }

    #[test]
    fn an_unreadable_path_names_the_path() {
        let d = dir("bad");
        let e = load(&d).expect_err("a directory is not a token");
        assert!(e.to_string().contains(d.to_str().unwrap()), "{e}");
    }

    /// A session's token that cannot be removed is marked revoked: no read
    /// returns it, the marker names its user, a replacement is kept, and a
    /// new sign-in's token is read again (the marker cleared).
    // Fault injection exists in debug builds only.
    #[cfg(debug_assertions)]
    #[test]
    fn a_token_that_will_not_go_is_marked_and_never_read_again() {
        let d = dir("marked");
        let p = d.join("device-token");
        store(&p, "tokA").unwrap();
        store_user(&p, "A").unwrap();
        assert_eq!(end_session(&p, Some("tokB"), None), Ended::Changed);
        faults::REMOVE.with(|f| f.set(true));
        let ended = end_session(&p, Some("tokA"), None);
        faults::REMOVE.with(|f| f.set(false));
        assert!(matches!(ended, Ended::Marked(_)), "{ended:?}");
        assert_eq!(ended.label(), "marked_revoked");
        assert!(read_raw(&p).unwrap().is_some(), "the fault removed it");
        assert_eq!(read(&p).unwrap(), None, "a revoked token was read");
        assert_eq!(load(&p).unwrap(), None);
        assert_eq!(
            revoked(&p, &fingerprint("tokA")).unwrap().user.as_deref(),
            Some("A")
        );
        store(&p, "tokC").unwrap();
        assert_eq!(load(&p).unwrap().as_deref(), Some("tokC"));
        // Another token's marker is never store's to remove: it refuses
        // only tokA, which is gone.
        assert!(
            revoked(&p, &fingerprint("tokA")).is_some(),
            "the sign-in removed A's marker"
        );
    }

    /// A read sees the token and its marker as one: a sign-in that replaces
    /// a revoked token (and clears its marker) between the two reads waits
    /// for the read, which refuses the revoked token rather than pairing it
    /// with the cleared marker.
    // Fault injection exists in debug builds only.
    #[cfg(debug_assertions)]
    #[test]
    fn a_read_never_pairs_an_old_token_with_a_new_marker_state() {
        let d = dir("read-locked");
        let p = d.join("device-token");
        store(&p, "tokA").unwrap();
        faults::REMOVE.with(|f| f.set(true));
        assert!(matches!(end_session(&p, None, Some("A")), Ended::Marked(_)));
        faults::REMOVE.with(|f| f.set(false));
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let p2 = p.clone();
        let reader = std::thread::spawn(move || {
            AFTER_RAW_READ.with(|h| {
                *h.borrow_mut() = Some(Box::new(move || {
                    let _ = tx.send(());
                    std::thread::sleep(std::time::Duration::from_millis(150));
                }));
            });
            load(&p2).unwrap()
        });
        rx.recv().unwrap();
        // The competing writer clears the very marker the reader checks,
        // under the file's lock, as a store of a token does. (A store of
        // tokA itself is refused while tokA is on file: same_token_revoked.)
        {
            let _held = hold();
            std::fs::remove_file(revoked_path(&p, &fingerprint("tokA"))).unwrap();
        }
        assert_eq!(reader.join().unwrap(), None, "the revoked token was read");
        assert_eq!(load(&p).unwrap().as_deref(), Some("tokA"));
    }

    /// CR 4642574358: the marker a store's unconfirmed install left is not
    /// a revocation. Once the folder syncs, the same approved token
    /// installs; a genuine revocation of the token on file still refuses
    /// it (`same_token_revoked`, tested above).
    #[test]
    fn an_unconfirmed_install_retries_with_the_same_token() {
        let d = dir("dirsync-retry");
        let path = d.join("device-token");
        store(&path, "old").unwrap();
        FAIL_DIR_SYNC.with(|f| f.set(true));
        let stored = store(&path, "new");
        FAIL_DIR_SYNC.with(|f| f.set(false));
        assert!(stored.is_err(), "{stored:?}");
        assert_eq!(load(&path).unwrap(), None);
        assert!(revoked(&path, &fingerprint("new")).is_some_and(|r| r.install_unconfirmed));
        crate::testlog::start();
        store(&path, "new").expect("the retried install was refused");
        assert_eq!(load(&path).unwrap().as_deref(), Some("new"));
        assert!(crate::testlog::lines()
            .iter()
            .any(|l| l.contains("token store verdict=retry reason=install_unconfirmed")));
    }

    /// CR 4644224038: a retry whose write fails keeps its unconfirmed
    /// install's marker, so after a restart (the fence reset) the token on
    /// file is still refused. The marker goes only once a retry's write and
    /// folder sync hold.
    #[test]
    fn a_failed_retry_keeps_the_token_refused_after_a_restart() {
        let d = dir("retry-write-fails");
        let path = d.join("device-token");
        store(&path, "old").unwrap();
        FAIL_DIR_SYNC.with(|f| f.set(true));
        assert!(store(&path, "new").is_err());
        FAIL_DIR_SYNC.with(|f| f.set(false));
        FAIL_WRITE.with(|f| f.set(true));
        let retried = store(&path, "new");
        FAIL_WRITE.with(|f| f.set(false));
        assert!(retried.is_err(), "{retried:?}");
        assert!(
            revoked(&path, &fingerprint("new")).is_some(),
            "the failed retry removed the marker"
        );
        assert_eq!(
            after_restart(&path),
            None,
            "the unconfirmed token became readable"
        );
        crate::testlog::start();
        store(&path, "new").unwrap();
        assert!(revoked(&path, &fingerprint("new")).is_none());
        assert_eq!(load(&path).unwrap().as_deref(), Some("new"));
        assert!(crate::testlog::lines()
            .iter()
            .any(|l| l.contains("verdict=retried reason=install_unconfirmed marker=cleared")));
    }

    /// CR reply 4644656366: a retry whose marker clearing cannot be synced
    /// keeps the marker as it was, so after a restart (the fence reset) the
    /// token on file is still refused.
    #[test]
    fn a_retry_whose_marker_clearing_is_not_synced_keeps_the_marker() {
        let d = dir("retry-marker-sync");
        let path = d.join("device-token");
        store(&path, "old").unwrap();
        FAIL_DIR_SYNC.with(|f| f.set(true));
        assert!(store(&path, "new").is_err());
        FAIL_DIR_SYNC.with(|f| f.set(false));
        crate::testlog::start();
        FAIL_MARKER_SYNC.with(|f| f.set(true));
        let retried = store(&path, "new");
        FAIL_MARKER_SYNC.with(|f| f.set(false));
        assert!(retried.is_err(), "{retried:?}");
        assert!(
            revoked(&path, &fingerprint("new")).is_some_and(|r| r.install_unconfirmed),
            "the marker was not put back as it was"
        );
        assert_eq!(load(&path).unwrap(), None, "read in this process");
        assert_eq!(after_restart(&path), None, "the token became readable");
        assert!(crate::testlog::lines()
            .iter()
            .any(|l| l.contains("token revocation marker verdict=kept reason=folder_not_synced")));
    }

    /// Outside diff token.rs:600-602 (review 5997638687): A's pairing
    /// survives A to B, its unlink failing. Storing A again does not take
    /// it back up: the stale pairing is cleared before the rename, or the
    /// store is refused with B left on file. The control: with the unlink
    /// working, A installs unpaired.
    #[test]
    fn an_earlier_installs_pairing_is_not_taken_back_up() {
        let d = dir("pairing-earlier-install");
        let path = d.join("device-token");
        store(&path, "tokA").unwrap();
        store_user(&path, "u1").unwrap();
        FAIL_PAIR_REMOVE.with(|f| f.set(true));
        store(&path, "tokB").unwrap();
        assert!(
            user_path(&path).exists(),
            "the failed unlink left no pairing"
        );
        crate::testlog::start();
        let again = store(&path, "tokA");
        FAIL_PAIR_REMOVE.with(|f| f.set(false));
        assert!(again.is_err(), "{again:?}");
        assert_eq!(load(&path).unwrap().as_deref(), Some("tokB"));
        assert!(crate::testlog::lines()
            .iter()
            .any(|l| l.contains("token store verdict=refused reason=earlier_pairing_not_cleared")));
        store(&path, "tokA").unwrap();
        let back = read(&path).unwrap().unwrap();
        assert_eq!(back.token, "tokA");
        assert_eq!(back.user, None, "an earlier install's user came back");
    }

    /// CR 4646025360: a marker is never unlinked, so no failure while it is
    /// cleared leaves the token with no marker. A retry whose clearing
    /// fails, with every marker write failing too (what used to put an
    /// unlinked marker back), leaves the old marker on disk: after a
    /// restart the token is still refused. Each failing step is tried: the
    /// folder sync and the rename.
    // Fault injection exists in debug builds only.
    #[cfg(debug_assertions)]
    #[test]
    fn a_failed_marker_clearing_never_leaves_the_token_unguarded() {
        for step in ["folder_not_synced", "rename_failed"] {
            let d = dir(&format!("marker-clear-{step}"));
            let path = d.join("device-token");
            store(&path, "old").unwrap();
            FAIL_DIR_SYNC.with(|f| f.set(true));
            assert!(store(&path, "new").is_err());
            FAIL_DIR_SYNC.with(|f| f.set(false));
            let marker_file = revoked_path(&path, &fingerprint("new"));
            let before = std::fs::read(&marker_file).unwrap();
            crate::testlog::start();
            let fault = |on: bool| {
                faults::MARKER.with(|f| f.set(on));
                if step == "rename_failed" {
                    faults::MARKER_RENAME.with(|f| f.set(on));
                } else {
                    FAIL_MARKER_SYNC.with(|f| f.set(on));
                }
            };
            fault(true);
            let retried = store(&path, "new");
            fault(false);
            assert!(retried.is_err(), "{step}: {retried:?}");
            assert_eq!(
                std::fs::read(&marker_file).unwrap(),
                before,
                "{step}: the marker on disk changed"
            );
            assert_eq!(
                after_restart(&path),
                None,
                "{step}: the token became readable"
            );
            assert!(crate::testlog::lines().iter().any(|l| l.contains(&format!(
                "token revocation marker verdict=kept reason={step}"
            ))));
        }
    }

    /// A cleared marker reads as no marker, and the clearing renames over
    /// the marker instead of unlinking it: the file stays.
    #[test]
    fn a_cleared_marker_stays_on_disk_and_reads_as_absent() {
        let d = dir("marker-cleared");
        let p = d.join("device-token");
        store(&p, "tokA").unwrap();
        write_marker(&p, Some("A"), &fingerprint("tokN"), false).unwrap();
        store(&p, "tokN").unwrap();
        let at = revoked_path(&p, &fingerprint("tokN"));
        assert!(at.exists(), "the marker was unlinked");
        assert_eq!(marker(&p, &fingerprint("tokN")), Marker::Absent);
        assert_eq!(load(&p).unwrap().as_deref(), Some("tokN"));
    }

    /// A sign-out retried after an unlink whose folder sync failed is
    /// durable only once the folder syncs: the absence alone is not.
    #[test]
    fn a_retried_sign_out_is_durable_only_once_the_folder_syncs() {
        let d = dir("absent-durable");
        let p = d.join("device-token");
        store(&p, "tokA").unwrap();
        FAIL_DIR_SYNC.with(|f| f.set(true));
        let first = forget(&p).unwrap();
        let retry = forget(&p).unwrap();
        FAIL_DIR_SYNC.with(|f| f.set(false));
        assert!(matches!(first, Forgotten::NotDurable(_)), "{first:?}");
        assert!(
            matches!(retry, Forgotten::NotDurable(_)),
            "an unsynced absence was reported durable: {retry:?}"
        );
        assert_eq!(forget(&p).unwrap(), Forgotten::Durable);
    }

    /// (a) A marker is its file name: one whose contents cannot be parsed
    /// still refuses its own token, and one whose presence cannot be told
    /// refuses it too (fail closed). A missing one refuses nothing, and a
    /// marker of one token never refuses another.
    // Fault injection exists in debug builds only.
    #[cfg(debug_assertions)]
    #[test]
    fn an_unreadable_marker_fails_closed_and_a_missing_one_is_absent() {
        let d = dir("marker-unreadable");
        let p = d.join("device-token");
        store(&p, "tokA").unwrap();
        assert_eq!(marker(&p, &fingerprint("tokA")), Marker::Absent);
        assert_eq!(load(&p).unwrap().as_deref(), Some("tokA"));
        faults::MARKER_READ.with(|f| f.set(true));
        let read_failed = load(&p).unwrap();
        faults::MARKER_READ.with(|f| f.set(false));
        assert_eq!(
            read_failed, None,
            "an unreadable marker let the token through"
        );
        std::fs::write(revoked_path(&p, &fingerprint("tokA")), "\0garbage").unwrap();
        assert!(matches!(
            marker(&p, &fingerprint("tokA")),
            Marker::Present(_)
        ));
        assert_eq!(load(&p).unwrap(), None, "an unparsed marker let it through");
        // A new sign-in is not refused by A's marker, which stays.
        store(&p, "tokB").unwrap();
        assert_eq!(load(&p).unwrap().as_deref(), Some("tokB"));
        assert!(revoked_path(&p, &fingerprint("tokA")).exists());
    }

    /// (b) A token that cannot be read to compare is not removed (it may
    /// be a replacement); the token the session ended with is marked for
    /// its user, and with no token named the end is unsafe.
    // Fault injection exists in debug builds only.
    #[cfg(debug_assertions)]
    #[test]
    fn an_unreadable_token_is_not_removed_and_the_ended_one_is_marked() {
        let d = dir("token-unreadable");
        let p = d.join("device-token");
        store(&p, "tokA").unwrap();
        faults::TOKEN_READ.with(|f| f.set(true));
        let ended = end_session(&p, Some("tokA"), Some("A"));
        let unnamed = end_session(&p, None, Some("A"));
        faults::TOKEN_READ.with(|f| f.set(false));
        assert!(matches!(ended, Ended::Marked(_)), "{ended:?}");
        assert!(
            read_raw(&p).unwrap().is_some(),
            "an uncompared token was removed"
        );
        let r = revoked(&p, &fingerprint("tokA")).expect("no marker");
        assert_eq!(r.user.as_deref(), Some("A"));
        assert_eq!(load(&p).unwrap(), None, "the ended token was read");
        assert!(matches!(unnamed, Ended::Unsafe { .. }), "{unnamed:?}");
    }

    /// CR 4637783569: a refusal for A that arrives while the file cannot
    /// be read does not replace B's marker: markers are one per token, so
    /// A's is written beside B's and both stay refused.
    // Fault injection exists in debug builds only.
    #[cfg(debug_assertions)]
    #[test]
    fn an_unreadable_token_keeps_another_tokens_marker() {
        let d = dir("token-unreadable-other-marker");
        let p = d.join("device-token");
        store(&p, "tokB").unwrap();
        faults::REMOVE.with(|f| f.set(true));
        let b = end_session(&p, Some("tokB"), Some("B"));
        faults::REMOVE.with(|f| f.set(false));
        assert!(matches!(b, Ended::Marked(_)), "{b:?}");
        faults::TOKEN_READ.with(|f| f.set(true));
        let a = end_session(&p, Some("tokA"), Some("A"));
        faults::TOKEN_READ.with(|f| f.set(false));
        assert!(matches!(&a, Ended::Marked(_)), "{a:?}");
        assert_eq!(
            revoked(&p, &fingerprint("tokB")).map(|r| r.user),
            Some(Some("B".to_owned()))
        );
        assert_eq!(
            revoked(&p, &fingerprint("tokA")).map(|r| r.user),
            Some(Some("A".to_owned()))
        );
        assert_eq!(load(&p).unwrap(), None, "B was read after its marker went");
    }

    /// CR 4638691172, 4638660897: a sign-out whose token can be neither
    /// removed nor marked, and cannot be read to be named, fences every
    /// token at that path, inside the operation and under the file's lock.
    /// A sign-in stored after it is read: the store replaces the fence in
    /// the same lock, so neither ordering refuses the new token.
    #[cfg(debug_assertions)]
    #[test]
    fn an_unnamed_unsafe_sign_out_fences_every_token_until_a_store() {
        let d = dir("fence-unnamed");
        let p = d.join("device-token");
        store(&p, "tokA").unwrap();
        faults::TOKEN_READ.with(|f| f.set(true));
        let ended = end_session(&p, None, None);
        faults::TOKEN_READ.with(|f| f.set(false));
        assert!(
            matches!(&ended, Ended::Unsafe { target: None, .. }),
            "{ended:?}"
        );
        assert_eq!(fence(&p), Fence::Every);
        assert_eq!(load(&p).unwrap(), None, "the unnamed token was read");
        store(&p, "tokB").unwrap();
        assert_eq!(load(&p).unwrap().as_deref(), Some("tokB"));
    }

    /// CR 4638691172, 4638660989: a sign-in stored after the caller read
    /// A and before the sign-out ran is the token the sign-out targets.
    /// When it can be neither removed nor marked, the fence names B, the
    /// token on file, not A from the earlier read.
    #[cfg(debug_assertions)]
    #[test]
    fn an_unsafe_sign_out_fences_the_token_it_targeted() {
        let d = dir("fence-target");
        let p = d.join("device-token");
        store(&p, "tokA").unwrap();
        let held = load(&p).unwrap();
        store(&p, "tokB").unwrap();
        faults::REMOVE.with(|f| f.set(true));
        faults::MARKER.with(|f| f.set(true));
        let ended = end_session(&p, None, None);
        faults::REMOVE.with(|f| f.set(false));
        faults::MARKER.with(|f| f.set(false));
        assert!(
            matches!(&ended, Ended::Unsafe { target: Some(t), .. } if *t == fingerprint("tokB")),
            "{ended:?}"
        );
        assert_eq!(held.as_deref(), Some("tokA"));
        assert_eq!(fence(&p), Fence::token(&fingerprint("tokB")));
        assert_eq!(load(&p).unwrap(), None, "the targeted token was read");
    }

    /// A fence is per token file: one path's sign-out refuses nothing at
    /// another.
    #[cfg(debug_assertions)]
    #[test]
    fn a_fence_holds_only_for_its_own_token_file() {
        let d = dir("fence-path");
        let (p, q) = (d.join("device-token"), d.join("other-token"));
        store(&p, "tokA").unwrap();
        store(&q, "tokA").unwrap();
        faults::REMOVE.with(|f| f.set(true));
        faults::MARKER.with(|f| f.set(true));
        let _ = end_session(&p, None, None);
        faults::REMOVE.with(|f| f.set(false));
        faults::MARKER.with(|f| f.set(false));
        assert_eq!(load(&p).unwrap(), None);
        assert_eq!(load(&q).unwrap().as_deref(), Some("tokA"));
    }

    /// Marks tokA revoked (its file will not go), as a sign-out does.
    #[cfg(debug_assertions)]
    fn marked_a(p: &Path) {
        store(p, "tokA").unwrap();
        faults::REMOVE.with(|f| f.set(true));
        let ended = end_session(p, Some("tokA"), Some("A"));
        faults::REMOVE.with(|f| f.set(false));
        assert!(matches!(ended, Ended::Marked(_)), "{ended:?}");
    }

    /// What a restart reads: the in-process fence is gone, the disk stays.
    fn after_restart(p: &Path) -> Option<String> {
        clear_fence(p);
        EVERY_FILE.store(false, std::sync::atomic::Ordering::SeqCst);
        load(p).unwrap()
    }

    /// CR 4639801671, case 1: the previous token cannot be read and the
    /// write fails. The old file and its marker stay as they were: after a
    /// restart A is on disk and still refused.
    #[cfg(debug_assertions)]
    #[test]
    fn a_failed_write_leaves_the_old_token_and_its_marker() {
        let d = dir("store-write-fails");
        let p = d.join("device-token");
        marked_a(&p);
        FAIL_PREVIOUS_READ.with(|f| f.set(true));
        FAIL_WRITE.with(|f| f.set(true));
        let stored = store(&p, "tokN");
        FAIL_WRITE.with(|f| f.set(false));
        FAIL_PREVIOUS_READ.with(|f| f.set(false));
        assert!(stored.is_err(), "{stored:?}");
        assert_eq!(
            read_raw(&p).unwrap().map(|s| s.token).as_deref(),
            Some("tokA")
        );
        assert!(
            revoked(&p, &fingerprint("tokA")).is_some(),
            "A's marker went"
        );
        assert_eq!(after_restart(&p), None, "A became readable");
    }

    /// CR 4639801671, case 2: the folder sync fails after the rename, then
    /// the app crashes. The new token was not synced in full, so it is
    /// refused here and by a marker of its own after the restart; A's
    /// marker stays, so A is refused too if the crash undoes the rename.
    #[cfg(debug_assertions)]
    #[test]
    fn a_store_whose_folder_will_not_sync_leaves_no_token_readable() {
        let d = dir("store-sync-fails");
        let p = d.join("device-token");
        marked_a(&p);
        FAIL_DIR_SYNC.with(|f| f.set(true));
        let stored = store(&p, "tokN");
        FAIL_DIR_SYNC.with(|f| f.set(false));
        assert!(stored.is_err(), "{stored:?}");
        assert_eq!(load(&p).unwrap(), None, "the unsynced token was read");
        assert!(
            revoked(&p, &fingerprint("tokA")).is_some(),
            "A's marker went"
        );
        assert_eq!(
            after_restart(&p),
            None,
            "the unsynced token was read after a restart"
        );
    }

    /// CR 4639801671 case 3, CR 4640059241: the marker naming the new
    /// token will not go. The sign-in is refused before anything is
    /// written: A stays on file and refused, and a read after the marker is
    /// repaired by hand (before any retry) still finds A, never the token
    /// that was refused.
    #[cfg(debug_assertions)]
    #[test]
    fn a_store_whose_own_marker_will_not_go_installs_nothing() {
        let d = dir("store-own-marker");
        let p = d.join("device-token");
        marked_a(&p);
        std::fs::create_dir_all(revoked_path(&p, &fingerprint("tokN")).join("held")).unwrap();
        let stored = store(&p, "tokN");
        assert!(stored.is_err(), "{stored:?}");
        let on_file = || read_raw(&p).unwrap().map(|s| s.token);
        assert_eq!(
            on_file().as_deref(),
            Some("tokA"),
            "the refused token was installed"
        );
        assert_eq!(load(&p).unwrap(), None);
        // Repaired by hand, no retry: the old state, still refused.
        std::fs::remove_dir_all(revoked_path(&p, &fingerprint("tokN"))).unwrap();
        assert_eq!(on_file().as_deref(), Some("tokA"));
        assert_eq!(load(&p).unwrap(), None, "a token nobody stored was read");
        assert_eq!(after_restart(&p), None, "A became readable");
        assert!(
            revoked(&p, &fingerprint("tokA")).is_some(),
            "A's marker went"
        );
        // Once its marker can go, a sign-in with it holds.
        store(&p, "tokN").unwrap();
        assert_eq!(load(&p).unwrap().as_deref(), Some("tokN"));
    }

    /// CR 4640059241, case 2: the folder sync fails after the new token is
    /// written and the crash loses the write. The previous token comes back
    /// on disk, and its own marker, which store never touches, still
    /// refuses it after the restart.
    #[cfg(debug_assertions)]
    #[test]
    fn a_write_lost_after_a_failed_sync_leaves_the_revoked_token_refused() {
        let d = dir("store-lost-write");
        let p = d.join("device-token");
        marked_a(&p);
        let before = std::fs::read(&p).unwrap();
        FAIL_DIR_SYNC.with(|f| f.set(true));
        let stored = store(&p, "tokN");
        FAIL_DIR_SYNC.with(|f| f.set(false));
        assert!(stored.is_err(), "{stored:?}");
        // The crash: the unsynced rename never reached the disk.
        std::fs::write(&p, before).unwrap();
        assert_eq!(
            read_raw(&p).unwrap().map(|s| s.token).as_deref(),
            Some("tokA")
        );
        assert_eq!(after_restart(&p), None, "the revoked token came back");
    }

    /// CR 4638660923, CR 4640059241: the removal of a marker that would
    /// refuse the new token is synced before the token is written. A sync
    /// that fails is named and refuses the sign-in: nothing is written, the
    /// old token stays, and a retry once the folder syncs holds.
    #[test]
    fn a_cleared_marker_is_synced_before_the_token_is_written() {
        let d = dir("token-marker-sync");
        let p = d.join("device-token");
        store(&p, "tokA").unwrap();
        write_marker(&p, Some("A"), &fingerprint("tokN"), false).unwrap();
        crate::testlog::start();
        FAIL_MARKER_SYNC.with(|f| f.set(true));
        let stored = store(&p, "tokN");
        FAIL_MARKER_SYNC.with(|f| f.set(false));
        assert!(stored.is_err(), "{stored:?}");
        assert_eq!(load(&p).unwrap().as_deref(), Some("tokA"));
        let lines = crate::testlog::lines();
        assert!(
            lines.iter().any(
                |l| l.contains("token revocation marker verdict=kept reason=folder_not_synced")
            ),
            "{lines:?}"
        );
        store(&p, "tokN").unwrap();
        assert_eq!(load(&p).unwrap().as_deref(), Some("tokN"));
    }

    /// (c) A removal that is not durable is marked too; only when the
    /// marker fails as well is it unsafe.
    // Fault injection exists in debug builds only.
    #[cfg(debug_assertions)]
    #[test]
    fn a_removal_that_is_not_durable_is_marked_too() {
        use std::io::ErrorKind::Other;
        let d = dir("not-durable-marked");
        let p = d.join("device-token");
        store(&p, "tokA").unwrap();
        DIR_SYNC_ERRORS.with(|q| *q.borrow_mut() = vec![Other, Other]);
        let ended = end_session(&p, None, Some("A"));
        DIR_SYNC_ERRORS.with(|q| q.borrow_mut().clear());
        assert!(
            matches!(&ended, Ended::Marked(why) if why.contains("not durable")),
            "{ended:?}"
        );
        assert_eq!(
            revoked(&p, &fingerprint("tokA"))
                .and_then(|r| r.user)
                .as_deref(),
            Some("A")
        );
        store(&p, "tokA").unwrap();
        DIR_SYNC_ERRORS.with(|q| *q.borrow_mut() = vec![Other, Other]);
        faults::MARKER.with(|f| f.set(true));
        let both = end_session(&p, None, Some("A"));
        faults::MARKER.with(|f| f.set(false));
        DIR_SYNC_ERRORS.with(|q| q.borrow_mut().clear());
        assert!(matches!(both, Ended::Unsafe { .. }), "{both:?}");
    }

    /// With neither the removal nor the marker written, the session's end
    /// says a restart is not safe, and the token is still read (nothing on
    /// disk refuses it).
    // Fault injection exists in debug builds only.
    #[cfg(debug_assertions)]
    #[test]
    fn a_token_neither_removed_nor_marked_is_reported_unsafe() {
        let d = dir("unsafe");
        let p = d.join("device-token");
        store(&p, "tokA").unwrap();
        faults::REMOVE.with(|f| f.set(true));
        faults::MARKER.with(|f| f.set(true));
        let ended = end_session(&p, None, Some("A"));
        faults::REMOVE.with(|f| f.set(false));
        faults::MARKER.with(|f| f.set(false));
        assert!(matches!(ended, Ended::Unsafe { .. }), "{ended:?}");
        assert_eq!(ended.label(), "restart_unsafe");
        // Still on file, so a restart may resume it; in this process the
        // fence the operation set refuses it.
        assert_eq!(
            read_raw(&p).unwrap().map(|s| s.token).as_deref(),
            Some("tokA")
        );
        assert_eq!(load(&p).unwrap(), None);
    }

    /// CR 4640611314: a token that is only whitespace is refused before
    /// anything changes: the token on file stays.
    #[test]
    fn an_empty_token_is_refused_and_the_old_one_stays() {
        let d = dir("empty-token");
        let p = d.join("device-token");
        store(&p, "tokA").unwrap();
        for empty in ["", "  ", "\n"] {
            assert!(store(&p, empty).is_err(), "{empty:?} was stored");
            assert_eq!(load(&p).unwrap().as_deref(), Some("tokA"), "{empty:?}");
        }
        store(&p, "  tokB \n").unwrap();
        assert_eq!(load(&p).unwrap().as_deref(), Some("tokB"));
    }

    /// CR 4640624120: a sign-in that yields the same token a marker
    /// revokes is refused before the marker is touched, whatever the
    /// write or the marker's sync would do: the token stays refused after
    /// a restart, and the log says reason=same_token_revoked.
    #[cfg(debug_assertions)]
    #[test]
    fn a_sign_in_with_the_same_revoked_token_stays_refused() {
        type Arm = fn(bool);
        let arms: [(&str, Arm); 3] = [
            ("plain", |_| {}),
            ("failed_write", |on| FAIL_WRITE.with(|f| f.set(on))),
            ("failed_marker_sync", |on| {
                FAIL_MARKER_SYNC.with(|f| f.set(on))
            }),
        ];
        for (name, arm) in arms {
            let d = dir(&format!("same-token-{name}"));
            let p = d.join("device-token");
            marked_a(&p);
            crate::testlog::start();
            arm(true);
            let stored = store(&p, "tokA");
            arm(false);
            let lines = crate::testlog::lines();
            assert!(stored.is_err(), "{name}: {stored:?}");
            assert!(
                revoked(&p, &fingerprint("tokA")).is_some(),
                "{name}: the marker went"
            );
            assert_eq!(load(&p).unwrap(), None, "{name}: read in this process");
            assert_eq!(after_restart(&p), None, "{name}: read after a restart");
            assert!(
                lines
                    .iter()
                    .any(|l| l.contains("reason=same_token_revoked")),
                "{name}: {lines:?}"
            );
            // Another token signs in as before.
            store(&p, "tokB").unwrap();
            assert_eq!(load(&p).unwrap().as_deref(), Some("tokB"), "{name}");
        }
    }

    /// CR 4640611326: a store that installs its token but cannot read it
    /// back is not a success: it says the sign-in is incomplete, and the
    /// fence it could not confirm is left as it was.
    #[test]
    fn a_store_that_cannot_read_back_is_incomplete() {
        let d = dir("read-back");
        let p = d.join("device-token");
        store(&p, "tokA").unwrap();
        add_fence(&p, &Fence::Every, "test");
        FAIL_READ_BACK.with(|f| f.set(true));
        let stored = store(&p, "tokB");
        FAIL_READ_BACK.with(|f| f.set(false));
        let e = stored.expect_err("an unconfirmed store succeeded");
        assert!(is_store_incomplete(&e), "{e:?}");
        assert_eq!(
            fence(&p),
            Fence::Every,
            "the fence followed an unconfirmed store"
        );
        assert!(!is_store_incomplete(&store(&p, "").unwrap_err()));
        clear_fence(&p);
    }

    /// CR 4642574336: a store that cannot read its token back fences that
    /// token: a reader that finds it on file refuses it. A later store
    /// that confirms the same token lifts the fence.
    #[test]
    fn a_store_that_cannot_read_back_fences_its_token_until_confirmed() {
        let d = dir("read-back-fence");
        let p = d.join("device-token");
        store(&p, "tokA").unwrap();
        crate::testlog::start();
        FAIL_READ_BACK.with(|f| f.set(true));
        let stored = store(&p, "tokB");
        FAIL_READ_BACK.with(|f| f.set(false));
        assert!(is_store_incomplete(&stored.unwrap_err()));
        assert!(
            fence(&p).refuses("tokB"),
            "the unconfirmed token was not fenced"
        );
        assert_eq!(load(&p).unwrap(), None, "the unconfirmed token was read");
        assert!(crate::testlog::lines()
            .iter()
            .any(|l| l.contains("reason=read_back_failed fenced=new_token")));
        store(&p, "tokB").unwrap();
        assert_eq!(fence(&p), Fence::Open, "a confirmed store left the fence");
        assert_eq!(load(&p).unwrap().as_deref(), Some("tokB"));
    }

    /// CR 4640611307: a store over an unnamed fence that cannot read the
    /// token it replaces allows the new token and nothing else: an older
    /// link's token stays refused.
    #[cfg(debug_assertions)]
    #[test]
    fn a_store_over_an_unnamed_fence_keeps_older_tokens_refused() {
        let d = dir("fence-all-but");
        let p = d.join("device-token");
        store(&p, "tokA").unwrap();
        add_fence(&p, &Fence::Every, "test");
        FAIL_PREVIOUS_READ.with(|f| f.set(true));
        store(&p, "tokB").unwrap();
        FAIL_PREVIOUS_READ.with(|f| f.set(false));
        let f = fence(&p);
        assert!(
            f.refuses("tokA"),
            "an older link's token was allowed: {f:?}"
        );
        assert!(!f.refuses("tokB"));
        assert_eq!(load(&p).unwrap().as_deref(), Some("tokB"));
        clear_fence(&p);
    }

    /// CR 4641125283, 4641128622: a store whose folder will not sync adds
    /// its token to the fence and narrows nothing. An older link's token,
    /// refused by an unnamed fence or by name, stays refused beside the
    /// new one.
    #[test]
    fn a_store_whose_folder_will_not_sync_keeps_the_fence_it_found() {
        for (case, held) in [
            ("every", Fence::Every),
            ("token", Fence::token(&fingerprint("tokA"))),
        ] {
            let d = dir(&format!("fence-sync-{case}"));
            let p = d.join("device-token");
            store(&p, "tokA").unwrap();
            add_fence(&p, &held, "test");
            FAIL_DIR_SYNC.with(|f| f.set(true));
            let stored = store(&p, "tokB");
            FAIL_DIR_SYNC.with(|f| f.set(false));
            assert!(stored.is_err(), "{case}: {stored:?}");
            let f = fence(&p);
            assert!(f.refuses("tokA"), "{case}: A's refusal was lost: {f:?}");
            assert!(
                f.refuses("tokB"),
                "{case}: the unsynced B is allowed: {f:?}"
            );
            clear_fence(&p);
        }
    }

    /// CR 4640611335: an unsafe sign-out of B after an unsafe sign-out of A
    /// keeps A refused; a later sign-in still reads its own token.
    #[cfg(debug_assertions)]
    #[test]
    fn a_second_unsafe_sign_out_keeps_the_first_refusal() {
        let d = dir("fence-union");
        let p = d.join("device-token");
        store(&p, "tokA").unwrap();
        faults::REMOVE.with(|f| f.set(true));
        faults::MARKER.with(|f| f.set(true));
        let _ = end_session(&p, None, None);
        faults::REMOVE.with(|f| f.set(false));
        faults::MARKER.with(|f| f.set(false));
        store(&p, "tokB").unwrap();
        assert!(fence(&p).refuses("tokA"));
        faults::REMOVE.with(|f| f.set(true));
        faults::MARKER.with(|f| f.set(true));
        let _ = end_session(&p, None, None);
        faults::REMOVE.with(|f| f.set(false));
        faults::MARKER.with(|f| f.set(false));
        let f = fence(&p);
        assert!(f.refuses("tokA"), "A's refusal was lost: {f:?}");
        assert!(f.refuses("tokB"), "{f:?}");
        store(&p, "tokC").unwrap();
        assert_eq!(load(&p).unwrap().as_deref(), Some("tokC"));
        clear_fence(&p);
    }

    /// Named fences join into one set: each name stays refused, and a
    /// store lifts only its own.
    #[test]
    fn named_fences_join_into_one_set() {
        let (a, b) = (fingerprint("tokA"), fingerprint("tokB"));
        let both = Fence::token(&a).union(&Fence::token(&b));
        assert!(both.refuses("tokA") && both.refuses("tokB"), "{both:?}");
        assert!(!both.refuses("tokC"), "{both:?}");
        assert_eq!(both.after_store(&a, None), Fence::token(&b));
        assert_eq!(Fence::token(&a).after_store(&a, None), Fence::Open);
        assert_eq!(Fence::Every.union(&both), Fence::Every);
        assert_eq!(
            both.union(&Fence::AllBut(a.clone())),
            Fence::Every,
            "the allowed token is named"
        );
        assert_eq!(
            Fence::token(&b).union(&Fence::AllBut(a.clone())),
            Fence::AllBut(a)
        );
    }

    /// CR 4640624099: the token file holds the token alone, as the base
    /// build reads it whole, also once a user is paired; the pairing file
    /// holds the user and the token's fingerprint.
    #[test]
    fn a_paired_token_file_holds_the_token_alone() {
        let d = dir("pair-format");
        let p = d.join("device-token");
        store(&p, "tokA").unwrap();
        store_user(&p, "A").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap().trim(), "tokA");
        assert_eq!(
            std::fs::read_to_string(user_path(&p)).unwrap(),
            format!("A\n{}\n", fingerprint("tokA"))
        );
        assert_eq!(read(&p).unwrap().unwrap().user.as_deref(), Some("A"));
    }

    /// CR 4640624099: a token file in the earlier two-line layout is split
    /// on read: the user moves to the pairing file, the token file keeps
    /// the token alone, and the pair reads as before.
    #[test]
    fn a_two_line_token_file_is_split_on_read() {
        let d = dir("pair-migrate");
        let p = d.join("device-token");
        std::fs::write(&p, "tokA\nA\n").unwrap();
        assert_eq!(read(&p).unwrap().unwrap().user.as_deref(), Some("A"));
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "tokA\n");
        assert_eq!(read(&p).unwrap().unwrap().user.as_deref(), Some("A"));
    }

    /// CR 4640624099: a pairing file naming another token's fingerprint
    /// pairs nothing: the token is unpaired (fail closed).
    #[test]
    fn a_pairing_for_another_token_leaves_the_token_unpaired() {
        let d = dir("pair-mismatch");
        let p = d.join("device-token");
        store(&p, "tokA").unwrap();
        store_user(&p, "A").unwrap();
        store(&p, "tokB").unwrap();
        assert_eq!(read(&p).unwrap().unwrap().user, None);
        std::fs::write(user_path(&p), format!("B\n{}\n", fingerprint("tokC"))).unwrap();
        assert_eq!(read(&p).unwrap().unwrap().user, None);
    }
}
