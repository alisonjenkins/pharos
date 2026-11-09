//! `/admin/items/{id}/metadata` — read and edit an item's or a show's
//! descriptive metadata (spec 015 US3, ADR-0097). Admin-only, like the other
//! Manage tabs. `POST /Items/{id}` (Jellyfin's metadata editor) writes
//! through the same service.
//!
//! `{id}` is a real item's id (numeric or wire form) or a show's synthesised
//! id. A `POST` body is `{"Fields": {...}, "Unlock": [...]}`: a field named in
//! `Fields` is written and locked (`null` clears it), a field in `Unlock` is
//! handed back to the scanner and online providers, and a field named in
//! neither is untouched. The answer is the same view a `GET` returns.

use crate::services::metadata_edit::{self as service, EditError};
use crate::{api::jellyfin::auth_extractor::AuthUser, state::AppState};
use actix_web::{error, web, HttpResponse, Responder};
use pharos_core::{
    EditTarget, EditableField, LockedFields, MediaStore, MetadataEdit, MetadataEditStore,
    SeriesMetadataStore,
};
use serde_json::{Map, Value};

use super::admin::require_admin;

pub fn register(cfg: &mut web::ServiceConfig) {
    cfg.route("/admin/items/{id}/metadata", web::get().to(get_metadata))
        .route("/admin/items/{id}/metadata", web::post().to(post_metadata))
        .route("/items/{id}", web::post().to(jellyfin_update_item));
}

/// The fields as the editor shows them, keyed by [`EditableField::wire`].
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "PascalCase")]
struct MetadataViewDto {
    id: String,
    /// `item` or `series`.
    target: &'static str,
    /// The Jellyfin item type (`Movie`, `Episode`, `Series`, …).
    kind: &'static str,
    /// What the item is called now; a show's name is not editable here.
    name: String,
    fields: Map<String, Value>,
    editable: Vec<&'static str>,
    locked: Vec<&'static str>,
    /// The scores as stored, unrounded, which is what `GET /Items` sends
    /// and so what Jellyfin's editor posts back unchanged.
    #[serde(skip)]
    exact_scores: Map<String, Value>,
}

/// A refused edit, shown against its field (FR-003).
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "PascalCase")]
struct EditRefusalDto {
    field: String,
    reason: String,
}

fn refusal(field: &str, reason: impl Into<String>) -> actix_web::Error {
    let body = EditRefusalDto {
        field: field.to_owned(),
        reason: reason.into(),
    };
    error::InternalError::from_response(body.reason.clone(), HttpResponse::BadRequest().json(body))
        .into()
}

/// A 500 that names no backend detail; the error itself goes to the log.
fn store_failure(what: &'static str, e: &dyn std::fmt::Display) -> actix_web::Error {
    tracing::error!(error = %e, "metadata edit: {what}");
    error::ErrorInternalServerError("metadata store unavailable")
}

fn date_value(unix: Option<i64>) -> Value {
    unix.map_or(Value::Null, |t| {
        let iso = pharos_jellyfin_api::dto::format_iso8601(t);
        Value::String(iso.get(..10).unwrap_or(&iso).to_owned())
    })
}

/// A stored REAL score at one decimal, as it was entered: 7.3, not
/// 7.300000190734863.
fn score(v: Option<f32>) -> Value {
    v.and_then(|r| serde_json::Number::from_f64((f64::from(r) * 10.0).round() / 10.0))
        .map_or(Value::Null, Value::Number)
}

/// A stored REAL score as `GET /Items` carries it.
fn exact_score(v: Option<f32>) -> Value {
    v.and_then(|r| serde_json::Number::from_f64(f64::from(r)))
        .map_or(Value::Null, Value::Number)
}

fn opt<T: Into<Value>>(v: Option<T>) -> Value {
    v.map_or(Value::Null, Into::into)
}

fn wire_list(fields: impl Iterator<Item = EditableField>) -> Vec<&'static str> {
    fields.map(EditableField::wire).collect()
}

