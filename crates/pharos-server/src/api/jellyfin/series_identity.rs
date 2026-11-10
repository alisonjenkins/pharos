//! `/admin/series-identity/*` — spec 033 §7 (V896), the Manage → Show
//! identity page: what needs a look (split folders, flagged matches, the
//! hand fixes made) and the hand fixes themselves. REST behind
//! `require_admin`, like every other Manage tab.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use actix_web::{error, web, HttpResponse, Responder};
use pharos_core::{SeriesIdentityStore, SeriesKeyUpdate, SeriesMetadataStore};
use pharos_scanner::series_split as split;
use serde::{Deserialize, Serialize};

use super::admin::require_admin;
use crate::api::jellyfin::auth_extractor::AuthUser;
use crate::online_enrich::OnlineEnricher;
use crate::series_hand_fix::{rebuild_paced, series_name, HandFixError, HandFixer, MoveTarget};
use crate::state::AppState;
use crate::tmdb::{TmdbClient, TmdbEnricher};
use crate::tvdb::{ReqwestTransport, TvdbClient, TvdbEnricher};

pub(super) fn register(cfg: &mut web::ServiceConfig) {
    cfg.route("/admin/series-identity", web::get().to(overview))
        .route("/admin/series-identity/flagged", web::get().to(flagged_ids))
        .route("/admin/series-identity/show", web::get().to(show))
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
    /// `dry_run` or `apply`: under `dry_run` a pending split waits for the
    /// switch to apply, under `apply` for the next scan.
    repair_mode: &'static str,
    splits: Vec<SplitDto>,
    flagged: Vec<FlaggedDto>,
    overrides: Vec<OverrideDto>,
}

/// A folder whose episodes sit, or would sit once the scanner's plan is
/// written, under more than one series key.
#[derive(Debug, Serialize, PartialEq)]
#[serde(rename_all = "PascalCase")]
struct SplitDto {
    folder: String,
    /// [`APPLIED`] when the stored rows already match the plan, else
    /// [`PENDING`] with the rows still to change in `moves`.
    status: &'static str,
    /// The groups as the plan has them.
    groups: Vec<GroupDto>,
    moves: Vec<PendingMoveDto>,
}

const APPLIED: &str = "applied";
const PENDING: &str = "pending";

