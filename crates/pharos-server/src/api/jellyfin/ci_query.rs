//! Case-INSENSITIVE query-string extractor.
//!
//! Jellyfin's API query params are camelCase and the real server (ASP.NET)
//! binds them **case-insensitively**, so official clients disagree on casing:
//! the jellyfin SDK (jellyfin-web 10.11 + every mobile/TV app) sends camelCase
//! (`seasonId`, `startIndex`, `userId`), while older jellyfin-web apiclient
//! paths send PascalCase (`SeasonId`). A case-SENSITIVE serde extractor
//! silently ignores the mismatched key — e.g. a `SeasonId`-only
//! `ShowsEpisodesQuery` returned EVERY season's episodes to any client sending
//! `seasonId`, so clicking a season showed the whole series (B18).
//!
//! [`CiQuery`] converts every query KEY to `snake_case` before parsing
//! (`SeasonId` and `seasonId` both → `season_id`), so a struct with ordinary
//! snake_case Rust fields (`#[serde(rename_all = "snake_case")]`, i.e. the
//! default) binds every casing. Values are left percent-encoded and untouched;
//! actix's own query parser decodes them.

use actix_web::{dev::Payload, error::ErrorBadRequest, web, Error, FromRequest, HttpRequest};
use serde::de::DeserializeOwned;
use std::future::{ready, Ready};

/// Drop-in replacement for `web::Query<T>` that matches query keys
/// case-insensitively. `T` must use `#[serde(rename_all = "lowercase")]`.
pub struct CiQuery<T>(pub T);

impl<T> CiQuery<T> {
    pub fn into_inner(self) -> T {
        self.0
    }
}

impl<T> std::ops::Deref for CiQuery<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.0
    }
}

impl<T: DeserializeOwned> FromRequest for CiQuery<T> {
    type Error = Error;
    type Future = Ready<Result<Self, Error>>;

    fn from_request(req: &HttpRequest, _: &mut Payload) -> Self::Future {
        let normalized = snake_case_query_keys(req.query_string());
        ready(
            web::Query::<T>::from_query(&normalized)
                .map(|q| CiQuery(q.into_inner()))
                .map_err(ErrorBadRequest),
        )
    }
}

/// Rewrite the KEY of every `key=value` pair to snake_case, leaving the (still
/// percent-encoded) value verbatim so no double-decode occurs.
///
/// REPEATED keys are merged into one comma-joined value: jellyfin-web sends
/// `fields=A&fields=B` for list params, and serde rejects a duplicate key on
/// a scalar field with a 400 — which broke search (B20). Jellyfin list params
/// are comma-CSV everywhere, so merging mirrors how ASP.NET's array binding
/// consumes the repeats.
fn snake_case_query_keys(qs: &str) -> String {
    let mut order: Vec<String> = Vec::new();
    let mut merged: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for pair in qs.split('&').filter(|p| !p.is_empty()) {
        let (k, v) = match pair.split_once('=') {
            Some((k, v)) => (to_snake_case(k), v),
            None => (to_snake_case(pair), ""),
        };
        match merged.get_mut(&k) {
            Some(existing) => {
                if !v.is_empty() {
                    if !existing.is_empty() {
                        existing.push(',');
                    }
                    existing.push_str(v);
                }
            }
            None => {
                order.push(k.clone());
                merged.insert(k, v.to_string());
            }
        }
    }
    order
        .into_iter()
        .map(|k| {
            let v = &merged[&k];
            if v.is_empty() {
                k
            } else {
                format!("{k}={v}")
            }
        })
        .collect::<Vec<_>>()
        .join("&")
}

/// `SeasonId` / `seasonId` → `season_id`; `Is4K` / `is4K` → `is_4k`. A new word
/// begins (underscore inserted) at an uppercase letter OR a digit that follows
/// a lowercase letter — matching the ordinary snake_case Rust field names the
/// query structs use (`is_4k`, `start_index`, `include_item_types`). An
/// uppercase after a digit or another uppercase does NOT split (`is4K` → the
/// `K` joins `4` → `is_4k`, not `is_4_k`), and an already-snake key is a fixed
/// point. Values are never passed here.
fn to_snake_case(key: &str) -> String {
    let mut out = String::with_capacity(key.len() + 4);
    let mut prev_lower = false;
    for c in key.chars() {
        if c.is_ascii_uppercase() {
            if prev_lower {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
            prev_lower = false;
        } else if c.is_ascii_digit() {
            if prev_lower {
                out.push('_');
            }
            out.push(c);
            prev_lower = false;
        } else {
            out.push(c);
            prev_lower = c.is_ascii_lowercase();
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snake_cases_keys_preserves_values() {
        // Both the SDK's camelCase and jellyfin-web's PascalCase collapse to
        // the same snake key that a plain Rust field name matches.
        assert_eq!(to_snake_case("SeasonId"), "season_id");
        assert_eq!(to_snake_case("seasonId"), "season_id");
        assert_eq!(to_snake_case("startTimeTicks"), "start_time_ticks");
        assert_eq!(to_snake_case("IncludeItemTypes"), "include_item_types");
        // Digit-boundary params (the resolution filters).
        assert_eq!(to_snake_case("Is4K"), "is_4k");
        assert_eq!(to_snake_case("is4K"), "is_4k");
        assert_eq!(to_snake_case("Is3D"), "is_3d");
        assert_eq!(to_snake_case("isHd"), "is_hd");
        // Already snake / lowercase is a fixed point.
        assert_eq!(to_snake_case("season_id"), "season_id");
        assert_eq!(to_snake_case("limit"), "limit");

        assert_eq!(
            snake_case_query_keys("SeasonId=ABC123&StartIndex=20&userId=X"),
            "season_id=ABC123&start_index=20&user_id=X"
        );
        // Percent-encoded value untouched (no double-decode).
        assert_eq!(
            snake_case_query_keys("IncludeItemTypes=Movie%2CSeries"),
            "include_item_types=Movie%2CSeries"
        );
        assert_eq!(snake_case_query_keys(""), "");
        // Repeated keys merge into one comma-joined value (B20: jellyfin-web
        // sends fields=A&fields=B; a duplicate scalar key 400'd search).
        assert_eq!(
            snake_case_query_keys("fields=PrimaryImageAspectRatio&fields=CanDelete&searchTerm=x"),
            "fields=PrimaryImageAspectRatio,CanDelete&search_term=x"
        );
    }
}
