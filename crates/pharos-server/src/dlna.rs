//! DLNA / UPnP MediaServer phase 1 (T48).
//!
//! Phase 1 scope: HTTP-level XML + SOAP endpoints. SSDP UDP-multicast
//! discovery is deferred to phase 2 — without it, clients need the
//! server URL configured manually, but the wire surface is identical
//! once discovery lands.
//!
//! Builds device + service descriptions on demand from the configured
//! `server_id` so a stable identity (T35) survives restarts. ContentDirectory
//! Browse returns library items as DIDL-Lite XML.

use crate::state::AppState;
use actix_web::{web, HttpRequest, HttpResponse};
use pharos_core::{MediaItem, MediaKind, MediaStore};

pub fn register(cfg: &mut web::ServiceConfig) {
    // T31 lowercase routes — LowercasePath middleware handles
    // PascalCase variants. UPnP clients send mixed case for both
    // /Dlna/{id}/... and the per-service paths.
    cfg.route(
        "/dlna/{server_id}/description.xml",
        web::get().to(description),
    )
    .route(
        "/dlna/{server_id}/contentdirectory/scpd.xml",
        web::get().to(content_directory_scpd),
    )
    .route(
        "/dlna/{server_id}/contentdirectory/control",
        web::post().to(content_directory_control),
    )
    .route(
        "/dlna/{server_id}/contentdirectory/events",
        web::route().to(events_stub),
    )
    .route(
        "/dlna/{server_id}/connectionmanager/scpd.xml",
        web::get().to(connection_manager_scpd),
    )
    .route(
        "/dlna/{server_id}/connectionmanager/control",
        web::post().to(connection_manager_control),
    );
}

async fn description(
    req: HttpRequest,
    state: web::Data<AppState>,
    path: web::Path<String>,
) -> HttpResponse {
    let server_id = path.into_inner();
    if server_id != state.server_id {
        return HttpResponse::NotFound().body("");
    }
    let base = base_url_from_request(&req);
    let body = device_description_xml(&state.server_id, &state.server_name, &base);
    HttpResponse::Ok()
        .content_type("text/xml; charset=\"utf-8\"")
        .body(body)
}

async fn content_directory_scpd(
    state: web::Data<AppState>,
    path: web::Path<String>,
) -> HttpResponse {
    if path.into_inner() != state.server_id {
        return HttpResponse::NotFound().body("");
    }
    HttpResponse::Ok()
        .content_type("text/xml; charset=\"utf-8\"")
        .body(CONTENT_DIRECTORY_SCPD)
}

async fn connection_manager_scpd(
    state: web::Data<AppState>,
    path: web::Path<String>,
) -> HttpResponse {
    if path.into_inner() != state.server_id {
        return HttpResponse::NotFound().body("");
    }
    HttpResponse::Ok()
        .content_type("text/xml; charset=\"utf-8\"")
        .body(CONNECTION_MANAGER_SCPD)
}

async fn content_directory_control(
    req: HttpRequest,
    state: web::Data<AppState>,
    path: web::Path<String>,
    body: web::Bytes,
) -> HttpResponse {
    if path.into_inner() != state.server_id {
        return HttpResponse::NotFound().body("");
    }
    let body_str = std::str::from_utf8(&body).unwrap_or_default();
    let action = soap_action_name(&req);
    match action.as_deref() {
        Some("Browse") => {
            // Pull ObjectID + StartingIndex + RequestedCount. Pharos
            // doesn't model folders yet so any ObjectID resolves to
            // the flat library — but pagination is honoured so control
            // points that scroll a 50k library don't re-fetch the
            // whole DIDL response on every page.
            let object_id = extract_xml_tag(body_str, "ObjectID").unwrap_or_default();
            let starting_index = parse_browse_u32(body_str, "StartingIndex").unwrap_or(0);
            let requested_count = parse_browse_u32(body_str, "RequestedCount").unwrap_or(0);
            let items = match state.stores.list().await {
                Ok(v) => v,
                Err(e) => {
                    return HttpResponse::InternalServerError().body(e.to_string());
                }
            };
            let base = base_url_from_request(&req);
            let xml = browse_response_xml(
                &items,
                &object_id,
                &base,
                &state.server_id,
                starting_index,
                requested_count,
            );
            HttpResponse::Ok()
                .content_type("text/xml; charset=\"utf-8\"")
                .body(xml)
        }
        Some("GetSortCapabilities") | Some("GetSearchCapabilities") | Some("GetSystemUpdateID") => {
            HttpResponse::Ok()
                .content_type("text/xml; charset=\"utf-8\"")
                .body(empty_soap_response(&action.unwrap_or_default()))
        }
        _ => HttpResponse::BadRequest().body("unknown SOAPAction"),
    }
}

