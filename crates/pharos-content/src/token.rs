//! The device token file, owned here whole (ADR 0145): the token on the
//! first line and, optionally, the id of the user it was issued to on the
//! second. The car's sign-in and the phone app both go through this module,
//! and every write of the file takes one process-wide lock, so a sign-in can
//! never land between another writer's read and its write (a user id would
//! otherwise sit beside a token it does not belong to, or a new token be
//! replaced by the old one).
//!
//! The file is in a directory the caller names (the app's private files
//! directory), replaced whole by a temp file of its own, fsynced and renamed
//! (mode 0600), with the directory synced after, so a crash leaves the old
//! file or the new one, never a mixture. A token with no user id (the older
//! layout kept the id in a sidecar file, which is never trusted) is a token
//! whose user is unknown. Never logs a value.

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
/// nothing in this process resumes from it. It names that token by
/// fingerprint; with no token to name it refuses every token until a store
/// installs one. Kept per path, so one file's sign-out refuses nothing at
/// another. Set and replaced only under the file's lock, by the operation
/// that decided it ([`end_session`], [`store`]), and honored by [`read`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Fence {
    /// No sign-out left a token behind.
    #[default]
    Open,
    /// The token with this fingerprint is refused.
    Token(String),
    /// The token left behind could not be named: every token is refused.
    Every,
}

impl Fence {
    /// The fence for a session end that left the token fingerprinted
    /// `target` on file (`None`: it could not be named).
    pub fn for_target(target: Option<&str>) -> Self {
        target.map_or(Self::Every, |fp| Self::Token(fp.to_owned()))
    }

    /// The fence for a sign-out that left `held` on file.
    pub fn for_token(held: Option<&str>) -> Self {
        held.map_or(Self::Every, |t| Self::Token(fingerprint(t)))
    }

    /// Whether `token` is refused.
    pub fn refuses(&self, token: &str) -> bool {
        match self {
            Self::Open => false,
            Self::Token(fp) => fingerprint(token) == *fp,
            Self::Every => true,
        }
    }

