//! Android TV sign-in logic with no I/O (spec 025): the reconnect backoff and
//! the QuickConnect state machine. It compiles on every target so host tests
//! cover it; `native/android.rs` does the sockets and files and asks this
//! module what to do next.
//!
//! Why it exists (spike findings): the server closes a socket that has not
//! signed in within 30 s (`auth_deadline`), reconnecting on every poll gets
//! `429 Too Many Requests` on the upgrade, an unapproved code expires after
//! about 10 minutes (`notFound`), and a call on a closed socket hangs.

use std::time::Duration;

/// First reconnect delay.
pub const BACKOFF_START: Duration = Duration::from_secs(2);
/// Reconnect delays stop doubling here.
pub const BACKOFF_CAP: Duration = Duration::from_secs(64);
/// Time between QuickConnect polls while the code waits for approval.
pub const POLL_INTERVAL: Duration = Duration::from_secs(3);
/// Longest a QuickConnect poll may take before it counts as lost.
pub const POLL_TIMEOUT: Duration = Duration::from_secs(10);
/// Longest any other call may take before it counts as lost.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(10);

/// Exponential delay: 2 s, 4 s, ... capped at 64 s, until `reset`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Backoff {
    next: Duration,
}

impl Backoff {
    pub fn new() -> Self {
        Self {
            next: BACKOFF_START,
        }
    }

    /// The delay to wait now; the one after it doubles, up to the cap.
    pub fn next_delay(&mut self) -> Duration {
        let now = self.next;
        self.next = (now * 2).min(BACKOFF_CAP);
        now
    }

    /// Start again at the first delay (after any answer from the server).
    pub fn reset(&mut self) {
        self.next = BACKOFF_START;
    }
}

impl Default for Backoff {
    fn default() -> Self {
        Self::new()
    }
}

/// What one QuickConnect poll came back with. `T` is the sign-in on approval.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Polled<T> {
    /// The code was approved.
    Approved(T),
    /// The server answered `notPermitted`: nobody has approved the code yet.
    NotYet,
    /// The server answered `notFound`: the code expired or was never minted.
    NotFound,
    /// No answer within `POLL_TIMEOUT`, and the socket is still open.
    TimedOut,
    /// The socket has closed (so no answer will come).
    Closed,
    /// Any other server error, e.g. rate limited.
    Failed(String),
}

impl<T> Polled<T> {
    /// Classify a server error by its API kind (`ClientError::Api::kind`).
    pub fn from_api_error(kind: &str, message: &str) -> Self {
        match kind {
            "notPermitted" => Polled::NotYet,
            "notFound" => Polled::NotFound,
            other => Polled::Failed(format!("{other}: {message}")),
        }
    }
}

/// What the QuickConnect loop does next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Signed in; stop.
    Finish,
    /// Poll the same code again after the delay.
    PollAfter(Duration),
    /// The code expired: wait, then mint a fresh one.
    RemintAfter(Duration),
    /// Drop the closed socket, wait, connect again, then keep polling the
    /// same secret (the server accepts it from a new connection).
    ReconnectAfter(Duration),
}

/// The QuickConnect loop's state: the backoff and the secret being polled.
#[derive(Debug, Clone, Default)]
pub struct QuickConnect {
    backoff: Backoff,
    secret: Option<String>,
    /// The current code has not been answered with anything but errors yet.
    fresh: bool,
    /// The last code died `notFound` on its first poll; keep backing off.
    fresh_not_found: bool,
}

impl QuickConnect {
    pub fn new() -> Self {
        Self::default()
    }

    /// The secret to poll with, if a code is pending.
    pub fn secret(&self) -> Option<&str> {
        self.secret.as_deref()
    }

    /// A code was minted. A mint proves connect and initiate work, so the
    /// backoff resets, unless the previous code was already `notFound` on its
    /// first poll (then the server is not keeping codes: keep growing).
    pub fn on_minted(&mut self, secret: String) {
        if !self.fresh_not_found {
            self.backoff.reset();
        }
        self.secret = Some(secret);
        self.fresh = true;
    }