async fn connection_manager_control(
    req: HttpRequest,
    state: web::Data<AppState>,
    path: web::Path<String>,
    _body: web::Bytes,
) -> HttpResponse {
    if path.into_inner() != state.server_id {
        return HttpResponse::NotFound().body("");
    }
    let action = soap_action_name(&req).unwrap_or_default();
    HttpResponse::Ok()
        .content_type("text/xml; charset=\"utf-8\"")
        .body(empty_soap_response(&action))
}

async fn events_stub() -> HttpResponse {
    // UPnP GENA events — SUBSCRIBE/UNSUBSCRIBE. Phase 1 acknowledges
    // without actually delivering events.
    HttpResponse::Ok()
        .insert_header(("SID", "uuid:pharos-events-0"))
        .insert_header(("TIMEOUT", "Second-1800"))
        .finish()
}

/// Parse `SOAPACTION: "urn:schemas-upnp-org:service:ContentDirectory:1#Browse"`
/// → `Some("Browse")`.
pub fn soap_action_name(req: &HttpRequest) -> Option<String> {
    let raw = req
        .headers()
        .get("SOAPACTION")
        .or_else(|| req.headers().get("SoapAction"))
        .or_else(|| req.headers().get("soapaction"))?;
    let s = raw.to_str().ok()?.trim().trim_matches('"');
    s.rsplit_once('#').map(|(_, name)| name.to_string())
}

/// Light XML reader for the SOAP body: pull the text of the first
/// `<TAG>...</TAG>` (namespace-agnostic — splits on the closing `>`).
pub fn extract_xml_tag(body: &str, tag: &str) -> Option<String> {
    let needle = format!("<{tag}");
    let i = body.find(&needle)?;
    let after_open = &body[i + needle.len()..];
    let gt = after_open.find('>')?;
    let inner = &after_open[gt + 1..];
    let close = format!("</{tag}>");
    let end = inner.find(&close)?;
    Some(inner[..end].trim().to_string())
}

fn base_url_from_request(req: &HttpRequest) -> String {
    let scheme = req.connection_info().scheme().to_string();
    let host = req.connection_info().host().to_string();
    format!("{scheme}://{host}")
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

pub fn device_description_xml(server_id: &str, server_name: &str, base: &str) -> String {
    let safe_name = xml_escape(server_name);
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<root xmlns="urn:schemas-upnp-org:device-1-0">
  <specVersion><major>1</major><minor>0</minor></specVersion>
  <URLBase>{base}</URLBase>
  <device>
    <deviceType>urn:schemas-upnp-org:device:MediaServer:1</deviceType>
    <friendlyName>{safe_name}</friendlyName>
    <manufacturer>pharos</manufacturer>
    <modelName>pharos</modelName>
    <modelNumber>1</modelNumber>
    <UDN>uuid:{server_id}</UDN>
    <serviceList>
      <service>
        <serviceType>urn:schemas-upnp-org:service:ContentDirectory:1</serviceType>
        <serviceId>urn:upnp-org:serviceId:ContentDirectory</serviceId>
        <SCPDURL>/Dlna/{server_id}/ContentDirectory/scpd.xml</SCPDURL>
        <controlURL>/Dlna/{server_id}/ContentDirectory/control</controlURL>
        <eventSubURL>/Dlna/{server_id}/ContentDirectory/events</eventSubURL>
      </service>
      <service>
        <serviceType>urn:schemas-upnp-org:service:ConnectionManager:1</serviceType>
        <serviceId>urn:upnp-org:serviceId:ConnectionManager</serviceId>
        <SCPDURL>/Dlna/{server_id}/ConnectionManager/scpd.xml</SCPDURL>
        <controlURL>/Dlna/{server_id}/ConnectionManager/control</controlURL>
        <eventSubURL>/Dlna/{server_id}/ConnectionManager/events</eventSubURL>
      </service>
    </serviceList>
  </device>
</root>
"#
    )
}

/// Parse an integer-typed UPnP Browse argument. Missing / unparseable
/// fields yield None — caller decides the default (0 = "from start"
/// for StartingIndex; 0 = "all" for RequestedCount).
pub fn parse_browse_u32(body: &str, tag: &str) -> Option<u32> {
    extract_xml_tag(body, tag).and_then(|s| s.trim().parse().ok())
}

/// Soft cap on a single Browse response so a control point that asks
/// for "all" doesn't drag a 50k-item DIDL across the wire in one shot.
/// Real clients paginate already; this is a safety belt.
pub const DLNA_BROWSE_MAX_PAGE: u32 = 1_000;

