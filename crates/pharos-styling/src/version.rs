//! Version stamps that key the compiled-track cache.

/// Bumped whenever the compiler's output for the same inputs changes.
pub const COMPILER_VERSION: u32 = 1;

/// Bumped whenever the director would pick different effects for the same
/// source. Stored with every plan so stale plans can be re-directed.
pub const DIRECTOR_VERSION: u32 = 1;
