//! Connection caps for `/pharos/rpc` (017 FR-010).
//!
//! Three bounds, shared by every actix worker:
//! - connections not yet signed in, server-wide ([`MAX_UNAUTHENTICATED`]),
//!   each given [`AUTH_DEADLINE`] to `resume` before it is closed. This bounds
//!   what an anonymous client can hold open;
//! - the same connections per client address ([`MAX_UNAUTHENTICATED_PER_IP`]),
//!   so one host cannot take the server-wide bound alone. Enforced only when
//!   the server knows its proxies ([`ConnectionLimits::with_per_ip`]);
//! - connections per signed-in user (`[server.native_api]
//!   .max_connections_per_user`), checked when `resume` succeeds.

use std::collections::HashMap;
use std::hash::Hash;
use std::net::IpAddr;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use pharos_core::UserId;

use crate::auth::ip_key;

/// Open connections that have not yet resumed a session, server-wide.
pub const MAX_UNAUTHENTICATED: u32 = 64;
/// Open connections that have not yet resumed a session, per client address.
/// A household's clients share one address, so this stays well above one
/// household's need while leaving most of [`MAX_UNAUTHENTICATED`] to others.
pub const MAX_UNAUTHENTICATED_PER_IP: u32 = 8;
/// How long a connection may stay open without a session.
pub const AUTH_DEADLINE: Duration = Duration::from_secs(30);

#[derive(Default)]
struct Counts {
    unauthenticated: u32,
    per_address: HashMap<IpAddr, u32>,
    per_user: HashMap<UserId, u32>,
}

/// The live counts. Cheap to clone; one per server.
#[derive(Clone, Default)]
pub struct ConnectionLimits {
    counts: Arc<Mutex<Counts>>,
    per_ip: bool,
}

/// Why [`ConnectionLimits::admit`] refused a connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// [`MAX_UNAUTHENTICATED`] connections are already open.
    Global,
    /// [`MAX_UNAUTHENTICATED_PER_IP`] connections from this address are open.
    PerAddress,
}

impl ConnectionLimits {
    fn lock(&self) -> std::sync::MutexGuard<'_, Counts> {
        self.counts.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Enforces [`MAX_UNAUTHENTICATED_PER_IP`]. Off by default: behind an
    /// ingress that is not in `trusted_proxies`, every client shares the
    /// ingress's address, and one host's idle sockets would refuse the rest.
    #[must_use]
    pub fn with_per_ip(mut self, per_ip: bool) -> Self {
        self.per_ip = per_ip;
        self
    }

