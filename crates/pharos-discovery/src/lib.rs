//! pharos-discovery — LAN/UPnP/SSDP + live-TV backends, lifted out of
//! `pharos-server` so the discovery cluster can be tested in
//! isolation and reused by future renderers.
//!
//! - [`ssdp`] — SSDP/UPnP responder. Owns its UDP socket; handlers
//!   are pure (no AppState) and reach the server via a single
//!   `SsdpResponder::spawn(server_id, server_name, advertise_url)`
//!   entry point.
//! - [`dlna_xml`] — pure helpers for the DLNA `MediaServer:1` SOAP
//!   surface (device description, ContentDirectory Browse, SCPD
//!   schemas, SOAP/XML parsers). Actix handlers live in
//!   `pharos-server::dlna` and only call these helpers.
//! - [`live_tv`] — M3U+XMLTV `TunerBackend` impl. Pure config →
//!   `LiveChannel` / `EpgProgram` from `pharos-core`.

pub mod dlna_xml;
pub mod live_tv;
pub mod ssdp;
