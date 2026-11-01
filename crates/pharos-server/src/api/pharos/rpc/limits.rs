//! Connection caps for `/pharos/rpc` (017 FR-010).
//!
//! Two bounds, shared by every actix worker:
//! - connections not yet signed in, server-wide ([`MAX_UNAUTHENTICATED`]),
//!   each given [`AUTH_DEADLINE`] to `resume` before it is closed. This bounds
//!   what an anonymous client can hold open;
//! - connections per signed-in user (`[server.native_api]
//!   .max_connections_per_user`), checked when `resume` succeeds.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use pharos_core::UserId;

/// Open connections that have not yet resumed a session, server-wide.
pub const MAX_UNAUTHENTICATED: u32 = 64;
/// How long a connection may stay open without a session.
pub const AUTH_DEADLINE: Duration = Duration::from_secs(30);

#[derive(Default)]
struct Counts {
    unauthenticated: u32,
    per_user: HashMap<UserId, u32>,
}

/// The live counts. Cheap to clone; one per server.
#[derive(Clone, Default)]
pub struct ConnectionLimits {
    counts: Arc<Mutex<Counts>>,
}

impl ConnectionLimits {
    fn lock(&self) -> std::sync::MutexGuard<'_, Counts> {
        self.counts.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A slot for a new, not yet signed-in connection, or `None` when
    /// [`MAX_UNAUTHENTICATED`] are already open.
    pub fn admit(&self) -> Option<Slot> {
        let mut counts = self.lock();
        if counts.unauthenticated >= MAX_UNAUTHENTICATED {
            return None;
        }
        counts.unauthenticated += 1;
        Some(Slot {
            limits: self.clone(),
            held: Held::Unauthenticated,
        })
    }

    /// Moves `slot` to `user`, or answers how many that user already holds
    /// when it would exceed `max`. A slot already held by `user` stays.
    pub fn sign_in(&self, slot: &mut Slot, user: UserId, max: u32) -> Result<(), u32> {
        if slot.held == Held::User(user) {
            return Ok(());
        }
        let mut counts = self.lock();
        let held = counts.per_user.get(&user).copied().unwrap_or(0);
        if held >= max {
            return Err(held);
        }
        *counts.per_user.entry(user).or_insert(0) += 1;
        release(&mut counts, slot.held);
        slot.held = Held::User(user);
        Ok(())
    }

    /// Moves a signed-in `slot` back to not signed in, after its last
    /// session logged out, so the user's count drops. Counted as
    /// unauthenticated even past [`MAX_UNAUTHENTICATED`]: the connection is
    /// already open, and the auth deadline closes it if nobody signs in.
    pub fn sign_out(&self, slot: &mut Slot) {
        if slot.held == Held::Unauthenticated {
            return;
        }
        let mut counts = self.lock();
        release(&mut counts, slot.held);
        counts.unauthenticated += 1;
        slot.held = Held::Unauthenticated;
    }

    /// Connections `user` holds now.
    pub fn held_by(&self, user: UserId) -> u32 {
        self.lock().per_user.get(&user).copied().unwrap_or(0)
    }

    /// Connections not yet signed in, now.
    pub fn unauthenticated(&self) -> u32 {
        self.lock().unauthenticated
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Held {
    Unauthenticated,
    User(UserId),
}

fn release(counts: &mut Counts, held: Held) {
    match held {
        Held::Unauthenticated => counts.unauthenticated = counts.unauthenticated.saturating_sub(1),
        Held::User(user) => {
            if let Some(n) = counts.per_user.get_mut(&user) {
                *n = n.saturating_sub(1);
                if *n == 0 {
                    counts.per_user.remove(&user);
                }
            }
        }
    }
}

/// One connection's place in the counts; released when dropped.
pub struct Slot {
    limits: ConnectionLimits,
    held: Held,
}

impl Slot {
    pub fn is_signed_in(&self) -> bool {
        matches!(self.held, Held::User(_))
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        release(&mut self.limits.lock(), self.held);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn unauthenticated_connections_are_capped_and_released_on_drop() {
        let limits = ConnectionLimits::default();
        let slots: Vec<_> = (0..MAX_UNAUTHENTICATED)
            .map(|_| limits.admit().unwrap())
            .collect();
        assert!(limits.admit().is_none());
        drop(slots);
        assert_eq!(limits.unauthenticated(), 0);
        assert!(limits.admit().is_some());
    }

    #[test]
    fn signing_in_moves_the_slot_and_caps_per_user() {
        let limits = ConnectionLimits::default();
        let user = UserId::new();
        let mut a = limits.admit().unwrap();
        let mut b = limits.admit().unwrap();
        limits.sign_in(&mut a, user, 1).unwrap();
        assert_eq!(limits.unauthenticated(), 1);
        assert_eq!(limits.sign_in(&mut b, user, 1), Err(1));
        assert!(!b.is_signed_in());
        // Resuming again as the same user on the same connection is free.
        limits.sign_in(&mut a, user, 1).unwrap();
        drop(a);
        assert_eq!(limits.held_by(user), 0);
        limits.sign_in(&mut b, user, 1).unwrap();
        assert_eq!(limits.unauthenticated(), 0);
    }

    /// A logout must not keep the user's place: otherwise N logouts on
    /// connections left open lock the user out.
    #[test]
    fn signing_out_releases_the_user_and_counts_unauthenticated() {
        let limits = ConnectionLimits::default();
        let user = UserId::new();
        let mut slot = limits.admit().unwrap();
        limits.sign_in(&mut slot, user, 1).unwrap();
        limits.sign_out(&mut slot);
        assert_eq!(limits.held_by(user), 0);
        assert_eq!(limits.unauthenticated(), 1);
        assert!(!slot.is_signed_in());
        drop(slot);
        assert_eq!(limits.unauthenticated(), 0);
    }

    #[test]
    fn switching_user_releases_the_first() {
        let limits = ConnectionLimits::default();
        let (u1, u2) = (UserId::new(), UserId::new());
        let mut slot = limits.admit().unwrap();
        limits.sign_in(&mut slot, u1, 4).unwrap();
        limits.sign_in(&mut slot, u2, 4).unwrap();
        assert_eq!((limits.held_by(u1), limits.held_by(u2)), (0, 1));
    }
}
