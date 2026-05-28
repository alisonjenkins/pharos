// This crate is dev-only — drives compat tests against a live pharos
// server. expect/unwrap is fine here; V17 applies to production code.
#![allow(clippy::expect_used, clippy::unwrap_used)]

//! Test-only Jellyfin HTTP client. Drives pharos the way a real
//! Jellyfin device would: `X-Emby-Authorization` header with
//! Client/Device/DeviceId/Version, body PascalCase, token via
//! `X-Emby-Token` once obtained.
//!
//! **All response DTOs use `#[serde(deny_unknown_fields)]` is**
//! *deliberately not* applied — Jellyfin sends many fields, we don't
//! care about all of them, and tests should pass even when we extend
//! the schema. Compat coverage instead asserts every *required* field
//! we deserialize is present and the right type. A *missing* required
//! field will surface as a serde error.
//!
//! V11: this client is the V11 evidence that the Jellyfin endpoints
//! we ship behave like a real client expects, not just like a
//! same-process test harness expects.

use reqwest::{header::HeaderMap, Client, StatusCode};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
    #[error("status {status}: {body}")]
    Status { status: u16, body: String },
    #[error("parse: {0}")]
    Parse(String),
}

#[derive(Debug, Clone)]
pub struct DeviceInfo {
    pub client: String,
    pub device: String,
    pub device_id: String,
    pub version: String,
}