/// Build a `Browse` SOAP response carrying a paginated slice of the
/// library as DIDL-Lite `<item>` entries under the root container.
///
/// `starting_index` is 0-based per UPnP. `requested_count = 0` means
/// "all" — we cap at [`DLNA_BROWSE_MAX_PAGE`].
pub fn browse_response_xml(
    items: &[MediaItem],
    object_id: &str,
    base: &str,
    _server_id: &str,
    starting_index: u32,
    requested_count: u32,
) -> String {
    let total = items.len() as u32;
    let start = starting_index.min(total);
    let cap = if requested_count == 0 {
        DLNA_BROWSE_MAX_PAGE
    } else {
        requested_count.min(DLNA_BROWSE_MAX_PAGE)
    };
    let end = (start.saturating_add(cap)).min(total);
    let page = &items[start as usize..end as usize];

    let mut didl = String::with_capacity(512);
    didl.push_str(r#"<DIDL-Lite xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:upnp="urn:schemas-upnp-org:metadata-1-0/upnp/">"#);
    for item in page {
        let id = item.id;
        let title = xml_escape(&item.title);
        let class = match item.kind {
            MediaKind::Movie | MediaKind::Episode => "object.item.videoItem",
            MediaKind::Audio => "object.item.audioItem.musicTrack",
        };
        let protocol_info = match item.kind {
            MediaKind::Audio => "http-get:*:audio/mpeg:*",
            _ => "http-get:*:video/webm:*",
        };
        let url = match item.kind {
            MediaKind::Audio => format!("{base}/Audio/{id}/stream"),
            _ => format!("{base}/Videos/{id}/stream"),
        };
        didl.push_str(&format!(
            r#"<item id="{id}" parentID="0" restricted="1"><dc:title>{title}</dc:title><upnp:class>{class}</upnp:class><res protocolInfo="{protocol_info}">{url}</res></item>"#
        ));
    }
    didl.push_str("</DIDL-Lite>");
    let didl_escaped = xml_escape(&didl);
    let number_returned = page.len();
    let object_id_safe = xml_escape(object_id);
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/">
  <s:Body>
    <u:BrowseResponse xmlns:u="urn:schemas-upnp-org:service:ContentDirectory:1">
      <Result>{didl_escaped}</Result>
      <NumberReturned>{number_returned}</NumberReturned>
      <TotalMatches>{total}</TotalMatches>
      <UpdateID>1</UpdateID>
      <ObjectID>{object_id_safe}</ObjectID>
    </u:BrowseResponse>
  </s:Body>
</s:Envelope>
"#
    )
}

fn empty_soap_response(action: &str) -> String {
    let action_safe = xml_escape(action);
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/">
  <s:Body>
    <u:{action_safe}Response xmlns:u="urn:schemas-upnp-org:service:ContentDirectory:1"/>
  </s:Body>
</s:Envelope>
"#
    )
}

const CONTENT_DIRECTORY_SCPD: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<scpd xmlns="urn:schemas-upnp-org:service-1-0">
  <specVersion><major>1</major><minor>0</minor></specVersion>
  <actionList>
    <action>
      <name>Browse</name>
      <argumentList>
        <argument><name>ObjectID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_ObjectID</relatedStateVariable></argument>
        <argument><name>BrowseFlag</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_BrowseFlag</relatedStateVariable></argument>
        <argument><name>Filter</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_Filter</relatedStateVariable></argument>
        <argument><name>StartingIndex</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_Index</relatedStateVariable></argument>
        <argument><name>RequestedCount</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_Count</relatedStateVariable></argument>
        <argument><name>SortCriteria</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_SortCriteria</relatedStateVariable></argument>
        <argument><name>Result</name><direction>out</direction><relatedStateVariable>A_ARG_TYPE_Result</relatedStateVariable></argument>
        <argument><name>NumberReturned</name><direction>out</direction><relatedStateVariable>A_ARG_TYPE_Count</relatedStateVariable></argument>
        <argument><name>TotalMatches</name><direction>out</direction><relatedStateVariable>A_ARG_TYPE_Count</relatedStateVariable></argument>
        <argument><name>UpdateID</name><direction>out</direction><relatedStateVariable>A_ARG_TYPE_UpdateID</relatedStateVariable></argument>
      </argumentList>
    </action>
    <action><name>GetSortCapabilities</name></action>
    <action><name>GetSearchCapabilities</name></action>
    <action><name>GetSystemUpdateID</name></action>
  </actionList>
  <serviceStateTable>
    <stateVariable sendEvents="no"><name>A_ARG_TYPE_ObjectID</name><dataType>string</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>A_ARG_TYPE_BrowseFlag</name><dataType>string</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>A_ARG_TYPE_Filter</name><dataType>string</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>A_ARG_TYPE_Index</name><dataType>ui4</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>A_ARG_TYPE_Count</name><dataType>ui4</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>A_ARG_TYPE_SortCriteria</name><dataType>string</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>A_ARG_TYPE_Result</name><dataType>string</dataType></stateVariable>
    <stateVariable sendEvents="yes"><name>A_ARG_TYPE_UpdateID</name><dataType>ui4</dataType></stateVariable>
  </serviceStateTable>
