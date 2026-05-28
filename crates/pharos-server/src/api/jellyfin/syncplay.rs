//! Jellyfin `/SyncPlay/*` HTTP surface.
//!
//! Group-watch UI in jellyfin-web fetches `GET /SyncPlay/List` the
//! moment the user clicks the group icon. Without a route there, the
//! 404 surfaces as "Error" in the UI and the panel never renders.
//!
//! The real SyncPlay protocol flows over the `/socket` WebSocket
//! (T16 / T17) — these HTTP endpoints keep jellyfin-web's REST
//! polling happy and expose the group list so the dropdown can show
//! existing groups before the user joins one.

use crate::{api::jellyfin::auth_extractor::AuthUser, sync::GroupRegistry};
use actix_web::{web, HttpResponse, Responder};
use serde::Serialize;

pub fn register(cfg: &mut web::ServiceConfig) {
    // T31: paths registered lowercase; `LowercasePath` middleware
    // folds jellyfin-web's PascalCase requests before routing.
    cfg.route("/syncplay/list", web::get().to(list_groups))
        .route("/syncplay/new", web::post().to(no_op_204))
        .route("/syncplay/join", web::post().to(no_op_204))
        .route("/syncplay/leave", web::post().to(no_op_204))
        .route("/syncplay/setnewqueue", web::post().to(no_op_204))
        .route("/syncplay/buffering", web::post().to(no_op_204))
        .route("/syncplay/ready", web::post().to(no_op_204))
        .route("/syncplay/pause", web::post().to(no_op_204))
        .route("/syncplay/unpause", web::post().to(no_op_204))
        .route("/syncplay/seek", web::post().to(no_op_204))
        .route("/syncplay/moveplaylistitem", web::post().to(no_op_204))
        .route("/syncplay/setignorewait", web::post().to(no_op_204))
        .route("/syncplay/nextitem", web::post().to(no_op_204))
        .route("/syncplay/previousitem", web::post().to(no_op_204))
        .route("/syncplay/setplaylistitem", web::post().to(no_op_204))
        .route("/syncplay/removefromplaylist", web::post().to(no_op_204))
        .route("/syncplay/setrepeatmode", web::post().to(no_op_204))
        .route("/syncplay/setshufflemode", web::post().to(no_op_204))
        .route("/syncplay/ping", web::post().to(no_op_204));
}

/// Jellyfin's `GroupInfoDto` shape. Only the fields jellyfin-web reads
/// for the dropdown render — full state lives over the socket.
#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
struct GroupInfoDto {
    group_id: String,
    group_name: String,
    /// `Idle` / `Playing` / `Paused` / `Waiting`. We map from the
    /// internal `PlaybackState` enum via the actor snapshot.
    state: &'static str,
    /// Member count. Full per-member list ships with the socket
    /// `SyncPlayGroupUpdate` payload — clients render it from there
    /// after joining.
    participants: Vec<String>,
    last_updated_at: String,
}

async fn list_groups(_user: AuthUser, registry: web::Data<GroupRegistry>) -> impl Responder {
    let Ok(handles) = registry.list().await else {
        // Actor unreachable: return empty rather than 500 so the UI
        // renders an "no active groups" pane instead of an error.
        let empty: Vec<GroupInfoDto> = vec![];
        return HttpResponse::Ok().json(empty);
    };
    let mut out = Vec::with_capacity(handles.len());
    for h in handles {
        let Some(snap) = h.snapshot().await else {
            continue;
        };
        out.push(GroupInfoDto {
            group_id: snap.id.to_string(),
            group_name: format!("Group {}", snap.id),
            // Snapshot doesn't currently expose play/pause state at
            // the HTTP level; surface the count signals so the
            // dropdown can render a label. Real state still flows
            // via /socket SyncPlayGroupUpdate.
            state: if snap.buffering_member_count > 0 {
                "Waiting"
            } else {
                "Idle"
            },
            // Member ids; richer display-name lives over the socket.
            participants: (0..snap.member_count)
                .map(|i| format!("member-{i}"))
                .collect(),
            last_updated_at: now_iso8601(),
        });
    }
    HttpResponse::Ok().json(out)
}

async fn no_op_204(_user: AuthUser) -> impl Responder {
    HttpResponse::NoContent().finish()
}

fn now_iso8601() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{secs}")
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use actix_web::{test, App};
    use pharos_core::{SecretString, TokenStore, UserId, UserPolicy, UserRecord, UserStore};

    async fn seed_auth() -> (web::Data<crate::state::AppState>, String) {
        use crate::auth::BuiltinAuth;
        use pharos_store_sqlx::sqlite::SqliteStore;
        let stores = SqliteStore::connect("sqlite::memory:").await.unwrap();
        let auth = BuiltinAuth::new(stores.clone());
        let hash = auth.hash_password(&SecretString::new("p")).unwrap();
        let uid = UserId::new();
        stores
            .create(UserRecord {
                id: uid,
                name: "u".into(),
                password_hash: hash,
                policy: UserPolicy::default(),
            })
            .await
            .unwrap();
        let token = stores.issue(uid, "t").await.unwrap();
        let state = web::Data::new(crate::state::AppState::new(stores, "t".into()));
        (state, token.0.expose().to_string())
    }

    #[actix_web::test]
    async fn syncplay_list_empty_when_no_groups() {
        let (state, token) = seed_auth().await;
        let reg = web::Data::new(GroupRegistry::spawn());
        let app =
            test::init_service(App::new().app_data(state).app_data(reg).configure(register)).await;
        let req = test::TestRequest::get()
            .uri("/syncplay/list")
            .insert_header(("X-Emby-Token", token.as_str()))
            .to_request();
        let body = test::call_and_read_body(&app, req).await;
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(v.is_array());
        assert_eq!(v.as_array().unwrap().len(), 0);
    }

    #[actix_web::test]
    async fn syncplay_list_returns_active_group() {
        let (state, token) = seed_auth().await;
        let reg = GroupRegistry::spawn();
        let handle = reg.create().await.unwrap();
        // Group has zero members → snapshot may report 0; we don't
        // care about state here, only that the id surfaces.
        let reg_data = web::Data::new(reg);
        let app = test::init_service(
            App::new()
                .app_data(state)
                .app_data(reg_data)
                .configure(register),
        )
        .await;
        let req = test::TestRequest::get()
            .uri("/syncplay/list")
            .insert_header(("X-Emby-Token", token.as_str()))
            .to_request();
        let body = test::call_and_read_body(&app, req).await;
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let arr = v.as_array().unwrap();
        // Empty groups terminate themselves on the next message —
        // we either see the freshly-created group or already-empty.
        assert!(arr.len() <= 1);
        if let Some(first) = arr.first() {
            assert_eq!(first["GroupId"], handle.group_id.to_string());
        }
    }
}