async fn view(
    state: &AppState,
    raw: &str,
    target: &EditTarget,
) -> Result<MetadataViewDto, actix_web::Error> {
    use EditableField as F;
    let mut fields = Map::new();
    let mut put = |f: F, v: Value| {
        fields.insert(f.wire().to_owned(), v);
    };
    match target {
        EditTarget::Item(id) => {
            let item = state.stores.get(*id).await.map_err(|e| match e {
                pharos_core::DomainError::NotFound(_) => error::ErrorNotFound("not found"),
                other => store_failure("could not read the item", &other),
            })?;
            let sort = state
                .stores
                .stored_sort_name(*id)
                .await
                .map_err(|e| store_failure("could not read the stored metadata", &e))?;
            let md = &item.metadata;
            put(F::Title, item.title.clone().into());
            put(F::SortName, opt(sort));
            put(F::Overview, opt(md.overview.clone()));
            put(F::Tagline, opt(md.tagline.clone()));
            put(F::ProductionYear, opt(md.production_year));
            put(F::PremiereDate, date_value(md.premiere_date));
            put(F::OfficialRating, opt(md.official_rating.clone()));
            put(F::CommunityRating, score(md.community_rating));
            put(F::CriticRating, score(md.critic_rating));
            let exact_scores = Map::from_iter([
                (
                    F::CommunityRating.wire().to_owned(),
                    exact_score(md.community_rating),
                ),
                // What `GET /Items` shows, which may be the Tomatometer.
                (
                    F::CriticRating.wire().to_owned(),
                    exact_score(pharos_jellyfin_api::dto::shown_critic_rating(&item)),
                ),
            ]);
            Ok(MetadataViewDto {
                id: raw.to_owned(),
                target: "item",
                kind: item.kind.base_item_kind(),
                name: item.title.clone(),
                fields,
                editable: wire_list(F::ALL.into_iter().filter(|f| f.on_item())),
                locked: wire_list(item.locked.iter()),
                exact_scores,
            })
        }
        EditTarget::Series { key, name } => {
            let meta = state
                .stores
                .series_metadata_by_keys(std::slice::from_ref(key))
                .await
                .map_err(|e| store_failure("could not read the stored metadata", &e))?
                .remove(key)
                .unwrap_or_default();
            put(F::Overview, opt(meta.overview.clone()));
            put(F::PremiereDate, date_value(meta.premiere_date));
            put(F::OfficialRating, opt(meta.official_rating.clone()));
            put(F::CommunityRating, score(meta.community_rating));
            put(F::Genres, meta.genres.clone().into());
            Ok(MetadataViewDto {
                id: raw.to_owned(),
                target: "series",
                kind: "Series",
                name: name.clone(),
                fields,
                editable: wire_list(F::ALL.into_iter().filter(|f| f.on_series())),
                locked: wire_list(meta.locked.iter()),
                exact_scores: Map::from_iter([(
                    F::CommunityRating.wire().to_owned(),
                    exact_score(meta.community_rating),
                )]),
            })
        }
    }
}

async fn target_for(state: &AppState, raw: &str) -> Result<EditTarget, actix_web::Error> {
    service::resolve_target(state, raw)
        .await
        .map_err(|e| store_failure("could not resolve the id", &e))?
        .ok_or_else(|| error::ErrorNotFound("no item or show has that id"))
}

async fn get_metadata(
    state: web::Data<AppState>,
    user: AuthUser,
    path: web::Path<String>,
) -> Result<impl Responder, actix_web::Error> {
    require_admin(&user)?;
    let raw = path.into_inner();
    let target = target_for(&state, &raw).await?;
    Ok(HttpResponse::Ok().json(view(&state, &raw, &target).await?))
}

fn text(field: EditableField, v: &Value) -> Result<Option<String>, actix_web::Error> {
    match v {
        Value::Null => Ok(None),
        Value::String(s) => Ok(Some(s.clone())),
        other => Err(refusal(
            field.wire(),
            format!("{} must be text or null, got {other}", field.wire()),
        )),
    }
}

fn number(field: EditableField, v: &Value) -> Result<Option<f64>, actix_web::Error> {
    match v {
        Value::Null => Ok(None),
        Value::Number(n) => n.as_f64().map(Some).ok_or_else(|| {
            refusal(
                field.wire(),
                format!("{} {n} is not a number", field.wire()),
            )
        }),
        other => Err(refusal(
            field.wire(),
            format!("{} must be a number or null, got {other}", field.wire()),
        )),
    }
}