impl Default for DeviceInfo {
    fn default() -> Self {
        Self {
            client: "pharos-test".into(),
            device: "rust-test".into(),
            device_id: Uuid::new_v4().simple().to_string(),
            version: "0.0.0".into(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct JellyfinClient {
    base: String,
    device: DeviceInfo,
    http: Client,
    token: Option<String>,
}

impl JellyfinClient {
    pub fn new(base: impl Into<String>, device: DeviceInfo) -> Self {
        Self {
            base: base.into(),
            device,
            http: Client::new(),
            token: None,
        }
    }

    pub fn token(&self) -> Option<&str> {
        self.token.as_deref()
    }

    pub fn base_url(&self) -> &str {
        &self.base
    }

    fn auth_header_value(&self, token: Option<&str>) -> String {
        let mut v = format!(
            r#"MediaBrowser Client="{}", Device="{}", DeviceId="{}", Version="{}""#,
            self.device.client, self.device.device, self.device.device_id, self.device.version
        );
        if let Some(t) = token {
            v.push_str(&format!(r#", Token="{t}""#));
        }
        v
    }

    fn auth_headers(&self) -> HeaderMap {
        let mut h = HeaderMap::new();
        let v = self.auth_header_value(self.token.as_deref());
        h.insert(
            "X-Emby-Authorization",
            v.parse().expect("static-shape header value"),
        );
        if let Some(t) = self.token.as_deref() {
            h.insert("X-Emby-Token", t.parse().expect("token is ascii"));
        }
        h
    }

    // ------ public Jellyfin API surface ------

    pub async fn system_info_public(&self) -> Result<SystemInfo, ClientError> {
        get_json(
            &self.http,
            &format!("{}/System/Info/Public", self.base),
            HeaderMap::new(),
        )
        .await
    }

    /// Authenticate by name; stash token in the client so subsequent
    /// calls auto-attach. Returns the full auth result for assertion.
    pub async fn authenticate_by_name(
        &mut self,
        username: &str,
        password: &str,
    ) -> Result<AuthenticationResult, ClientError> {
        let mut h = HeaderMap::new();
        h.insert(
            "X-Emby-Authorization",
            self.auth_header_value(None)
                .parse()
                .expect("static-shape header"),
        );
        let body = serde_json::json!({
            "Username": username,
            "Pw": password,
        });
        let resp = self
            .http
            .post(format!("{}/Users/AuthenticateByName", self.base))
            .headers(h)
            .json(&body)
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ClientError::Status {
                status: status.as_u16(),
                body,
            });
        }
        let bytes = resp.bytes().await?;
        let parsed: AuthenticationResult =
            serde_json::from_slice(&bytes).map_err(|e| ClientError::Parse(e.to_string()))?;
        self.token = Some(parsed.access_token.clone());
        Ok(parsed)
    }

    pub async fn users_me(&self) -> Result<User, ClientError> {
        get_json(
            &self.http,
            &format!("{}/Users/Me", self.base),
            self.auth_headers(),
        )
        .await
    }

    pub async fn items(&self) -> Result<ItemsResult, ClientError> {
        get_json(
            &self.http,
            &format!("{}/Items", self.base),
            self.auth_headers(),
        )
        .await
    }

    pub async fn item(&self, id: &str) -> Result<BaseItem, ClientError> {
        get_json(
            &self.http,
            &format!("{}/Items/{id}", self.base),
            self.auth_headers(),
        )
        .await
    }

    pub async fn library_virtual_folders(&self) -> Result<Vec<VirtualFolder>, ClientError> {
        get_json(
            &self.http,
            &format!("{}/Library/VirtualFolders", self.base),
            self.auth_headers(),
        )
        .await
    }

    pub async fn videos_stream_head(&self, item_id: &str) -> Result<StatusCode, ClientError> {
        let resp = self
            .http
            .head(format!("{}/Videos/{item_id}/stream", self.base))
            .headers(self.auth_headers())
            .send()
            .await?;
        Ok(resp.status())
    }

    pub async fn sessions(&self) -> Result<Vec<SessionInfo>, ClientError> {
        get_json(
            &self.http,
            &format!("{}/Sessions", self.base),
            self.auth_headers(),
        )
        .await
    }
}

async fn get_json<T>(client: &Client, url: &str, headers: HeaderMap) -> Result<T, ClientError>
where
    T: for<'de> Deserialize<'de>,
{
    let resp = client.get(url).headers(headers).send().await?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(ClientError::Status {
            status: status.as_u16(),
            body,
        });
    }
    let bytes = resp.bytes().await?;
    serde_json::from_slice(&bytes).map_err(|e| ClientError::Parse(e.to_string()))
}

// ------ response DTOs ------
// These mirror what a real Jellyfin device library expects (V7).
// Lenient on unknown fields, strict on required field names + types.

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct SystemInfo {
    pub id: String,
    pub server_name: String,
    pub version: String,
    pub product_name: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct AuthenticationResult {
    pub user: User,
    pub session_info: SessionInfo,
    pub access_token: String,
    pub server_id: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct User {
    pub name: String,
    pub server_id: String,
    pub id: String,
    pub has_password: bool,
    pub policy: UserPolicy,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct UserPolicy {
    pub is_administrator: bool,
    pub enable_media_playback: bool,
    pub enable_remote_access: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct SessionInfo {
    pub id: String,
    pub user_id: String,
    pub user_name: String,
    pub device_id: String,
    pub server_id: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct ItemsResult {
    pub items: Vec<BaseItem>,
    pub total_record_count: u32,
    pub start_index: u32,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct BaseItem {
    pub id: String,
    pub name: String,
    pub server_id: String,
    #[serde(rename = "Type")]
    pub kind: String,
    pub media_type: String,
    pub is_folder: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct VirtualFolder {
    pub name: String,
    pub locations: Vec<String>,
    pub collection_type: String,
    pub item_id: String,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn auth_header_with_token_includes_token_field() {
        let mut c = JellyfinClient::new("http://x", DeviceInfo::default());
        c.token = Some("abc123".into());
        let v = c.auth_header_value(c.token.as_deref());
        assert!(v.starts_with("MediaBrowser "));
        assert!(v.contains(r#"Token="abc123""#));
    }

    #[test]
    fn auth_header_without_token_omits_token_field() {
        let c = JellyfinClient::new("http://x", DeviceInfo::default());
        let v = c.auth_header_value(None);
        assert!(!v.contains("Token="));
    }

    #[test]
    fn auth_result_deserializes_from_sample() {
        let sample = br#"{
            "User": {"Name":"u","ServerId":"s","Id":"i","HasPassword":true,
                     "Policy":{"IsAdministrator":false,"EnableMediaPlayback":true,"EnableRemoteAccess":true}},
            "SessionInfo": {"Id":"s","UserId":"u","UserName":"u","DeviceId":"d","ServerId":"s"},
            "AccessToken": "tok",
            "ServerId": "s"
        }"#;
        let r: AuthenticationResult = serde_json::from_slice(sample).unwrap();
        assert_eq!(r.access_token, "tok");
        assert_eq!(r.user.name, "u");
    }
}