</scpd>
"#;

const CONNECTION_MANAGER_SCPD: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<scpd xmlns="urn:schemas-upnp-org:service-1-0">
  <specVersion><major>1</major><minor>0</minor></specVersion>
  <actionList>
    <action><name>GetProtocolInfo</name></action>
    <action><name>GetCurrentConnectionIDs</name></action>
  </actionList>
  <serviceStateTable></serviceStateTable>
</scpd>
"#;

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fixture_items() -> Vec<MediaItem> {
        vec![
            MediaItem {
                id: 1,
                path: PathBuf::from("/m/1.webm"),
                title: "Movie 1 & friends".into(),
                kind: MediaKind::Movie,
                ..Default::default()
            },
            MediaItem {
                id: 5,
                path: PathBuf::from("/m/5.mp3"),
                title: "Track".into(),
                kind: MediaKind::Audio,
                ..Default::default()
            },
        ]
    }

    #[test]
    fn device_description_carries_server_id_and_service_urls() {
        let xml = device_description_xml("abc123", "pharos-test", "http://example");
        assert!(xml.contains("uuid:abc123"));
        assert!(xml.contains("pharos-test"));
        assert!(xml.contains("MediaServer:1"));
        assert!(xml.contains("/Dlna/abc123/ContentDirectory/scpd.xml"));
        assert!(xml.contains("/Dlna/abc123/ConnectionManager/control"));
        assert!(xml.contains("<URLBase>http://example</URLBase>"));
    }

    #[test]
    fn browse_response_emits_one_didl_item_per_media_item() {
        let xml = browse_response_xml(&fixture_items(), "0", "http://x", "srv", 0, 0);
        assert!(xml.contains("<NumberReturned>2</NumberReturned>"));
        assert!(xml.contains("<TotalMatches>2</TotalMatches>"));
        // DIDL is double-escaped — `<item id="1"` becomes
        // `&lt;item id=&quot;1&quot;`.
        assert!(xml.contains("&lt;item id=&quot;1&quot;"));
        assert!(xml.contains("&lt;item id=&quot;5&quot;"));
        // Movie uses /Videos URL, audio uses /Audio.
        assert!(xml.contains("http://x/Videos/1/stream"));
        assert!(xml.contains("http://x/Audio/5/stream"));
    }

    #[test]
    fn browse_response_xml_escapes_title() {
        // `Movie 1 & friends` must arrive as `&amp;` even after the
        // double-escape (the DIDL inner XML is itself entity-encoded).
        let xml = browse_response_xml(&fixture_items(), "0", "http://x", "srv", 0, 0);
        // After two passes, `&` → `&amp;` (first pass) → `&amp;amp;` (second).
        assert!(xml.contains("Movie 1 &amp;amp; friends"));
    }

    #[test]
    fn browse_response_honours_starting_index_and_requested_count() {
        // Build 5 fake items.
        let items: Vec<MediaItem> = (0..5)
            .map(|i| MediaItem {
                id: i,
                path: PathBuf::from(format!("/m/{i}.mkv")),
                title: format!("Item {i}"),
                kind: MediaKind::Movie,
                ..Default::default()
            })
            .collect();
        let xml = browse_response_xml(&items, "0", "http://x", "srv", 1, 2);
        // Window {1, 2} → 2 returned, total still 5.
        assert!(xml.contains("<NumberReturned>2</NumberReturned>"), "{xml}");
        assert!(xml.contains("<TotalMatches>5</TotalMatches>"));
        // Item 1 and 2 present, 0/3/4 absent (DIDL doubly-escaped).
        assert!(xml.contains("&lt;item id=&quot;1&quot;"));
        assert!(xml.contains("&lt;item id=&quot;2&quot;"));
        assert!(!xml.contains("&lt;item id=&quot;0&quot;"));
        assert!(!xml.contains("&lt;item id=&quot;3&quot;"));
    }

    #[test]
    fn parse_browse_u32_handles_present_and_missing_tags() {
        let body = r#"<Browse><StartingIndex>7</StartingIndex></Browse>"#;
        assert_eq!(parse_browse_u32(body, "StartingIndex"), Some(7));
        assert_eq!(parse_browse_u32(body, "RequestedCount"), None);
    }

    #[test]
    fn extract_xml_tag_handles_namespaced_open_tag() {
        let body = r#"<s:Envelope><Browse><ObjectID xmlns="x">42</ObjectID></Browse></s:Envelope>"#;
        assert_eq!(extract_xml_tag(body, "ObjectID"), Some("42".into()));
    }

    #[test]
    fn extract_xml_tag_returns_none_when_absent() {
        assert!(extract_xml_tag("<x>1</x>", "ObjectID").is_none());
    }
}