/// Parses the body's `Fields` and `Unlock` into an edit. Type errors name
/// the field and the value sent; limits are checked by the service.
fn parse_edit(body: &Value) -> Result<MetadataEdit, actix_web::Error> {
    use EditableField as F;
    let mut edit = MetadataEdit::default();
    let fields = match body.get("Fields") {
        None | Some(Value::Null) => Map::new(),
        Some(Value::Object(m)) => m.clone(),
        Some(other) => {
            return Err(refusal(
                "Fields",
                format!("Fields must be an object, got {other}"),
            ))
        }
    };
    for (key, v) in &fields {
        let f =
            F::parse(key).ok_or_else(|| refusal(key, format!("{key} is not an editable field")))?;
        match f {
            F::Title => {
                edit.title = Some(text(f, v)?.unwrap_or_default());
            }
            F::SortName => edit.sort_name = Some(text(f, v)?),
            F::Overview => edit.overview = Some(text(f, v)?),
            F::Tagline => edit.tagline = Some(text(f, v)?),
            F::OfficialRating => edit.official_rating = Some(text(f, v)?),
            F::ProductionYear => {
                let y = match number(f, v)? {
                    None => None,
                    Some(n) if n.fract() == 0.0 && (0.0..=f64::from(u32::MAX)).contains(&n) => {
                        // Checked whole and in range just above.
                        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                        Some(n as u32)
                    }
                    Some(n) => {
                        return Err(refusal(
                            f.wire(),
                            format!("ProductionYear {n} is not a year"),
                        ))
                    }
                };
                edit.production_year = Some(y);
            }
            F::PremiereDate => {
                let d = match text(f, v)? {
                    None => None,
                    Some(s) if s.trim().is_empty() => None,
                    Some(s) => Some(
                        pharos_scanner::metadata::nfo::parse_date_unix(&s).ok_or_else(|| {
                            refusal(
                                f.wire(),
                                format!("PremiereDate {s:?} is not a YYYY-MM-DD date"),
                            )
                        })?,
                    ),
                };
                edit.premiere_date = Some(d);
            }
            #[allow(clippy::cast_possible_truncation)]
            F::CommunityRating => edit.community_rating = Some(number(f, v)?.map(|n| n as f32)),
            #[allow(clippy::cast_possible_truncation)]
            F::CriticRating => edit.critic_rating = Some(number(f, v)?.map(|n| n as f32)),
            F::Genres => {
                let list = match v {
                    Value::Null => Vec::new(),
                    Value::Array(items) => items
                        .iter()
                        .map(|g| {
                            g.as_str().map(str::to_owned).ok_or_else(|| {
                                refusal(f.wire(), format!("Genres entries must be text, got {g}"))
                            })
                        })
                        .collect::<Result<_, _>>()?,
                    other => {
                        return Err(refusal(
                            f.wire(),
                            format!("Genres must be a list, got {other}"),
                        ))
                    }
                };
                edit.genres = Some(list);
            }
        }
    }
    let mut unlock = LockedFields::NONE;
    match body.get("Unlock") {
        None | Some(Value::Null) => {}
        Some(Value::Array(names)) => {
            for n in names {
                let Some(name) = n.as_str() else {
                    return Err(refusal(
                        "Unlock",
                        format!("Unlock entries must be field names, got {n}"),
                    ));
                };
                let f = F::parse(name)
                    .ok_or_else(|| refusal(name, format!("{n} is not an editable field")))?;
                unlock.insert(f);
            }
        }
        Some(other) => {
            return Err(refusal(
                "Unlock",
                format!("Unlock must be a list, got {other}"),
            ))
        }
    }
    edit.unlock = unlock;
    Ok(edit)
}

/// The HTTP answer for a service refusal.
pub(super) fn edit_error(e: EditError) -> actix_web::Error {
    match e {
        EditError::Rejected(r) => refusal(r.field(), r.to_string()),
        EditError::NotFound(_) => error::ErrorNotFound("not found"),
        // `service::apply` has already logged the store's error.
        EditError::Store(_) => error::ErrorInternalServerError("metadata store unavailable"),
    }
}

async fn post_metadata(
    state: web::Data<AppState>,
    user: AuthUser,
    path: web::Path<String>,
    body: web::Json<Value>,
) -> Result<impl Responder, actix_web::Error> {
    require_admin(&user)?;
    let raw = path.into_inner();
    let target = target_for(&state, &raw).await?;
    let edit =
        parse_edit(&body).inspect_err(|e| service::record_unparsed(&user.0.name, &target, e))?;
    service::apply(&state, &user.0.name, &target, &edit)
        .await
        .map_err(edit_error)?;
    Ok(HttpResponse::Ok().json(view(&state, &raw, &target).await?))
}

/// Jellyfin `BaseItemDto` keys for each field, and the `LockedFields`
/// (`MetadataField`) name that locks it where Jellyfin has one.
const JELLYFIN_KEYS: [(EditableField, &str, Option<&str>); 10] = [
    (EditableField::Title, "Name", Some("Name")),
    (EditableField::SortName, "ForcedSortName", None),
    (EditableField::Overview, "Overview", Some("Overview")),
    (EditableField::Tagline, "Taglines", None),
    (EditableField::ProductionYear, "ProductionYear", None),
    (EditableField::PremiereDate, "PremiereDate", None),
    (
        EditableField::OfficialRating,
        "OfficialRating",
        Some("OfficialRating"),
    ),
    (EditableField::CommunityRating, "CommunityRating", None),
    (EditableField::CriticRating, "CriticRating", None),
    (EditableField::Genres, "Genres", Some("Genres")),
];

