//! Shared application state held in `actix_web::web::Data`.
//!
//! Concrete backend types are wired here so handlers stay free of generics.
//! Swap point: change the type aliases below — handlers are untouched.

use crate::auth::BuiltinAuth;
use pharos_store_sqlx::sqlite::SqliteStore;
use uuid::Uuid;

pub type Stores = SqliteStore;
pub type Auth = BuiltinAuth<Stores>;

pub struct AppState {
    pub stores: Stores,
    pub auth: Auth,
    pub server_id: String,
    pub server_name: String,
    pub version: &'static str,
}

impl AppState {
    pub fn new(stores: Stores, server_name: String) -> Self {
        let auth = BuiltinAuth::new(stores.clone());
        Self {
            stores,
            auth,
            server_id: Uuid::new_v4().simple().to_string(),
            server_name,
            version: env!("CARGO_PKG_VERSION"),
        }
    }
}
