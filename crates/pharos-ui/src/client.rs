//! Server client. Two layers:
//!
//! - **Parse helpers** (always compiled, host-testable). Map Jellyfin-shaped
//!   JSON bytes into `LoggedInUser` / `Vec<LibraryItem>`.
//! - **Transport** (gated by `web` feature, WASM-only). Wraps the parse
//!   helpers around `gloo_net::http::Request`.
//!
//! V16: only the public Jellyfin-compat surface is consumed. No backdoor.

use crate::api_types::{ItemKind, LibraryItem, LoggedInUser};
use serde::Deserialize;

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("http: {0}")]
    Http(String),
    #[error("parse: {0}")]
    Parse(String),
    #[error("status {0}")]
    Status(u16),
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct AuthResponseDto {
    user: AuthUserDto,
    server_id: String,
    access_token: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct AuthUserDto {
    id: String,
    name: String,
    policy: AuthPolicyDto,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct AuthPolicyDto {
    is_administrator: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ItemsResponseDto {
    items: Vec<ItemDto>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ItemDto {
    id: String,
    name: String,
    #[serde(rename = "Type")]
    kind: String,
}

pub fn parse_auth_response(bytes: &[u8]) -> Result<LoggedInUser, ClientError> {
    let parsed: AuthResponseDto =
        serde_json::from_slice(bytes).map_err(|e| ClientError::Parse(e.to_string()))?;
    Ok(LoggedInUser {
        id: parsed.user.id,
        name: parsed.user.name,
        server_id: parsed.server_id,
        access_token: parsed.access_token,
        is_admin: parsed.user.policy.is_administrator,
    })
}

pub fn parse_items_response(bytes: &[u8]) -> Result<Vec<LibraryItem>, ClientError> {
    let parsed: ItemsResponseDto =
        serde_json::from_slice(bytes).map_err(|e| ClientError::Parse(e.to_string()))?;
    Ok(parsed
        .items
        .into_iter()
        .map(|i| LibraryItem {
            id: i.id,
            name: i.name,
            kind: ItemKind::from_jellyfin_type(&i.kind),
        })
        .collect())
}

/// T50 — admin user-list parser. The Jellyfin `/Users` endpoint
/// returns a bare array of `UserDto` (NOT wrapped in `ItemsResult`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdminUser {
    pub id: String,
    pub name: String,
    pub is_admin: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct AdminUserDto {
    id: String,
    name: String,
    policy: AdminUserPolicyDto,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct AdminUserPolicyDto {
    is_administrator: bool,
}

pub fn parse_admin_users_response(bytes: &[u8]) -> Result<Vec<AdminUser>, ClientError> {
    let parsed: Vec<AdminUserDto> =
        serde_json::from_slice(bytes).map_err(|e| ClientError::Parse(e.to_string()))?;
    Ok(parsed
        .into_iter()
        .map(|u| AdminUser {
            id: u.id,
            name: u.name,
            is_admin: u.policy.is_administrator,
        })
        .collect())
}

#[cfg(feature = "web")]
pub mod web {
    //! gloo-net HTTP wrappers. Browser-only. Each call composes the
    //! relevant parse helper above so unit tests of the parse layer
    //! cover the JSON contract.

    use super::*;
    use gloo_net::http::Request;
    use serde::Serialize;

    #[derive(Serialize)]
    #[serde(rename_all = "PascalCase")]
    struct AuthRequestBody<'a> {
        username: &'a str,
        pw: &'a str,
    }

    pub async fn authenticate(
        base: &str,
        username: &str,
        password: &str,
    ) -> Result<LoggedInUser, ClientError> {
        let body = serde_json::to_string(&AuthRequestBody {
            username,
            pw: password,
        })
        .map_err(|e| ClientError::Parse(e.to_string()))?;
        let resp = Request::post(&format!("{base}/Users/AuthenticateByName"))
            .header("Content-Type", "application/json")
            .body(body)
            .map_err(|e| ClientError::Http(e.to_string()))?
            .send()
            .await
            .map_err(|e| ClientError::Http(e.to_string()))?;
        if !resp.ok() {
            return Err(ClientError::Status(resp.status()));
        }
        let bytes = resp
            .binary()
            .await
            .map_err(|e| ClientError::Http(e.to_string()))?;
        parse_auth_response(&bytes)
    }

    pub async fn list_items(base: &str, token: &str) -> Result<Vec<LibraryItem>, ClientError> {
        let resp = Request::get(&format!("{base}/Items"))
            .header("X-Emby-Token", token)
            .send()
            .await
            .map_err(|e| ClientError::Http(e.to_string()))?;
        if !resp.ok() {
            return Err(ClientError::Status(resp.status()));
        }
        let bytes = resp
            .binary()
            .await
            .map_err(|e| ClientError::Http(e.to_string()))?;
        parse_items_response(&bytes)
    }

    #[derive(Serialize)]
    #[serde(rename_all = "PascalCase")]
    struct CreateUserBody<'a> {
        name: &'a str,
        password: &'a str,
    }

    pub async fn admin_list_users(
        base: &str,
        token: &str,
    ) -> Result<Vec<AdminUser>, ClientError> {
        let resp = Request::get(&format!("{base}/Users"))
            .header("X-Emby-Token", token)
            .send()
            .await
            .map_err(|e| ClientError::Http(e.to_string()))?;
        if !resp.ok() {
            return Err(ClientError::Status(resp.status()));
        }
        let bytes = resp
            .binary()
            .await
            .map_err(|e| ClientError::Http(e.to_string()))?;
        parse_admin_users_response(&bytes)
    }

    pub async fn admin_create_user(
        base: &str,
        token: &str,
        name: &str,
        password: &str,
    ) -> Result<(), ClientError> {
        let body = serde_json::to_string(&CreateUserBody { name, password })
            .map_err(|e| ClientError::Parse(e.to_string()))?;
        let resp = Request::post(&format!("{base}/Users/New"))
            .header("X-Emby-Token", token)
            .header("Content-Type", "application/json")
            .body(body)
            .map_err(|e| ClientError::Http(e.to_string()))?
            .send()
            .await
            .map_err(|e| ClientError::Http(e.to_string()))?;
        if !resp.ok() {
            return Err(ClientError::Status(resp.status()));
        }
        Ok(())
    }

    pub async fn admin_delete_user(
        base: &str,
        token: &str,
        user_id: &str,
    ) -> Result<(), ClientError> {
        let resp = Request::delete(&format!("{base}/Users/{user_id}"))
            .header("X-Emby-Token", token)
            .send()
            .await
            .map_err(|e| ClientError::Http(e.to_string()))?;
        if !resp.ok() {
            return Err(ClientError::Status(resp.status()));
        }
        Ok(())
    }

    pub async fn admin_library_refresh(base: &str, token: &str) -> Result<(), ClientError> {
        let resp = Request::post(&format!("{base}/Library/Refresh"))
            .header("X-Emby-Token", token)
            .body("")
            .map_err(|e| ClientError::Http(e.to_string()))?
            .send()
            .await
            .map_err(|e| ClientError::Http(e.to_string()))?;
        if !resp.ok() {
            return Err(ClientError::Status(resp.status()));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    const AUTH_BODY: &[u8] = br#"{
        "User": {
            "Name": "ali",
            "Id": "abc123",
            "Policy": {
                "IsAdministrator": true,
                "IsHidden": false,
                "IsDisabled": false,
                "EnableMediaPlayback": true,
                "EnableAllChannels": true,
                "EnableAllFolders": true,
                "EnableAllDevices": true,
                "EnablePublicSharing": false,
                "EnableRemoteAccess": true,
                "EnableContentDeletion": true,
                "EnableContentDownloading": true,
                "EnableSyncTranscoding": true,
                "EnableMediaConversion": true,
                "EnableAudioPlaybackTranscoding": true,
                "EnableVideoPlaybackTranscoding": true,
                "EnablePlaybackRemuxing": true
            }
        },
        "SessionInfo": {},
        "AccessToken": "tok-xyz",
        "ServerId": "srv-1"
    }"#;

    const ITEMS_BODY: &[u8] = br#"{
        "Items": [
            {"Id":"1","Name":"Movie One","Type":"Movie","ServerId":"srv-1","MediaType":"Video","IsFolder":false,"UserData":{"Played":false,"PlayCount":0}},
            {"Id":"2","Name":"Song","Type":"Audio","ServerId":"srv-1","MediaType":"Audio","IsFolder":false,"UserData":{"Played":false,"PlayCount":0}}
        ],
        "TotalRecordCount": 2,
        "StartIndex": 0
    }"#;

    #[test]
    fn parse_auth_extracts_token_and_user() {
        let u = parse_auth_response(AUTH_BODY).unwrap();
        assert_eq!(u.id, "abc123");
        assert_eq!(u.name, "ali");
        assert_eq!(u.access_token, "tok-xyz");
        assert_eq!(u.server_id, "srv-1");
        assert!(u.is_admin);
    }

    #[test]
    fn parse_items_maps_kind_and_drops_unknown_fields() {
        let items = parse_items_response(ITEMS_BODY).unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].id, "1");
        assert_eq!(items[0].kind, ItemKind::Movie);
        assert_eq!(items[1].kind, ItemKind::Audio);
    }

    #[test]
    fn parse_auth_garbage_is_parse_err() {
        let r = parse_auth_response(b"not json");
        assert!(matches!(r, Err(ClientError::Parse(_))));
    }

    #[test]
    fn parse_items_empty_array_returns_empty_vec() {
        let body =
            br#"{"Items":[],"TotalRecordCount":0,"StartIndex":0}"#;
        let items = parse_items_response(body).unwrap();
        assert!(items.is_empty());
    }
}
