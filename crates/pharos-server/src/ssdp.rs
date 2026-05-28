//! SSDP UDP-multicast responder (T48 phase 2).
//!
//! Listens on `239.255.255.250:1900` for SSDP `M-SEARCH` discovery
//! requests and replies with HTTP-style unicast 200 responses that
//! point control points at pharos's DLNA description URL
//! (`/Dlna/{server_id}/description.xml`, already served by `dlna.rs`).
//!
//! Wire shape (M-SEARCH request → unicast reply):
//!
//! ```text
//! M-SEARCH * HTTP/1.1
//! HOST: 239.255.255.250:1900
//! MAN: "ssdp:discover"
//! MX: 1
//! ST: urn:schemas-upnp-org:device:MediaServer:1
//!
//! HTTP/1.1 200 OK
//! CACHE-CONTROL: max-age=1800
//! EXT:
//! LOCATION: http://{advertise}/Dlna/{server_id}/description.xml
//! SERVER: pharos/{ver}
//! ST: urn:schemas-upnp-org:device:MediaServer:1
//! USN: uuid:{server_id}::urn:schemas-upnp-org:device:MediaServer:1
//! ```
//!
//! Multiple `ST` queries get one response each. Pharos answers
//! `ssdp:all`, `upnp:rootdevice`, the MediaServer URN, and our exact
//! UUID. Unknown STs are ignored (per spec).
//!
//! Periodic NOTIFY `ssdp:alive` multicasts + a `ssdp:byebye` on shutdown
//! follow the same shape — clients refresh + invalidate accordingly.

use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::time::Duration;
use tokio::net::UdpSocket;

const MCAST_GROUP: Ipv4Addr = Ipv4Addr::new(239, 255, 255, 250);
const MCAST_PORT: u16 = 1900;
/// Targets pharos answers M-SEARCH for.
const ANSWERABLE_TARGETS: &[&str] = &[
    "ssdp:all",
    "upnp:rootdevice",
    "urn:schemas-upnp-org:device:MediaServer:1",
];

/// SSDP responder handle. Drop → task ends (no graceful byebye yet —
/// the network sees the device fall silent and ages it out via
/// CACHE-CONTROL).
pub struct SsdpResponder {
    handle: tokio::task::JoinHandle<()>,
}

impl SsdpResponder {
    /// Bind + spawn the listener. Returns immediately with a handle
    /// the caller stashes on AppState; the task runs until dropped.
    /// `advertise_url` is the externally-reachable origin published
    /// in LOCATION (e.g. `http://192.168.1.10:8096`).
    pub async fn spawn(
        server_id: String,
        server_name: String,
        advertise_url: String,
    ) -> io::Result<Self> {
        let sock = bind_multicast()?;
        let server_header = format!("pharos/{} UPnP/1.0 DLNADOC/1.50", env!("CARGO_PKG_VERSION"));
        let _ = server_name;
        let handle = tokio::spawn(async move {
            run_loop(sock, server_id, advertise_url, server_header).await;
        });
        Ok(Self { handle })
    }

    pub fn abort(self) {
        self.handle.abort();
    }
}

fn bind_multicast() -> io::Result<UdpSocket> {
    // Bind via the std API so we can set SO_REUSEADDR + multicast
    // membership before flipping to non-blocking.
    use std::net::UdpSocket as StdSocket;
    let bind = SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), MCAST_PORT);
    let raw = socket2::Socket::new(
        socket2::Domain::IPV4,
        socket2::Type::DGRAM,
        Some(socket2::Protocol::UDP),
    )?;
    raw.set_reuse_address(true)?;
    #[cfg(unix)]
    raw.set_reuse_port(true)?;
    raw.bind(&bind.into())?;
    raw.join_multicast_v4(&MCAST_GROUP, &Ipv4Addr::UNSPECIFIED)?;
    let std_sock: StdSocket = raw.into();
    std_sock.set_nonblocking(true)?;
    UdpSocket::from_std(std_sock)
}