    /// The verdict word for logs.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Token(_) => "token",
            Self::Every => "every",
        }
    }

    /// The fence once a store installed the token fingerprinted `stored`
    /// over the one fingerprinted `replaced` (when that could be read).
    /// A fence naming another token stays: it refuses only that token. One
    /// naming the stored token, or none, opens. An unnamed one names the
    /// token the store overwrote, if it could be read and differs, and
    /// opens otherwise: the token it could not name is no longer on file.
    pub fn after_store(&self, stored: &str, replaced: Option<&str>) -> Self {
        match self {
            Self::Open => Self::Open,
            Self::Token(fp) if fp != stored => self.clone(),
            Self::Token(_) => Self::Open,
            Self::Every => match replaced {
                Some(old) if old != stored => Self::Token(old.to_owned()),
                _ => Self::Open,
            },
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

/// Replaces the fence of `path`; the caller holds the file's lock.
fn set_fence_held(path: &Path, next: Fence, reason: &str) {
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
    }
}

/// The fence of the token file at `path`, read under the file's lock.
pub fn fence(path: &Path) -> Fence {
    let _held = hold();
    fence_held(path)
}

/// Sets the fence of the token file at `path`, under the file's lock: for a
/// caller that decided it from a read of its own (a test, or a host whose
/// sign-out was refused before it reached the file).
pub fn set_fence(path: &Path, next: Fence, reason: &str) {
    let _held = hold();
    set_fence_held(path, next, reason);
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

/// `None` for no token. A user id is the second line, when there is one.
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

fn encode(token: &str, user: Option<&str>) -> String {
    match user {
        Some(u) => format!("{token}\n{u}\n"),
        None => format!("{token}\n"),
    }
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
    match marker(path) {
        Marker::Present(r) if r.token == fingerprint(&stored.token) => {
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
        Marker::Present(_) | Marker::Absent => {
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

/// The file as it is, marker or not. The writers compare against this.
fn read_raw(path: &Path) -> Result<Option<Stored>, ContentError> {
    match std::fs::read_to_string(path) {
        Ok(raw) => Ok(decode(&raw)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(err(path, e)),
    }
}

/// The revocation marker beside the token file: written when a session
/// ended and its token could not be removed. It names the token's user and
/// a fingerprint of the token, never the token.
pub fn revoked_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".revoked");
    PathBuf::from(name)
}

/// What a revocation marker holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Revoked {
    /// The user the revoked token was paired with, when it was.
    pub user: Option<String>,
    token: String,
}

/// The revocation marker beside a token file, as a read finds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Marker {
    /// No marker: nothing is revoked.
    Absent,
    /// A marker naming a token.
    Present(Revoked),
    /// A marker that is there but could not be read or parsed (why): it
    /// may name any token, so every read refuses (fail closed).
    Unreadable(String),
}

/// The marker beside `path`, when there is a readable one.
pub fn revoked(path: &Path) -> Option<Revoked> {
    match marker(path) {
        Marker::Present(r) => Some(r),
        Marker::Absent | Marker::Unreadable(_) => None,
    }
}

/// The marker beside `path`: absent, present, or there and unreadable.
pub fn marker(path: &Path) -> Marker {
    #[cfg(debug_assertions)]
    if faults::MARKER_READ.with(std::cell::Cell::get) {
        return Marker::Unreadable("marker read failed".to_owned());
    }
    let raw = match std::fs::read_to_string(revoked_path(path)) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Marker::Absent,
        Err(e) => return Marker::Unreadable(e.to_string()),
    };
    let mut lines = raw.lines();
    let user = lines.next().map(str::trim).filter(|u| !u.is_empty());
    let token = lines.next().map(str::trim).unwrap_or_default();
    if token.len() != 16 || !token.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Marker::Unreadable("no token fingerprint in the marker".to_owned());
    }
    Marker::Present(Revoked {
        user: user.map(str::to_owned),
        token: token.to_owned(),
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

pub fn store(path: &Path, token: &str) -> Result<(), ContentError> {
    let _held = hold();
    let token = token.trim();
    let previous = read_raw(path).ok().flatten();
    let replaced = previous.as_ref().map(|s| fingerprint(&s.token));
    // A new sign-in: a marker of an older session no longer has anything to
    // refuse (it names that token only), so it goes. Its removal comes
    // AFTER the new token is on disk whenever the marker may refuse the
    // token on file: a store that fails, or a crash, before the install
    // then leaves the marker refusing the old token, here and after a
    // restart. Only a readable marker that names the new token and not the
    // old one goes first: it refuses nothing on disk, so a failure after
    // its removal changes nothing, and one that will not go fails the store
    // with nothing changed.
    let new = fingerprint(token);
    match marker(path) {
        Marker::Present(r) if r.token == new && replaced.as_deref() != Some(new.as_str()) => {
            if let Err(e) = remove_marker(path) {
                log::warn!(
                    "token store verdict=refused reason=revocation_marker_blocks_token error={e}"
                );
                return Err(err(
                    path,
                    format!(
                        "the revocation marker would refuse the new token and cannot be removed: {e}"
                    ),
                ));
            }
            write(path, token, None).map_err(|e| err(path, e))?;
        }
        Marker::Present(r) if r.token != new => {
            // It cannot refuse the new token: it may stay.
            write(path, token, None).map_err(|e| err(path, e))?;
            if let Err(e) = remove_marker(path) {
                log::warn!(
                    "token revocation marker verdict=kept reason=marks_another_token error={e}"
                );
            }
        }
        Marker::Absent => {
            write(path, token, None).map_err(|e| err(path, e))?;
            if let Err(e) = remove_marker(path) {
                log::warn!("token revocation marker verdict=kept reason=absent_on_read error={e}");
            }
        }
        // Unreadable (it refuses every token, the old one too), or naming
        // the old token that the new one equals: the new token goes on disk
        // durably first, then the marker. A marker that will not go, or a
        // folder that will not sync, rolls the install back: no token is
        // left that a repair of the marker would make readable.
        Marker::Present(_) | Marker::Unreadable(_) => {
            write(path, token, None).map_err(|e| err(path, e))?;
            let dir = path.parent().ok_or_else(|| err(path, "no directory"))?;
            let installed = durable_dir(dir)
                .and_then(|()| remove_marker(path).map_err(|e| format!("marker not removed: {e}")));
            if let Err(why) = installed {
                let rolled = std::fs::remove_file(path)
                    .map_err(|e| e.to_string())
                    .and_then(|()| durable_dir(dir));
                match &rolled {
                    Ok(()) => log::warn!(
                        "token store verdict=refused reason=revocation_marker_blocks_token rollback=removed error={why}"
                    ),
                    Err(e) => {
                        // Still on file behind the marker: refused here too,
                        // whatever becomes of the marker in this process.
                        set_fence_held(path, Fence::Token(new.clone()), "store_rollback_failed");
                        log::warn!(
                            "token store verdict=refused reason=revocation_marker_blocks_token rollback=failed rollback_error={e} error={why}"
                        );
                    }
                }
                return Err(err(
                    path,
                    format!("the revocation marker would refuse the new token and cannot be removed: {why}"),
                ));
            }
        }
    }
    // The hook hears of the store only once the file is read back holding
    // the new token: a fence it lifts is for a token really gone.
    match read_raw(path) {
        Ok(Some(s)) if s.token == token => {
            let stored = fingerprint(token);
            let next = fence_held(path).after_store(&stored, replaced.as_deref());
            set_fence_held(path, next, "token_stored");
            if EVERY_FILE.swap(false, std::sync::atomic::Ordering::SeqCst) {
                log::info!("token fence verdict=open scope=process reason=token_stored");
            }
            if let Some(hook) = ON_STORED.get() {
                hook(&fingerprint(token), replaced.as_deref());
            }
        }
        Ok(other) => log::warn!(
            "token store verdict=not_replaced reason=read_back_differs found={}",
            if other.is_some() {
                "another_token"
            } else {
                "none"
            }
        ),
        Err(e) => log::warn!(
            "token store verdict=not_replaced reason=read_back_failed error={}",
            e.redacted()
        ),
    }
    Ok(())
}

/// Pairs `user` with the token on file, read and written under the lock.
/// Refused when there is none: an id with no token to belong to is nobody's.
pub fn store_user(path: &Path, user: &str) -> Result<(), ContentError> {
    let _held = hold();
    let stored = read_raw(path)?;
    #[cfg(test)]
    tests::between_read_and_write();
    match stored {
        Some(s) => write(path, &s.token, Some(user)).map_err(|e| err(path, e)),
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
        Some(s) if s.token == seen.trim() => write(path, &s.token, Some(user))
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
        set_fence_held(
            path,
            Fence::for_target(target.as_deref()),
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
            // A marker for another token (or one that cannot be read, which
            // refuses every token) is what keeps that token from resuming:
            // it stays. The ended token's revocation is then unconfirmed.
            let target = fingerprint(seen);
            match marker(path) {
                Marker::Present(r) if r.token != target => {
                    log::warn!(
                        "token revocation marker verdict=kept reason=marks_another_token error={e}"
                    );
                    return Ended::Unsafe {
                        removal,
                        marker: "a marker for another token is in place".to_owned(),
                        target: Some(target),
                    };
                }
                Marker::Unreadable(why) => {
                    log::warn!("token revocation marker verdict=kept reason=marker_unreadable marker_error={why} error={e}");
                    return Ended::Unsafe {
                        removal,
                        marker: format!("an unreadable marker is in place ({why})"),
                        target: Some(target),
                    };
                }
                Marker::Present(_) | Marker::Absent => {}
            }
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
    match write_marker(path, named, token_fingerprint) {
        Ok(()) => {
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

/// Removes the revocation marker beside `path` (absent is fine), and
/// syncs the folder once it went, so the removal is on disk before the
/// store reports success. A sync that fails is logged apart from a removal
/// that failed: the marker is gone for every reader, and a crash that brings
/// it back only refuses again (fail closed), so the store still holds.
fn remove_marker(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(revoked_path(path)) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    }
    match path.parent().map(durable_dir) {
        Some(Ok(())) => {
            log::info!("token revocation marker verdict=cleared reason=signed_in");
        }
        Some(Err(why)) => log::warn!(
            "token revocation marker verdict=cleared_not_durable reason=signed_in error={why}"
        ),
        None => log::warn!(
            "token revocation marker verdict=cleared_not_durable reason=signed_in error=no_directory"
        ),
    }
    Ok(())
}

/// Writes the marker whole (temp file, fsync, rename, folder sync). The
/// folder sync must succeed too: a marker a crash can lose is no marker.
fn write_marker(path: &Path, user: Option<&str>, token: &str) -> std::io::Result<()> {
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
    let target = revoked_path(path);
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
        f.write_all(format!("{user}\n{token}\n").as_bytes())?;
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
        /// The marker cannot be read (it is there).
        pub static MARKER_READ: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
        /// The token file cannot be read in `end_session`.
        pub static TOKEN_READ: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
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
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(absent(path)),
        Err(e) => return Err(err(path, e)),
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
fn durable_dir(dir: &Path) -> Result<(), String> {
    let Err(first) = sync_dir(dir) else {
        return Ok(());
    };
    match sync_dir(dir) {
        Ok(()) => {
            log::info!("token file dir sync verdict=retried first_error={first}");
            Ok(())
        }
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::InvalidInput | std::io::ErrorKind::Unsupported
            ) =>
        {
            Err(format!("directory sync unsupported: {e}"))
        }
        Err(e) => Err(e.to_string()),
    }
}

/// The file the older layout kept the user id in. Never read; removed when
/// found, because the id it holds is not known to belong to the token.
pub fn legacy_user_path(path: &Path) -> PathBuf {
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
fn write(path: &Path, token: &str, user: Option<&str>) -> std::io::Result<()> {
    #[cfg(test)]
    if tests::FAIL_WRITE.with(std::cell::Cell::get) {
        return Err(std::io::Error::other("token write failed"));
    }
    if token.contains('\n') || user.is_some_and(|u| u.contains('\n')) {
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
        f.write_all(encode(token, user).as_bytes())?;
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
        /// While set, writing the token file fails before anything changes.
        pub(super) static FAIL_WRITE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
        /// While set, syncing the directory after a rename fails.
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

    /// A sign-in that lands while a user id is being paired with the old
    /// token waits for the pairing: the new token survives, with no user
    /// (its own is not known yet), never the old token or the old user.
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
                    let t = std::thread::spawn(move || store(&p, "tokB").unwrap());
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
                user: None
            }),
            "the sign-in was overwritten by the pairing"
        );
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

    /// A rename whose directory entry cannot be flushed has still installed
    /// the new file: the write succeeds (the caller's memory and hooks then
    /// follow the file) and the lost durability is logged with its reason.
    #[test]
    fn a_failed_directory_sync_after_the_install_keeps_the_new_token() {
        let d = dir("dirsync");
        let path = d.join("device-token");
        store(&path, "old").unwrap();
        crate::testlog::start();
        FAIL_DIR_SYNC.with(|f| f.set(true));
        let stored = store(&path, "new");
        FAIL_DIR_SYNC.with(|f| f.set(false));
        assert!(stored.is_ok(), "{stored:?}");
        assert_eq!(load(&path).unwrap().as_deref(), Some("new"));
        let lines = crate::testlog::lines();
        assert!(
            lines.iter().any(|l| l.contains("installed but not synced")
                && l.contains("reason=directory sync failed")),
            "{lines:?}"
        );
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
            (vec![Other, Unsupported], Some("directory sync unsupported")),
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
        std::fs::write(legacy_user_path(&p), "A").unwrap();
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
        assert_eq!(revoked(&p).unwrap().user.as_deref(), Some("A"));
        store(&p, "tokC").unwrap();
        assert_eq!(load(&p).unwrap().as_deref(), Some("tokC"));
        assert!(revoked(&p).is_none(), "the sign-in left the marker");
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
        store(&p, "tokB").unwrap();
        assert_eq!(reader.join().unwrap(), None, "the revoked token was read");
        assert_eq!(load(&p).unwrap().as_deref(), Some("tokB"));
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

    /// (a) A marker that is there but cannot be read, or holds no
    /// fingerprint, refuses every token (fail closed); a missing one
    /// refuses nothing.
    // Fault injection exists in debug builds only.
    #[cfg(debug_assertions)]
    #[test]
    fn an_unreadable_marker_fails_closed_and_a_missing_one_is_absent() {
        let d = dir("marker-unreadable");
        let p = d.join("device-token");
        store(&p, "tokA").unwrap();
        assert_eq!(marker(&p), Marker::Absent);
        assert_eq!(load(&p).unwrap().as_deref(), Some("tokA"));
        faults::MARKER_READ.with(|f| f.set(true));
        let read_failed = load(&p).unwrap();
        faults::MARKER_READ.with(|f| f.set(false));
        assert_eq!(
            read_failed, None,
            "an unreadable marker let the token through"
        );
        std::fs::write(revoked_path(&p), "A\n").unwrap();
        assert!(
            matches!(marker(&p), Marker::Unreadable(_)),
            "{:?}",
            marker(&p)
        );
        assert_eq!(
            load(&p).unwrap(),
            None,
            "a marker with no fingerprint let it through"
        );
        // A new sign-in replaces both.
        store(&p, "tokB").unwrap();
        assert_eq!(marker(&p), Marker::Absent);
        assert_eq!(load(&p).unwrap().as_deref(), Some("tokB"));
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
        let r = revoked(&p).expect("no marker");
        assert_eq!(r.user.as_deref(), Some("A"));
        assert_eq!(load(&p).unwrap(), None, "the ended token was read");
        assert!(matches!(unnamed, Ended::Unsafe { .. }), "{unnamed:?}");
    }

    /// CR 4637783569: a refusal for A that arrives while the file cannot
    /// be read does not replace B's marker. B stays refused, and the end
    /// reports A's revocation unconfirmed.
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
        assert!(
            matches!(&a, Ended::Unsafe { target: Some(t), .. } if *t == fingerprint("tokA")),
            "{a:?}"
        );
        assert_eq!(revoked(&p).map(|r| r.user), Some(Some("B".to_owned())));
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
        assert_eq!(fence(&p), Fence::Token(fingerprint("tokB")));
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

    /// CR 4637783563: a sign-in whose old marker cannot be removed and
    /// would refuse the new token (here it cannot be read: a folder in
    /// its place) fails, rather than report a token no read returns.
    #[test]
    fn a_store_fails_while_an_unremovable_marker_blocks_the_token() {
        let d = dir("token-store-blocked");
        let p = d.join("device-token");
        let marker = revoked_path(&p);
        std::fs::create_dir_all(marker.join("held")).unwrap();
        let stored = store(&p, "tokN");
        assert!(stored.is_err(), "{stored:?}");
        assert_eq!(load(&p).unwrap(), None);
        std::fs::remove_dir_all(&marker).unwrap();
        // CR 4638660913: the failed store left no token that the repair
        // made readable.
        assert_eq!(
            load(&p).unwrap(),
            None,
            "a failed store's token became readable"
        );
        store(&p, "tokN").unwrap();
        assert_eq!(load(&p).unwrap().as_deref(), Some("tokN"));
    }

    /// CR 4638660913: a store refused by a marker that will not go leaves
    /// no token behind it (the install is rolled back) and the marker in
    /// place.
    #[test]
    fn a_blocked_store_leaves_no_token_and_keeps_its_marker() {
        let d = dir("token-store-blocked-prev");
        let p = d.join("device-token");
        store(&p, "tokA").unwrap();
        let marker = revoked_path(&p);
        std::fs::create_dir_all(marker.join("held")).unwrap();
        assert!(store(&p, "tokN").is_err());
        assert_eq!(
            read_raw(&p).unwrap(),
            None,
            "the install was not rolled back"
        );
        assert!(marker.is_dir(), "the marker was changed");
        assert_eq!(load(&p).unwrap(), None);
    }

    /// CR 4639610735: a marker that refuses the old token goes only after
    /// the new token is on disk. A write that fails leaves the marker, so
    /// after a restart (the in-process fence gone) the old token is still
    /// refused.
    #[test]
    fn a_failed_write_keeps_the_marker_refusing_the_old_token() {
        let d = dir("token-store-write-fails");
        let p = d.join("device-token");
        store(&p, "tokA").unwrap();
        // Unreadable: it refuses every token, tokA included.
        std::fs::write(revoked_path(&p), "not a marker").unwrap();
        assert_eq!(load(&p).unwrap(), None);
        FAIL_WRITE.with(|f| f.set(true));
        let stored = store(&p, "tokN");
        FAIL_WRITE.with(|f| f.set(false));
        assert!(stored.is_err(), "{stored:?}");
        // A restart: nothing in-process survives.
        set_fence(&p, Fence::Open, "test_restart");
        assert_eq!(
            read_raw(&p).unwrap().map(|s| s.token).as_deref(),
            Some("tokA")
        );
        assert_eq!(load(&p).unwrap(), None, "the old token became readable");
    }

    /// CR 4638660923: the removal of a marker that would refuse the new
    /// token is synced before the store reports success; a sync that fails
    /// is logged apart, as not durable, and the store still holds.
    #[test]
    fn a_cleared_marker_is_synced_and_a_failed_sync_is_named() {
        let d = dir("token-marker-sync");
        let p = d.join("device-token");
        store(&p, "tokA").unwrap();
        write_marker(&p, Some("A"), &fingerprint("tokN")).unwrap();
        crate::testlog::start();
        DIR_SYNC_ERRORS.with(|q| {
            q.borrow_mut()
                .extend([std::io::ErrorKind::Other, std::io::ErrorKind::Other]);
        });
        store(&p, "tokN").unwrap();
        DIR_SYNC_ERRORS.with(|q| q.borrow_mut().clear());
        assert_eq!(load(&p).unwrap().as_deref(), Some("tokN"));
        let lines = crate::testlog::lines();
        assert!(
            lines
                .iter()
                .any(|l| l.contains("token revocation marker verdict=cleared_not_durable")),
            "{lines:?}"
        );
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
        assert_eq!(revoked(&p).and_then(|r| r.user).as_deref(), Some("A"));
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
}