    /// A stored token was resumed over a working connection.
    pub fn on_resumed(&mut self) {
        self.backoff.reset();
    }

    /// Next step after a poll. A `notPermitted`/approved answer proves the
    /// code lives and resets the backoff; a lost poll or an error backs off.
    /// Only `notFound` drops the secret: a closed socket keeps it.
    pub fn on_poll<T>(&mut self, polled: &Polled<T>) -> Action {
        match polled {
            Polled::Approved(_) => {
                self.backoff.reset();
                self.fresh_not_found = false;
                Action::Finish
            }
            Polled::NotYet => {
                self.backoff.reset();
                self.fresh = false;
                self.fresh_not_found = false;
                Action::PollAfter(POLL_INTERVAL)
            }
            Polled::NotFound => {
                self.secret = None;
                // Never faster than the poll interval; a code that was
                // already gone on its first poll grows the delay instead.
                let wait = if self.fresh {
                    self.fresh_not_found = true;
                    self.backoff.next_delay().max(POLL_INTERVAL)
                } else {
                    self.backoff.reset();
                    POLL_INTERVAL
                };
                self.fresh = false;
                Action::RemintAfter(wait)
            }
            Polled::Closed => Action::ReconnectAfter(self.backoff.next_delay()),
            Polled::TimedOut | Polled::Failed(_) => Action::PollAfter(self.backoff.next_delay()),
        }
    }

    /// Delay before retrying a failed connect or a failed code mint.
    pub fn retry_delay(&mut self) -> Duration {
        self.backoff.next_delay()
    }
}

/// The app id from a `/proc/self/cmdline` first argument: the process name
/// minus any `:process` suffix. Rejects anything that is not shaped like a
/// package name, so it cannot steer the path.
pub fn package_from_cmdline(raw: &str) -> Result<String, String> {
    let name = raw.split(':').next().unwrap_or_default();
    if name.is_empty() || name.contains('/') || !name.contains('.') || name.contains("..") {
        return Err(format!("not a package name: {raw:?}"));
    }
    Ok(name.to_owned())
}

/// The native API socket URL for a server `base`. Only `https://` is
/// accepted: the socket carries the QuickConnect secret and the device token
/// it buys, so a cleartext `http://` base is refused, never quietly turned
/// into `ws://`. A trailing `/` on `base` is dropped.
pub fn rpc_url(base: &str) -> Result<String, String> {
    let Some(host) = base.strip_prefix("https://") else {
        return Err(format!(
            "refusing server {base:?}: the TV app only connects over https://"
        ));
    };
    let host = host.trim_end_matches('/');
    if host.is_empty() {
        return Err(format!("no host in server {base:?}"));
    }
    if host.chars().any(char::is_whitespace) {
        return Err(format!("whitespace in the host of server {base:?}"));
    }
    // Only `host[:port]`: userinfo (`trusted@attacker`) would send the
    // secret to the host after the `@`, and a path, query or fragment
    // would land in front of `/pharos/rpc`.
    if let Some(bad) = host
        .chars()
        .find(|c| matches!(c, '@' | '?' | '#' | '/' | '\\'))
    {
        return Err(format!(
            "{bad:?} in the host of server {base:?}: give only https://host[:port]"
        ));
    }
    check_authority(host).map_err(|why| format!("{why} in server {base:?}"))?;
    Ok(format!("wss://{host}/pharos/rpc"))
}