/// A Jellyfin value in the admin API's shape: the first tagline, a
/// premiere date's day.
fn from_jellyfin(f: EditableField, v: &Value) -> Value {
    use EditableField as F;
    let v = match (f, v) {
        (F::Tagline, Value::Array(list)) => list.first().cloned().unwrap_or(Value::Null),
        _ => v.clone(),
    };
    match (f, v) {
        (F::PremiereDate, Value::String(s)) => Value::String(s.get(..10).unwrap_or(&s).to_owned()),
        // jellyfin-web posts its number boxes as text.
        (F::ProductionYear | F::CommunityRating | F::CriticRating, Value::String(s)) => {
            match s.trim() {
                "" => Value::Null,
                t => t
                    .parse::<f64>()
                    .ok()
                    .and_then(serde_json::Number::from_f64)
                    .map_or(Value::String(s.clone()), Value::Number),
            }
        }
        (_, Value::String(s)) if s.trim().is_empty() => Value::Null,
        (_, v) => v,
    }
}

/// Numbers match when they are the same `f32`, the type every stored
/// score and year has: `GET /Items` writes a score's shortest f32 form, so
/// an unchanged `7.256` reads back as the stored value, and `7.2561` (a
/// different f32) is an edit.
#[allow(clippy::cast_possible_truncation, clippy::float_cmp)]
fn same(a: &Value, b: &Value) -> bool {
    match (a.as_f64(), b.as_f64()) {
        (Some(x), Some(y)) => x as f32 == y as f32,
        _ => a == b,
    }
}

