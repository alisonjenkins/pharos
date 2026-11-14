//! A debug switch that makes the server unreachable from this app: every
//! connection is refused before it starts, as in a black spot, while the
//! phone's own network stays up for adb.

use std::sync::atomic::{AtomicBool, Ordering};

static UNREACHABLE: AtomicBool = AtomicBool::new(false);

/// Why a refused connection failed, for its error.
pub const WHY: &str = "debug offline: server unreachable";

pub fn set_unreachable(on: bool) {
    UNREACHABLE.store(on, Ordering::SeqCst);
}

pub fn unreachable() -> bool {
    UNREACHABLE.load(Ordering::SeqCst)
}