/// `host[:port]` or `[v6][:port]`, with at most one port separator and a
/// port of 1..=65535. A malformed authority is a permanent error, so it is
/// caught here instead of failing every reconnect attempt.
fn check_authority(authority: &str) -> Result<(), String> {
    let (host, port) = if let Some(rest) = authority.strip_prefix('[') {
        let Some((v6, after)) = rest.split_once(']') else {
            return Err(format!("unclosed '[' in host {authority:?}"));
        };
        if v6.is_empty()
            || !v6
                .chars()
                .all(|c| c.is_ascii_hexdigit() || c == ':' || c == '.')
        {
            return Err(format!("bad IPv6 address {v6:?}"));
        }
        match after {
            "" => (v6, None),
            _ => match after.strip_prefix(':') {
                Some(port) => (v6, Some(port)),
                None => return Err(format!("{after:?} after the IPv6 address")),
            },
        }
    } else {
        match authority.split_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (authority, None),
        }
    };
    if host.is_empty() {
        return Err(format!("no host in {authority:?}"));
    }
    if !authority.starts_with('[') && host.contains(['[', ']']) {
        return Err(format!("stray bracket in host {host:?}"));
    }
    if let Some(port) = port {
        let valid = !port.is_empty()
            && port.chars().all(|c| c.is_ascii_digit())
            && port.parse::<u16>().is_ok_and(|p| p != 0);
        if !valid {
            return Err(format!("bad port {port:?} (want 1..=65535)"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The socket carries the QuickConnect secret and the device token, so a
    /// cleartext base is refused. Disarm check: mapping `http://` to `ws://`
    /// again fails this.
    #[test]
    fn the_rpc_url_is_wss_and_a_cleartext_base_is_refused() {
        assert_eq!(
            rpc_url("https://tv.example.com").as_deref(),
            Ok("wss://tv.example.com/pharos/rpc")
        );
        assert_eq!(
            rpc_url("https://tv.example.com/").as_deref(),
            Ok("wss://tv.example.com/pharos/rpc")
        );
        assert_eq!(
            rpc_url("https://tv.example.com:8443").as_deref(),
            Ok("wss://tv.example.com:8443/pharos/rpc")
        );
        assert_eq!(
            rpc_url("https://[::1]:8443").as_deref(),
            Ok("wss://[::1]:8443/pharos/rpc")
        );
        for bad in [
            "http://tv.example.com",
            "ws://tv.example.com",
            "tv.example.com",
            "https://",
            "https://tv.exa mple.com",
            "https://tv.example.com\t",
            "https://tv.example.com@attacker.example",
            "https://tv.example.com?x=1",
            "https://tv.example.com#frag",
            "https://tv.example.com/jellyfin",
            "https://tv.example.com:8443:443",
            "https://tv.example.com:",
            "https://tv.example.com:0",
            "https://tv.example.com:65536",
            "https://tv.example.com:84a3",
            "https://:8443",
            "https://[::1",
            "https://[::1]x",
            "https://tv]example.com",
        ] {
            assert!(rpc_url(bad).is_err(), "{bad} must be refused");
        }
    }

    fn secs(s: u64) -> Duration {
        Duration::from_secs(s)
    }

    #[test]
    fn backoff_doubles_from_two_seconds_and_caps_at_sixty_four() {
        let mut b = Backoff::new();
        let got: Vec<_> = (0..8).map(|_| b.next_delay()).collect();
        assert_eq!(
            got,
            [2, 4, 8, 16, 32, 64, 64, 64].map(secs),
            "2 s doubling, capped at 64 s"
        );
    }

    #[test]
    fn backoff_reset_starts_over() {
        let mut b = Backoff::new();
        b.next_delay();
        b.next_delay();
        b.reset();
        assert_eq!(b.next_delay(), secs(2));
    }

    #[test]
    fn approval_finishes() {
        assert_eq!(
            QuickConnect::new().on_poll(&Polled::Approved(())),
            Action::Finish
        );
    }

    #[test]
    fn not_yet_polls_again_in_three_seconds() {
        assert_eq!(
            QuickConnect::new().on_poll::<()>(&Polled::NotYet),
            Action::PollAfter(secs(3))
        );
    }

    #[test]
    fn an_expired_code_is_replaced_after_the_poll_interval() {
        let mut qc = QuickConnect::new();
        qc.on_minted("s".into());
        qc.on_poll::<()>(&Polled::NotYet);
        assert_eq!(
            qc.on_poll::<()>(&Polled::NotFound),
            Action::RemintAfter(secs(3))
        );
        assert_eq!(qc.secret(), None);
    }

    #[test]
    fn a_fresh_code_that_is_not_found_backs_off_across_mints() {
        let mut qc = QuickConnect::new();
        let mut waits = vec![];
        for _ in 0..4 {
            qc.on_minted("s".into());
            match qc.on_poll::<()>(&Polled::NotFound) {
                Action::RemintAfter(d) => waits.push(d),
                other => panic!("{other:?}"),
            }
        }
        assert_eq!(
            waits,
            [3, 4, 8, 16].map(secs),
            "never under 3 s, then growing"
        );
    }

    #[test]
    fn closed_keeps_the_secret_and_not_found_drops_it() {
        let mut qc = QuickConnect::new();
        qc.on_minted("s".into());
        qc.on_poll::<()>(&Polled::NotYet);
        assert!(matches!(
            qc.on_poll::<()>(&Polled::Closed),
            Action::ReconnectAfter(_)
        ));
        assert_eq!(qc.secret(), Some("s"));
        qc.on_poll::<()>(&Polled::NotFound);
        assert_eq!(qc.secret(), None);
    }

    #[test]
    fn a_mint_or_resume_resets_the_backoff() {
        let mut qc = QuickConnect::new();
        qc.retry_delay();
        qc.retry_delay();
        qc.on_minted("s".into());
        assert_eq!(qc.retry_delay(), secs(2));
        qc.retry_delay();
        qc.on_resumed();
        assert_eq!(qc.retry_delay(), secs(2));
    }

    #[test]
    fn package_names_are_parsed_from_the_process_name() {
        let ok = package_from_cmdline;
        assert_eq!(
            ok("com.alisonjenkins.pharos.tv"),
            Ok("com.alisonjenkins.pharos.tv".into())
        );
        assert_eq!(
            ok("com.alisonjenkins.pharos.tv:sync"),
            Ok("com.alisonjenkins.pharos.tv".into())
        );
        assert!(ok("").is_err());
        assert!(ok("/system/bin/app_process").is_err());
        assert!(ok("../x.y").is_err());
        assert!(ok("pharos").is_err(), "no dot");
    }

    #[test]
    fn closed_reconnects_with_growing_backoff() {
        let mut qc = QuickConnect::new();
        assert_eq!(
            qc.on_poll::<()>(&Polled::Closed),
            Action::ReconnectAfter(secs(2))
        );
        assert_eq!(
            qc.on_poll::<()>(&Polled::Closed),
            Action::ReconnectAfter(secs(4))
        );
    }

    #[test]
    fn timeout_and_errors_retry_the_poll_on_the_same_socket() {
        let mut qc = QuickConnect::new();
        assert_eq!(
            qc.on_poll::<()>(&Polled::TimedOut),
            Action::PollAfter(secs(2))
        );
        assert_eq!(
            qc.on_poll::<()>(&Polled::Failed("rateLimited: slow down".into())),
            Action::PollAfter(secs(4))
        );
    }

    #[test]
    fn any_server_answer_resets_the_backoff() {
        let mut qc = QuickConnect::new();
        qc.on_poll::<()>(&Polled::Closed);
        qc.on_poll::<()>(&Polled::Closed);
        qc.on_poll::<()>(&Polled::NotYet);
        assert_eq!(
            qc.on_poll::<()>(&Polled::Closed),
            Action::ReconnectAfter(secs(2))
        );
    }

    #[test]
    fn api_kinds_map_to_poll_answers() {
        assert_eq!(
            Polled::<()>::from_api_error("notPermitted", "x"),
            Polled::NotYet
        );
        assert_eq!(
            Polled::<()>::from_api_error("notFound", "x"),
            Polled::NotFound
        );
        assert_eq!(
            Polled::<()>::from_api_error("tooManyRequests", "slow"),
            Polled::Failed("tooManyRequests: slow".into())
        );
    }
}