    /// A slot for a new, not yet signed-in connection from `client_ip`, or
    /// why it is refused. A refusal counts nothing.
    pub fn admit(&self, client_ip: IpAddr) -> Result<Slot, Refusal> {
        let ip = ip_key(client_ip);
        let mut counts = self.lock();
        if counts.unauthenticated >= MAX_UNAUTHENTICATED {
            return Err(Refusal::Global);
        }
        let from_address = counts.per_address.get(&ip).copied().unwrap_or(0);
        if self.per_ip && from_address >= MAX_UNAUTHENTICATED_PER_IP {
            return Err(Refusal::PerAddress);
        }
        open_unauthenticated(&mut counts, ip);
        Ok(Slot {
            limits: self.clone(),
            held: Held::Unauthenticated,
            ip,
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
        release(&mut counts, slot.held, slot.ip);
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
        release(&mut counts, slot.held, slot.ip);
        open_unauthenticated(&mut counts, slot.ip);
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

fn open_unauthenticated(counts: &mut Counts, ip: IpAddr) {
    counts.unauthenticated += 1;
    *counts.per_address.entry(ip).or_insert(0) += 1;
}

/// Gives back what `held` took. The address is counted only while the slot
/// is not signed in, so a user's slot never touches `per_address`.
fn release(counts: &mut Counts, held: Held, ip: IpAddr) {
    match held {
        Held::Unauthenticated => {
            counts.unauthenticated = counts.unauthenticated.saturating_sub(1);
            decrement(&mut counts.per_address, &ip);
        }
        Held::User(user) => decrement(&mut counts.per_user, &user),
    }
}

fn decrement<K: Eq + Hash>(map: &mut HashMap<K, u32>, key: &K) {
    if let Some(n) = map.get_mut(key) {
        *n = n.saturating_sub(1);
        if *n == 0 {
            map.remove(key);
        }
    }
}

/// One connection's place in the counts; released when dropped.
pub struct Slot {
    limits: ConnectionLimits,
    held: Held,
    /// The client's address as [`ip_key`] keys it, kept from `admit` so
    /// `sign_out` can count the address again.
    ip: IpAddr,
}

impl Slot {
    pub fn is_signed_in(&self) -> bool {
        matches!(self.held, Held::User(_))
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        release(&mut self.limits.lock(), self.held, self.ip);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::net::Ipv4Addr;

    use super::*;

    fn addr(last: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(192, 0, 2, last))
    }

    #[test]
    fn unauthenticated_connections_are_capped_and_released_on_drop() {
        let limits = ConnectionLimits::default();
        let slots: Vec<_> = (0..MAX_UNAUTHENTICATED)
            .map(|_| limits.admit(addr(1)).unwrap())
            .collect();
        assert_eq!(limits.admit(addr(2)).err(), Some(Refusal::Global));
        drop(slots);
        assert_eq!(limits.unauthenticated(), 0);
        assert!(limits.admit(addr(2)).is_ok());
    }

    #[test]
    fn one_address_is_capped_while_another_is_admitted() {
        let limits = ConnectionLimits::default().with_per_ip(true);
        let mut slots: Vec<_> = (0..MAX_UNAUTHENTICATED_PER_IP)
            .map(|_| limits.admit(addr(1)).unwrap())
            .collect();
        assert_eq!(limits.admit(addr(1)).err(), Some(Refusal::PerAddress));
        assert_eq!(limits.unauthenticated(), MAX_UNAUTHENTICATED_PER_IP);
        let other = limits.admit(addr(2));
        assert!(other.is_ok());
        slots.push(other.unwrap());
    }

    #[test]
    fn releasing_a_slot_frees_its_address_again() {
        let limits = ConnectionLimits::default().with_per_ip(true);
        let mut slots: Vec<_> = (0..MAX_UNAUTHENTICATED_PER_IP)
            .map(|_| limits.admit(addr(1)).unwrap())
            .collect();
        slots.truncate(MAX_UNAUTHENTICATED_PER_IP as usize - 1);
        assert!(limits.admit(addr(1)).is_ok());
    }

    #[test]
    fn signing_in_frees_the_address_slot() {
        let limits = ConnectionLimits::default().with_per_ip(true);
        let user = UserId::new();
        let mut first = limits.admit(addr(1)).unwrap();
        let _rest: Vec<_> = (1..MAX_UNAUTHENTICATED_PER_IP)
            .map(|_| limits.admit(addr(1)).unwrap())
            .collect();
        assert!(limits.admit(addr(1)).is_err());
        limits.sign_in(&mut first, user, 1).unwrap();
        assert!(limits.admit(addr(1)).is_ok());
    }

    /// A logout puts the connection back in the address's count, so an
    /// address cannot shed its cap by signing in and out.
    #[test]
    fn signing_out_counts_the_address_again() {
        let limits = ConnectionLimits::default().with_per_ip(true);
        let user = UserId::new();
        let mut first = limits.admit(addr(1)).unwrap();
        let _rest: Vec<_> = (1..MAX_UNAUTHENTICATED_PER_IP)
            .map(|_| limits.admit(addr(1)).unwrap())
            .collect();
        limits.sign_in(&mut first, user, 1).unwrap();
        let refilled = limits.admit(addr(1)).unwrap();
        limits.sign_out(&mut first);
        assert!(limits.admit(addr(1)).is_err());
        drop(refilled);
    }

    #[test]
    fn disabled_per_address_cap_admits_past_the_address_limit() {
        let limits = ConnectionLimits::default();
        let slots: Vec<_> = (0..MAX_UNAUTHENTICATED_PER_IP + 1)
            .map(|_| limits.admit(addr(1)).unwrap())
            .collect();
        assert_eq!(slots.len(), MAX_UNAUTHENTICATED_PER_IP as usize + 1);
    }

    /// A logout must not keep the user's place: otherwise N logouts on
    /// connections left open lock the user out.
    #[test]
    fn signing_out_releases_the_user_and_counts_unauthenticated() {
        let limits = ConnectionLimits::default();
        let user = UserId::new();
        let mut slot = limits.admit(addr(1)).unwrap();
        limits.sign_in(&mut slot, user, 1).unwrap();
        limits.sign_out(&mut slot);
        assert_eq!(limits.held_by(user), 0);
        assert_eq!(limits.unauthenticated(), 1);
        assert!(!slot.is_signed_in());
        drop(slot);
        assert_eq!(limits.unauthenticated(), 0);
    }

    #[test]
    fn signing_in_moves_the_slot_and_caps_per_user() {
        let limits = ConnectionLimits::default();
        let user = UserId::new();
        let mut a = limits.admit(addr(1)).unwrap();
        let mut b = limits.admit(addr(1)).unwrap();
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

    #[test]
    fn switching_user_releases_the_first() {
        let limits = ConnectionLimits::default();
        let (u1, u2) = (UserId::new(), UserId::new());
        let mut slot = limits.admit(addr(1)).unwrap();
        limits.sign_in(&mut slot, u1, 4).unwrap();
        limits.sign_in(&mut slot, u2, 4).unwrap();
        assert_eq!((limits.held_by(u1), limits.held_by(u2)), (0, 1));
    }
}