async fn run_loop(
    sock: UdpSocket,
    server_id: String,
    advertise_url: String,
    server_header: String,
) {
    let mut buf = vec![0u8; 2048];
    // Periodic ssdp:alive broadcast every 30 minutes (max-age is
    // 1800; refresh slightly earlier so clients don't expire).
    let mut alive_timer = tokio::time::interval(Duration::from_secs(1500));
    alive_timer.tick().await; // Skip the immediate tick — wait for next.

    // Send an immediate alive on startup so existing control points
    // pick us up without waiting for their next discovery scan.
    send_alives(&sock, &server_id, &advertise_url, &server_header).await;

    loop {
        tokio::select! {
            res = sock.recv_from(&mut buf) => {
                let Ok((n, peer)) = res else { continue };
                let Ok(req) = std::str::from_utf8(&buf[..n]) else { continue };
                handle_search(&sock, peer, req, &server_id, &advertise_url, &server_header).await;
            }
            _ = alive_timer.tick() => {
                send_alives(&sock, &server_id, &advertise_url, &server_header).await;
            }
        }
    }
}

async fn handle_search(
    sock: &UdpSocket,
    peer: SocketAddr,
    req: &str,
    server_id: &str,
    advertise_url: &str,
    server_header: &str,
) {
    if !req.starts_with("M-SEARCH") {
        return;
    }
    let Some(st) = parse_header(req, "ST") else {
        return;
    };
    for target in matched_targets(&st, server_id) {
        let reply = build_msearch_response(server_id, advertise_url, server_header, &target);
        let _ = sock.send_to(reply.as_bytes(), peer).await;
    }
}

/// Compute the (ST, USN) pairs pharos answers for a given M-SEARCH ST.
/// Returns one per matched target; ssdp:all expands to every entry.
fn matched_targets(st: &str, server_id: &str) -> Vec<String> {
    let uuid_target = format!("uuid:{server_id}");
    if st == "ssdp:all" {
        return ANSWERABLE_TARGETS
            .iter()
            .map(|s| s.to_string())
            .chain(std::iter::once(uuid_target))
            .filter(|s| *s != "ssdp:all")
            .collect();
    }
    if st == uuid_target {
        return vec![uuid_target];
    }
    ANSWERABLE_TARGETS
        .iter()
        .filter(|t| **t == st)
        .map(|t| t.to_string())
        .collect()
}

pub fn build_msearch_response(
    server_id: &str,
    advertise_url: &str,
    server_header: &str,
    target: &str,
) -> String {
    let usn = usn_for(server_id, target);
    let location = format!("{advertise_url}/Dlna/{server_id}/description.xml");
    format!(
        "HTTP/1.1 200 OK\r\n\
         CACHE-CONTROL: max-age=1800\r\n\
         EXT:\r\n\
         LOCATION: {location}\r\n\
         SERVER: {server_header}\r\n\
         ST: {target}\r\n\
         USN: {usn}\r\n\
         \r\n",
    )
}

fn usn_for(server_id: &str, target: &str) -> String {
    if target == format!("uuid:{server_id}") {
        return target.to_string();
    }
    format!("uuid:{server_id}::{target}")
}

async fn send_alives(
    sock: &UdpSocket,
    server_id: &str,
    advertise_url: &str,
    server_header: &str,
) {
    let mcast: SocketAddr = SocketAddr::new(MCAST_GROUP.into(), MCAST_PORT);
    let uuid_target = format!("uuid:{server_id}");
    let targets: Vec<String> = ANSWERABLE_TARGETS
        .iter()
        .filter(|s| **s != "ssdp:all")
        .map(|s| s.to_string())
        .chain(std::iter::once(uuid_target))
        .collect();
    for target in targets {
        let msg = build_notify_alive(server_id, advertise_url, server_header, &target);
        let _ = sock.send_to(msg.as_bytes(), mcast).await;
    }
}

