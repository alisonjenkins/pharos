//! `/admin/series-identity/*` — spec 033 §7 (V896), the Manage → Show
//! identity page: what needs a look (split folders, flagged matches, the
//! hand fixes made) and the hand fixes themselves. REST behind
//! `require_admin`, like every other Manage tab.

use std::collections::BTreeMap;

use actix_web::{error, web, HttpResponse, Responder};
use pharos_core::{SeriesIdentityStore, SeriesMetadataStore};
use serde::{Deserialize, Serialize};

use super::admin::require_admin;
use crate::api::jellyfin::auth_extractor::AuthUser;
use crate::online_enrich::OnlineEnricher;
use crate::series_hand_fix::{HandFixDone, HandFixError, HandFixer, MoveTarget};
use crate::state::AppState;
use crate::tmdb::{TmdbClient, TmdbEnricher};
use crate::tvdb::{ReqwestTransport, TvdbClient, TvdbEnricher};

pub(super) fn register(cfg: &mut web::ServiceConfig) {
    cfg.route("/admin/series-identity", web::get().to(overview))
        .route("/admin/series-identity/search", web::get().to(search))
        .route("/admin/series-identity/pick", web::post().to(pick))
        .route("/admin/series-identity/confirm", web::post().to(confirm))
        .route("/admin/series-identity/move", web::post().to(move_files))
        .route(
            "/admin/series-identity/keep-together",
            web::post().to(keep_together),
        )
        .route(
            "/admin/series-identity/overrides/{id}",
            web::delete().to(remove),
        );
}

#[derive(Debug, Serialize, PartialEq)]
#[serde(rename_all = "PascalCase")]
struct OverviewDto {
    splits: Vec<SplitDto>,
    flagged: Vec<FlaggedDto>,
    overrides: Vec<OverrideDto>,
}

/// A folder whose episodes sit under more than one series key.
#[derive(Debug, Serialize, PartialEq)]
#[serde(rename_all = "PascalCase")]
struct SplitDto {
    folder: String,
    groups: Vec<GroupDto>,
}

#[derive(Debug, Serialize, PartialEq)]
#[serde(rename_all = "PascalCase")]
struct GroupDto {
    series_key: String,
    year: Option<u32>,
    paths: Vec<String>,
}

/// A show match the matcher flagged as a close call (V891).
#[derive(Debug, Serialize, PartialEq)]
#[serde(rename_all = "PascalCase")]
struct FlaggedDto {
    series_key: String,
    name: String,
    provider: Option<String>,
    external_id: Option<String>,
    premiere_date: Option<i64>,
    runner_up_id: Option<String>,
    runner_up_score: Option<f32>,
}

#[derive(Debug, Serialize, PartialEq)]
#[serde(rename_all = "PascalCase")]
struct OverrideDto {
    id: i64,
    folder: String,
    path: Option<String>,
    target_key: String,
    target_name: Option<String>,
    target_year: Option<u32>,
    source: &'static str,
    keeps_together: bool,
    created_by: Option<String>,
    created_at: i64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
struct DoneDto {
    moved: Vec<String>,
    rebuilt: usize,
}

impl From<HandFixDone> for DoneDto {
    fn from(d: HandFixDone) -> Self {
        Self {
            moved: d.moved,
            rebuilt: d.rebuilt,
        }
    }
}

fn internal(context: &str) -> impl Fn(pharos_core::DomainError) -> actix_web::Error + '_ {
    move |e| error::ErrorInternalServerError(format!("{context}: {e}"))
}