/// Jellyfin's editor posts the whole item, so only a field whose value
/// changed, or that the body locks, becomes part of the edit. It never
/// unlocks: the item answers carry no `LockedFields`, so an empty list in
/// the body says nothing about what the admin wants.
fn jellyfin_fields(
    body: &Value,
    current: &Map<String, Value>,
    editable: &[&str],
) -> Map<String, Value> {
    let locks: Vec<&str> = body
        .get("LockedFields")
        .and_then(Value::as_array)
        .map(|l| l.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let mut out = Map::new();
    for (f, key, lock_name) in JELLYFIN_KEYS {
        if !editable.contains(&f.wire()) {
            continue;
        }
        let Some(raw) = body.get(key) else { continue };
        // pharos sends no ForcedSortName, so an empty one is the editor's
        // empty box, not a request to clear the sort title.
        if f == EditableField::SortName && raw.as_str().map_or(true, |t| t.trim().is_empty()) {
            continue;
        }
        let v = from_jellyfin(f, raw);
        let locked = lock_name.is_some_and(|n| locks.iter().any(|l| l.eq_ignore_ascii_case(n)));
        let changed = current.get(f.wire()).map_or(true, |c| !same(c, &v));
        if changed || locked {
            out.insert(f.wire().to_owned(), v);
        }
    }
    out
}

/// `POST /Items/{id}` — jellyfin-web's metadata editor save. Answers 204
/// like Jellyfin.
async fn jellyfin_update_item(
    state: web::Data<AppState>,
    user: AuthUser,
    path: web::Path<String>,
    body: web::Json<Value>,
) -> Result<impl Responder, actix_web::Error> {
    require_admin(&user)?;
    let raw = path.into_inner();
    let target = target_for(&state, &raw).await?;
    let current = view(&state, &raw, &target).await?;
    // Compare against what `GET /Items` sent, not the view's rounded scores.
    let mut stored = current.fields;
    stored.extend(current.exact_scores);
    let fields = jellyfin_fields(&body, &stored, &current.editable);
    if fields.is_empty() {
        tracing::info!(admin = %user.0.name, target = ?target, "metadata edit: the Jellyfin save changed nothing");
        return Ok(HttpResponse::NoContent().finish());
    }
    let edit = parse_edit(&serde_json::json!({ "Fields": fields }))
        .inspect_err(|e| service::record_unparsed(&user.0.name, &target, e))?;
    service::apply(&state, &user.0.name, &target, &edit)
        .await
        .map_err(edit_error)?;
    Ok(HttpResponse::NoContent().finish())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use serde_json::json;

    #[test]
    fn a_body_sets_clears_and_unlocks() {
        let edit = parse_edit(&json!({
            "Fields": {"Title": "Alien", "Tagline": null, "ProductionYear": 1979,
                       "PremiereDate": "1979-05-25", "CommunityRating": 8.5},
            "Unlock": ["Overview"]
        }))
        .unwrap();
        assert_eq!(edit.title.as_deref(), Some("Alien"));
        assert_eq!(edit.tagline, Some(None));
        assert_eq!(edit.production_year, Some(Some(1979)));
        assert_eq!(edit.premiere_date, Some(Some(296_438_400)));
        assert_eq!(edit.community_rating, Some(Some(8.5)));
        assert_eq!(edit.overview, None, "a field not named is untouched");
        assert!(edit.unlock.contains(EditableField::Overview));
    }

    #[test]
    fn a_jellyfin_save_edits_only_what_changed_or_is_locked() {
        let current = json!({"Title": "Alien", "Overview": "Old", "Tagline": "In space",
                             "ProductionYear": 1979, "OfficialRating": "R",
                             "CommunityRating": 8.5, "PremiereDate": "1979-05-25"});
        let current = current.as_object().unwrap();
        let editable = [
            "Title",
            "Overview",
            "Tagline",
            "ProductionYear",
            "OfficialRating",
            "CommunityRating",
            "PremiereDate",
        ];
        let body = json!({"Name": "Alien", "Overview": "New", "Taglines": ["In space"],
                          "ProductionYear": 1979, "OfficialRating": "R", "CommunityRating": 8.5,
                          "PremiereDate": "1979-05-25T00:00:00.0000000Z",
                          "Genres": ["Horror"], "LockedFields": ["OfficialRating"]});
        let out = jellyfin_fields(&body, current, &editable);
        let mut keys: Vec<&str> = out.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            ["OfficialRating", "Overview"],
            "Genres is not editable here"
        );
    }

    #[test]
    fn jellyfin_webs_text_boxes_and_empty_values_are_not_changes() {
        let current = json!({"Title": "Alien", "SortName": "alien", "Tagline": null,
                             "ProductionYear": 1979, "CommunityRating": null, "CriticRating": 97});
        let current = current.as_object().unwrap();
        let editable = [
            "Title",
            "SortName",
            "Tagline",
            "ProductionYear",
            "CommunityRating",
            "CriticRating",
        ];
        let body = json!({"Name": "Alien", "ForcedSortName": "", "Taglines": [""],
                          "ProductionYear": "1979", "CommunityRating": "", "CriticRating": "97"});
        assert!(jellyfin_fields(&body, current, &editable).is_empty());
        let body = json!({"ProductionYear": "1980", "ForcedSortName": "Alien 1"});
        let out = jellyfin_fields(&body, current, &editable);
        assert_eq!(out["ProductionYear"], json!(1980.0));
        assert_eq!(out["SortName"], "Alien 1");
        assert_eq!(
            parse_edit(&json!({"Fields": out})).unwrap().production_year,
            Some(Some(1980))
        );
    }

    #[test]
    fn an_unchanged_precise_score_is_not_an_edit() {
        let stored = 7.256_f32;
        let current = json!({"CommunityRating": exact_score(Some(stored))});
        let current = current.as_object().unwrap();
        let editable = ["CommunityRating"];
        // `GET /Items` serialises the f32, which reads back as 7.256.
        let sent = serde_json::to_value(stored).unwrap();
        let body = json!({ "CommunityRating": sent });
        assert!(jellyfin_fields(&body, current, &editable).is_empty());
        let body = json!({"CommunityRating": 7.26});
        assert_eq!(
            jellyfin_fields(&body, current, &editable)["CommunityRating"],
            json!(7.26),
            "an edit inside the same tenth is still an edit"
        );
        let body = json!({"CommunityRating": 7.2561});
        assert!(
            jellyfin_fields(&body, current, &editable).contains_key("CommunityRating"),
            "any change to the stored f32 is an edit"
        );
        assert_eq!(score(Some(stored)), json!(7.3), "the view still rounds");
    }

    #[test]
    fn type_errors_name_the_field_and_value() {
        for (body, needle) in [
            (json!({"Fields": {"Bogus": 1}}), "Bogus"),
            (json!({"Fields": {"ProductionYear": "soon"}}), "\"soon\""),
            (json!({"Fields": {"ProductionYear": 1979.5}}), "1979.5"),
            (json!({"Fields": {"PremiereDate": "25/05"}}), "25/05"),
            (json!({"Fields": {"Genres": [1]}}), "1"),
            (json!({"Unlock": ["Nope"]}), "Nope"),
            (
                json!({"Unlock": [1]}),
                "Unlock entries must be field names, got 1",
            ),
        ] {
            let e = parse_edit(&body).unwrap_err();
            assert!(e.to_string().contains(needle), "{body}: {e}");
        }
    }
}