pub fn build_notify_alive(
    server_id: &str,
    advertise_url: &str,
    server_header: &str,
    target: &str,
) -> String {
    let usn = usn_for(server_id, target);
    let location = format!("{advertise_url}/Dlna/{server_id}/description.xml");
    format!(
        "NOTIFY * HTTP/1.1\r\n\
         HOST: 239.255.255.250:1900\r\n\
         CACHE-CONTROL: max-age=1800\r\n\
         LOCATION: {location}\r\n\
         NT: {target}\r\n\
         NTS: ssdp:alive\r\n\
         SERVER: {server_header}\r\n\
         USN: {usn}\r\n\
         \r\n",
    )
}

/// Case-insensitive single-header parser (M-SEARCH headers are
/// case-insensitive). Returns None if the header is missing or the
/// value is empty.
fn parse_header(req: &str, name: &str) -> Option<String> {
    for line in req.split("\r\n") {
        if let Some((k, v)) = line.split_once(':') {
            if k.trim().eq_ignore_ascii_case(name) {
                let v = v.trim();
                // ST: "ssdp:discover" (quoted in MAN, occasionally ST).
                let v = v.trim_matches('"');
                if v.is_empty() {
                    return None;
                }
                return Some(v.to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn parse_header_case_insensitive_and_strips_quotes() {
        let req = "M-SEARCH * HTTP/1.1\r\nMAN: \"ssdp:discover\"\r\nST: upnp:rootdevice\r\n\r\n";
        assert_eq!(parse_header(req, "man").as_deref(), Some("ssdp:discover"));
        assert_eq!(
            parse_header(req, "ST").as_deref(),
            Some("upnp:rootdevice"),
        );
        assert_eq!(parse_header(req, "MX"), None);
    }

    #[test]
    fn matched_targets_ssdp_all_includes_uuid_and_root_and_mediaserver() {
        let v = matched_targets("ssdp:all", "abc123");
        assert!(v.contains(&"upnp:rootdevice".to_string()));
        assert!(v.contains(&"urn:schemas-upnp-org:device:MediaServer:1".to_string()));
        assert!(v.contains(&"uuid:abc123".to_string()));
    }

    #[test]
    fn matched_targets_uuid_only_returns_uuid() {
        let v = matched_targets("uuid:abc123", "abc123");
        assert_eq!(v, vec!["uuid:abc123".to_string()]);
    }

    #[test]
    fn matched_targets_unknown_st_returns_empty() {
        let v = matched_targets("urn:does-not-exist:1", "abc123");
        assert!(v.is_empty());
    }

    #[test]
    fn msearch_response_carries_location_and_usn() {
        let r = build_msearch_response(
            "abc123",
            "http://192.168.1.10:8096",
            "pharos/0.0.0 UPnP/1.0 DLNADOC/1.50",
            "upnp:rootdevice",
        );
        assert!(r.starts_with("HTTP/1.1 200 OK\r\n"), "{r}");
        assert!(r.contains("LOCATION: http://192.168.1.10:8096/Dlna/abc123/description.xml"), "{r}");
        assert!(r.contains("ST: upnp:rootdevice"), "{r}");
        assert!(r.contains("USN: uuid:abc123::upnp:rootdevice"), "{r}");
        assert!(r.contains("CACHE-CONTROL: max-age=1800"), "{r}");
        // Two trailing CRLFs.
        assert!(r.ends_with("\r\n\r\n"), "{r}");
    }

    #[test]
    fn msearch_response_uuid_target_has_self_usn() {
        let r = build_msearch_response("abc", "http://h", "s", "uuid:abc");
        assert!(r.contains("USN: uuid:abc\r\n"), "{r}");
    }

    #[test]
    fn notify_alive_carries_nt_and_nts() {
        let n = build_notify_alive(
            "abc",
            "http://h:8096",
            "pharos/0.0.0",
            "upnp:rootdevice",
        );
        assert!(n.starts_with("NOTIFY * HTTP/1.1\r\n"), "{n}");
        assert!(n.contains("NT: upnp:rootdevice"), "{n}");
        assert!(n.contains("NTS: ssdp:alive"), "{n}");
        assert!(n.contains("LOCATION: http://h:8096/Dlna/abc/description.xml"), "{n}");
        assert!(n.contains("HOST: 239.255.255.250:1900"), "{n}");
    }
}