async fn overview(
    state: web::Data<AppState>,
    user: AuthUser,
) -> Result<impl Responder, actix_web::Error> {
    require_admin(&user)?;
    let rows = state
        .stores
        .series_identity_rows("")
        .await
        .map_err(internal("reading episode identities"))?;
    let mut folders: BTreeMap<String, BTreeMap<String, GroupDto>> = BTreeMap::new();
    for row in rows {
        let Some(folder) = row.series_folder else {
            continue;
        };
        let key = row.series_key.unwrap_or_else(|| folder.clone());
        folders
            .entry(folder)
            .or_default()
            .entry(key.clone())
            .or_insert_with(|| GroupDto {
                series_key: key,
                year: row.series_year,
                paths: Vec::new(),
            })
            .paths
            .push(row.path);
    }
    let splits = folders
        .into_iter()
        .filter(|(_, groups)| groups.len() > 1)
        .map(|(folder, groups)| SplitDto {
            folder,
            groups: groups
                .into_values()
                .map(|mut g| {
                    g.paths.sort();
                    g
                })
                .collect(),
        })
        .collect();
    let keys = state
        .stores
        .flagged_series_keys()
        .await
        .map_err(internal("reading flagged shows"))?;
    let mut stored = state
        .stores
        .series_metadata_by_keys(&keys)
        .await
        .map_err(internal("reading flagged shows"))?;
    let flagged = keys
        .iter()
        .filter_map(|k| stored.remove(k))
        .map(|m| FlaggedDto {
            series_key: m.series_key,
            name: m.series_name,
            provider: m.match_provider,
            external_id: m.match_external_id,
            premiere_date: m.premiere_date,
            runner_up_id: m.match_runner_up_id,
            runner_up_score: m.match_runner_up_score,
        })
        .collect();
    let overrides = state
        .stores
        .all_series_overrides()
        .await
        .map_err(internal("reading overrides"))?
        .into_iter()
        .map(|o| OverrideDto {
            keeps_together: o.keeps_together(),
            id: o.id,
            folder: o.series_folder,
            path: o.path,
            target_key: o.target_key,
            target_name: o.target_name,
            target_year: o.target_year,
            source: o.source.as_str(),
            created_by: o.created_by,
            created_at: o.created_at,
        })
        .collect();
    Ok(web::Json(OverviewDto {
        splits,
        flagged,
        overrides,
    }))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct SearchQuery {
    name: String,
    #[serde(default)]
    year: Option<u32>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
struct CandidateDto {
    provider: &'static str,
    id: String,
    name: String,
    year: Option<u32>,
}

/// The shows each configured provider has under a name, for a pick.
async fn search(
    state: web::Data<AppState>,
    user: AuthUser,
    query: web::Query<SearchQuery>,
) -> Result<impl Responder, actix_web::Error> {
    require_admin(&user)?;
    let (tmdb, tvdb) = providers(&state);
    let mut out = Vec::new();
    let kind = pharos_core::MediaKind::Episode;
    if let Some(tv) = &tvdb {
        out.extend(candidates(
            "tvdb",
            tv.search(kind, &query.name, query.year).await,
        ));
    }
    if let Some(tm) = &tmdb {
        out.extend(candidates(
            "tmdb",
            tm.search(kind, &query.name, query.year).await,
        ));
    }
    Ok(web::Json(out))
}

fn candidates(
    provider: &'static str,
    found: Vec<pharos_core::SearchCandidate>,
) -> impl Iterator<Item = CandidateDto> {
    found.into_iter().map(move |c| CandidateDto {
        provider,
        id: c.id,
        name: c.title,
        year: c.year,
    })
}

fn providers(state: &AppState) -> (Option<TmdbEnricher>, Option<TvdbEnricher<ReqwestTransport>>) {
    let key = |k: Option<&str>| k.filter(|k| !k.is_empty()).map(str::to_string);
    (
        key(state.tmdb_api_key.as_deref()).map(|k| TmdbEnricher(TmdbClient::new(k))),
        key(state.tvdb_api_key.as_deref()).map(|k| TvdbEnricher(TvdbClient::new(k))),
    )
}

/// One hand fix as a request asked for it.
enum Op {
    Pick {
        series_key: String,
        provider: String,
        id: String,
    },
    Confirm {
        series_key: String,
    },
    Move {
        folder: String,
        paths: Vec<String>,
        target: MoveTarget,
    },
    KeepTogether {
        folder: String,
    },
    Remove {
        id: i64,
    },
}

/// Run one hand fix with the server's providers, cache and repair mode, as
/// `user`, and answer with what it changed.
async fn fix(state: &AppState, user: &AuthUser, op: Op) -> Result<HttpResponse, actix_web::Error> {
    require_admin(user)?;
    let (tmdb, tvdb) = providers(state);
    let fixer = HandFixer {
        store: &state.stores,
        bg_io: &state.bg_io,
        cache: state.images.as_ref(),
        tmdb: tmdb.as_ref(),
        tvdb: tvdb.as_ref(),
        cfg: &state.metadata_config,
        mode: state.metadata_config.series_identity_repair,
        admin: user.0.name.as_str(),
        now: crate::metadata_backfill::now_secs(),
    };
    let done = match op {
        Op::Pick {
            series_key,
            provider,
            id,
        } => {
            let provider = provider.to_ascii_lowercase();
            fixer.pin(&series_key, Some((&provider, &id))).await
        }
        Op::Confirm { series_key } => fixer.pin(&series_key, None).await,
        Op::Move {
            folder,
            paths,
            target,
        } => fixer.move_files(&folder, &paths, target).await,
        Op::KeepTogether { folder } => fixer.keep_together(&folder).await,
        Op::Remove { id } => fixer.remove(id).await,
    };
    match done {
        Ok(done) => Ok(HttpResponse::Ok().json(DoneDto::from(done))),
        Err(e) => {
            tracing::warn!(admin = %user.0.name, error = %e, "series hand fix refused");
            Err(match e {
                HandFixError::Invalid(m) => error::ErrorBadRequest(m),
                HandFixError::NotFound(m) => error::ErrorNotFound(m),
                HandFixError::Unavailable(m) => error::ErrorBadGateway(m),
                HandFixError::Store(e) => error::ErrorInternalServerError(e.to_string()),
            })
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct PickBody {
    series_key: String,
    provider: String,
    id: String,
}

async fn pick(
    state: web::Data<AppState>,
    user: AuthUser,
    body: web::Json<PickBody>,
) -> Result<impl Responder, actix_web::Error> {
    let b = body.into_inner();
    let op = Op::Pick {
        series_key: b.series_key,
        provider: b.provider,
        id: b.id,
    };
    fix(&state, &user, op).await
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ConfirmBody {
    series_key: String,
}

async fn confirm(
    state: web::Data<AppState>,
    user: AuthUser,
    body: web::Json<ConfirmBody>,
) -> Result<impl Responder, actix_web::Error> {
    let series_key = body.into_inner().series_key;
    fix(&state, &user, Op::Confirm { series_key }).await
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct MoveBody {
    folder: String,
    paths: Vec<String>,
    #[serde(default)]
    target_key: Option<String>,
    #[serde(default)]
    target_name: Option<String>,
    #[serde(default)]
    target_year: Option<u32>,
}

async fn move_files(
    state: web::Data<AppState>,
    user: AuthUser,
    body: web::Json<MoveBody>,
) -> Result<impl Responder, actix_web::Error> {
    let b = body.into_inner();
    let op = Op::Move {
        folder: b.folder,
        paths: b.paths,
        target: MoveTarget {
            key: b.target_key,
            name: b.target_name,
            year: b.target_year,
        },
    };
    fix(&state, &user, op).await
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct FolderBody {
    folder: String,
}

async fn keep_together(
    state: web::Data<AppState>,
    user: AuthUser,
    body: web::Json<FolderBody>,
) -> Result<impl Responder, actix_web::Error> {
    let folder = body.into_inner().folder;
    fix(&state, &user, Op::KeepTogether { folder }).await
}

async fn remove(
    state: web::Data<AppState>,
    user: AuthUser,
    path: web::Path<i64>,
) -> Result<impl Responder, actix_web::Error> {
    let id = path.into_inner();
    fix(&state, &user, Op::Remove { id }).await
}