/// One stored row the scanner's plan would change.
#[derive(Debug, Serialize, PartialEq)]
#[serde(rename_all = "PascalCase")]
struct PendingMoveDto {
    path: String,
    from_key: String,
    from_year: Option<u32>,
    to_key: String,
    to_year: Option<u32>,
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
    /// The show's item id, for a link to its title page.
    series_id: String,
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

/// What a fix changed: the paths it moved, and how many episodes it marked
/// to be rebuilt after this answer.
#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
struct DoneDto {
    moved: Vec<String>,
    rebuilding: usize,
}

/// The flagged shows' stored matches, in the store's order.
async fn flagged(state: &AppState) -> Result<Vec<pharos_core::SeriesMetadata>, actix_web::Error> {
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
    Ok(keys.iter().filter_map(|k| stored.remove(k)).collect())
}

/// The Series item id of a show's identity key, as the item routes mint it
/// (a key that is only the show's name is a row with no folder).
fn series_id(series_key: &str, name: &str) -> String {
    let folder = (series_key != name).then_some(series_key);
    pharos_jellyfin_api::dto::series_id_for_key(folder, name)
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
struct FlaggedIdsDto {
    series_ids: Vec<String>,
}

/// The item ids of flagged shows, for the title page's "Check match" badge
/// (spec 033 §3); cheap, unlike the overview's whole-library plan.
async fn flagged_ids(
    state: web::Data<AppState>,
    user: AuthUser,
) -> Result<impl Responder, actix_web::Error> {
    require_admin(&user)?;
    let series_ids = flagged(&state)
        .await?
        .iter()
        .map(|m| series_id(&m.series_key, &m.series_name))
        .collect();
    Ok(web::Json(FlaggedIdsDto { series_ids }))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ShowQuery {
    series_id: String,
}

/// A show's stored match as the "This show" card states it.
#[derive(Debug, Serialize, PartialEq)]
#[serde(rename_all = "PascalCase")]
struct ShowMatchDto {
    provider: Option<String>,
    external_id: Option<String>,
    source: Option<String>,
    premiere_date: Option<i64>,
}

#[derive(Debug, Serialize, PartialEq)]
#[serde(rename_all = "PascalCase")]
struct ShowEpisodeDto {
    id: pharos_core::MediaId,
    path: String,
    season: Option<u32>,
    episode: Option<u32>,
    title: String,
}

#[derive(Debug, Serialize, PartialEq)]
#[serde(rename_all = "PascalCase")]
struct ShowDto {
    series_key: String,
    series_id: String,
    folder: String,
    name: String,
    /// `None` while the show has no stored record at all.
    current_match: Option<ShowMatchDto>,
    episodes: Vec<ShowEpisodeDto>,
}

/// One show by its item id, flagged or not: the key it is stored under, its
/// match and its episode files, for the Manage tab opened from a title page.
/// The id is a hash of the key, so it is found by hashing each stored key.
async fn show(
    state: web::Data<AppState>,
    user: AuthUser,
    query: web::Query<ShowQuery>,
) -> Result<impl Responder, actix_web::Error> {
    require_admin(&user)?;
    let want = super::items::canonical_wire_id(&query.series_id).into_owned();
    let rows = state
        .stores
        .series_identity_rows("")
        .await
        .map_err(internal("reading episode identities"))?;
    let found = rows.iter().find_map(|r| {
        let folder = r.series_folder.as_deref()?;
        let key = r.series_key.as_deref().unwrap_or(folder);
        (pharos_jellyfin_api::dto::series_id_for_key(Some(key), "") == want)
            .then(|| (key.to_string(), folder.to_string()))
    });
    let Some((series_key, folder)) = found else {
        tracing::warn!(
            admin = %user.0.name,
            series_id = %want,
            rows = rows.len(),
            "series identity: no stored show has this item id"
        );
        return Err(error::ErrorNotFound(format!(
            "no show has item id {want:?}"
        )));
    };
    let metadata = state
        .stores
        .series_metadata_by_keys(std::slice::from_ref(&series_key))
        .await
        .map_err(internal("reading the show's match"))?
        .remove(&series_key);
    let mut rows = state
        .stores
        .series_episodes(&series_key)
        .await
        .map_err(internal("reading the show's episodes"))?;
    rows.sort_by(|a, b| (a.season, a.episode, &a.path).cmp(&(b.season, b.episode, &b.path)));
    let episodes = rows
        .into_iter()
        .map(|e| ShowEpisodeDto {
            id: e.id,
            path: e.path,
            season: e.season,
            episode: e.episode,
            title: e.title,
        })
        .collect();
    let name = metadata
        .as_ref()
        .map_or_else(|| series_name(&series_key), |m| m.series_name.clone());
    Ok(web::Json(ShowDto {
        series_id: want,
        current_match: metadata.map(|m| ShowMatchDto {
            provider: m.match_provider,
            external_id: m.match_external_id,
            source: m.match_source,
            premiere_date: m.premiere_date,
        }),
        series_key,
        folder,
        name,
        episodes,
    }))
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
    let splits = splits(&state.stores, rows, crate::metadata_backfill::now_secs())
        .await
        .map_err(internal("planning folder splits"))?;
    let flagged = flagged(&state)
        .await?
        .into_iter()
        .map(|m| FlaggedDto {
            series_id: series_id(&m.series_key, &m.series_name),
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
        repair_mode: state.metadata_config.series_identity_repair.label(),
        splits,
        flagged,
        overrides,
    }))
}

/// Plan every stored row as a scan would and list the folders that split,
/// applied or still pending (V896): under a dry run the plan is written only
/// for admin-placed rows, so the page is where the rest shows.
async fn splits<S: SeriesIdentityStore>(
    store: &S,
    rows: Vec<pharos_core::SeriesIdentityRow>,
    now: i64,
) -> pharos_core::DomainResult<Vec<SplitDto>> {
    let paths: Vec<PathBuf> = rows.iter().map(|r| PathBuf::from(&r.path)).collect();
    let walked = split::folders_of(&paths);
    let ctx = split::context(store, &walked).await?;
    let plan = split::plan(&walked, &ctx, now);
    let mut pending: HashMap<String, SeriesKeyUpdate> = split::diff(&plan, &rows)
        .into_iter()
        .map(|u| (u.path.clone(), u))
        .collect();
    let mut folders: BTreeMap<String, (BTreeMap<String, GroupDto>, Vec<PendingMoveDto>)> =
        BTreeMap::new();
    for row in rows {
        let Some(folder) = row.series_folder else {
            continue;
        };
        let from_key = row.series_key.unwrap_or_else(|| folder.clone());
        let (key, year) = match pending.remove(&row.path) {
            Some(u) => (
                u.series_key.unwrap_or_else(|| folder.clone()),
                u.series_year,
            ),
            None => (from_key.clone(), row.series_year),
        };
        let (groups, moves) = folders.entry(folder).or_default();
        // A row whose only change is its stated year stays in its group (B981).
        if key != from_key {
            moves.push(PendingMoveDto {
                path: row.path.clone(),
                from_key,
                from_year: row.series_year,
                to_key: key.clone(),
                to_year: year,
            });
        }
        groups
            .entry(key.clone())
            .or_insert_with(|| GroupDto {
                series_key: key,
                year,
                paths: Vec::new(),
            })
            .paths
            .push(row.path);
    }
    Ok(folders
        .into_iter()
        .filter(|(_, (groups, moves))| groups.len() > 1 || !moves.is_empty())
        .map(|(folder, (groups, mut moves))| {
            moves.sort_by(|a, b| a.path.cmp(&b.path));
            SplitDto {
                folder,
                status: if moves.is_empty() { APPLIED } else { PENDING },
                groups: groups
                    .into_values()
                    .map(|mut g| {
                        g.paths.sort();
                        g
                    })
                    .collect(),
                moves,
            }
        })
        .collect())
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
async fn fix(
    state: &web::Data<AppState>,
    user: &AuthUser,
    op: Op,
) -> Result<HttpResponse, actix_web::Error> {
    require_admin(user)?;
    let (tmdb, tvdb) = providers(state);
    let fixer = HandFixer {
        store: &state.stores,
        bg_io: &state.bg_io,
        cache: state.images.as_ref(),
        tmdb: tmdb.as_ref(),
        tvdb: tvdb.as_ref(),
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
        Ok(done) => {
            let answer = DoneDto {
                moved: done.moved,
                rebuilding: done.rebuild.len(),
            };
            if !done.rebuild.is_empty() {
                rebuild_after(state.clone(), done.rebuild, user.0.name.clone());
            }
            Ok(HttpResponse::Ok().json(answer))
        }
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

/// Rebuild a fix's episodes on the request's worker after it answered.
/// Without an image cache they stay marked for the background pass.
fn rebuild_after(state: web::Data<AppState>, ids: Vec<pharos_core::MediaId>, admin: String) {
    actix_web::rt::spawn(async move {
        let Some(cache) = state.images.as_ref() else {
            tracing::warn!(
                admin = %admin,
                episodes = ids.len(),
                "series hand fix: no image cache, episodes left marked"
            );
            return;
        };
        let (tmdb, tvdb) = providers(&state);
        rebuild_paced(
            &state.stores,
            &state.bg_io,
            cache,
            tmdb.as_ref(),
            tvdb.as_ref(),
            &state.metadata_config,
            &ids,
            &admin,
        )
        .await;
    });
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
