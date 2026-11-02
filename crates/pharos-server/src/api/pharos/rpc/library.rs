//! `Library`: what a signed-in user may browse. Each method calls one
//! `services::library` function and encodes its answer (US4).

use std::rc::Rc;

use pharos_api::pharos_capnp::{
    item_id, item_summary, items_result, library as wire, Kind, LibraryKind as WireKind,
};
use pharos_api::{server, Denial};
use pharos_core::{LibraryKind, MediaItem, MediaKind, User, UserItemData};
use pharos_jellyfin_api::dto::{BaseItemDto, WireId};

use super::bootstrap::token_denial;
use super::session::Session;
use crate::services::auth::{user_for_token, TokenRefusal};
use crate::services::library::{self, KindFilter, LibraryView, ListError, ListPage};

pub(super) struct Library {
    session: Rc<Session>,
}

impl Library {
    pub(super) fn new(session: Rc<Session>) -> Self {
        Self { session }
    }

    /// The session's user as the store has them now, so a policy change or
    /// a disabled account applies to the next call, not the next sign-in.
    async fn current_user(&self) -> Result<User, Denial> {
        if let Some(denial) = self.session.ended() {
            return Err(denial);
        }
        user_for_token(&self.session.state.stores, &self.session.token)
            .await
            .map_err(|refusal| {
                if let TokenRefusal::Unavailable(detail) = &refusal {
                    tracing::warn!(error = %detail, "native library: token store unavailable");
                }
                token_denial(&refusal)
            })
    }
}

impl server::library::Handler for Library {
    async fn views(
        self: Rc<Self>,
        _params: wire::ViewsParams,
        results: &mut wire::ViewsResults,
    ) -> Result<(), capnp::Error> {
        let user = match self.current_user().await {
            Ok(user) => user,
            Err(denial) => {
                denial.write(results.get().init_result().init_error());
                return Ok(());
            }
        };
        let views = library::views(&self.session.state, &user.policy).await;
        let views: Vec<(WireId, LibraryView)> = views
            .into_iter()
            .filter_map(|view| match WireId::parse(&view.id) {
                Some(id) => Some((id, view)),
                None => {
                    tracing::warn!(id = %view.id, name = %view.name, "native library: view id is not a wire id; skipped");
                    None
                }
            })
            .collect();
        let count = u32::try_from(views.len()).unwrap_or(u32::MAX);
        let mut page = results.get().init_result().init_page();
        page.set_total(count);
        page.set_start(0);
        let mut items = page.init_items(count);
        for (i, (id, view)) in (0..count).zip(&views) {
            write_view(items.reborrow().get(i), *id, view);
        }
        Ok(())
    }

    async fn latest(
        self: Rc<Self>,
        params: wire::LatestParams,
        results: &mut wire::LatestResults,
    ) -> Result<(), capnp::Error> {
        let params = params.get()?;
        let parent = if params.has_parent_id() {
            Some(read_item_id(params.get_parent_id()?)?.to_string())
        } else {
            None
        };
        // 0 is the field's default: the caller asked for no particular limit.
        let limit = Some(params.get_limit()).filter(|&n| n > 0);
        let user = match self.current_user().await {
            Ok(user) => user,
            Err(denial) => {
                denial.write(results.get().init_result().init_error());
                return Ok(());
            }
        };
        let state = &self.session.state;
        let listed = library::latest(state, &user, parent.as_deref(), &KindFilter::Unset, limit)
            .await
            .map(|items| {
                let total = u32::try_from(items.len()).unwrap_or(u32::MAX);
                ListPage {
                    items,
                    total,
                    start: 0,
                }
            });
        write_listing(
            results.get().init_result(),
            listed,
            "latest",
            &state.server_id,
        );
        Ok(())
    }

    async fn resume(
        self: Rc<Self>,
        params: wire::ResumeParams,
        results: &mut wire::ResumeResults,
    ) -> Result<(), capnp::Error> {
        let params = params.get()?;
        let kinds = kind_filter(params.get_kinds()?);
        // 0 is the field's default: the caller asked for no particular limit.
        let limit = Some(params.get_limit()).filter(|&n| n > 0);
        let user = match self.current_user().await {
            Ok(user) => user,
            Err(denial) => {
                denial.write(results.get().init_result().init_error());
                return Ok(());
            }
        };
        let state = &self.session.state;
        let listed = library::resume(state, &user, &kinds, 0, limit).await;
        write_listing(
            results.get().init_result(),
            listed,
            "resume",
            &state.server_id,
        );
        Ok(())
    }
}

/// A listing's page, or `unavailable` with its cause logged.
fn write_listing(
    out: items_result::Builder<'_>,
    listed: Result<ListPage, ListError>,
    method: &'static str,
    server_id: &str,
) {
    let listed = match listed {
        Ok(listed) => listed,
        Err(e) => {
            let detail = match &e {
                ListError::Hidden(hidden) => hidden.label().to_owned(),
                ListError::Store(e) => e.to_string(),
            };
            tracing::warn!(method, error = %detail, "native library: listing unavailable");
            Denial::Unavailable {
                dependency: "media store",
            }
            .write(out.init_error());
            return;
        }
    };
    let count = u32::try_from(listed.items.len()).unwrap_or(u32::MAX);
    let mut page = out.init_page();
    page.set_total(listed.total);
    page.set_start(listed.start);
    let mut items = page.init_items(count);
    for (i, (item, user_data)) in (0..count).zip(&listed.items) {
        write_item(items.reborrow().get(i), item, *user_data, server_id);
    }
}

/// The stored kinds a `kinds` list names; an empty list names none, so
/// keeps every kind. Kinds pharos only synthesises (series, album…) are never
/// stored, so a list of only those matches nothing (B153).
fn kind_filter(kinds: capnp::enum_list::Reader<'_, Kind>) -> KindFilter {
    if kinds.is_empty() {
        return KindFilter::Unset;
    }
    let mut stored: Vec<MediaKind> = kinds
        .iter()
        .filter_map(|k| match k.ok()? {
            Kind::Movie => Some(MediaKind::Movie),
            Kind::Episode => Some(MediaKind::Episode),
            Kind::Audio => Some(MediaKind::Audio),
            Kind::Book => Some(MediaKind::Book),
            Kind::Series
            | Kind::Season
            | Kind::Album
            | Kind::Artist
            | Kind::Person
            | Kind::Collection
            | Kind::Playlist
            | Kind::Channel
            | Kind::Library => None,
        })
        .collect();
    stored.dedup();
    if stored.is_empty() {
        KindFilter::MatchesNothing
    } else {
        KindFilter::Kinds(stored)
    }
}

fn read_item_id(id: item_id::Reader<'_>) -> Result<WireId, capnp::Error> {
    Ok(match id.which()? {
        item_id::Real(id) => WireId::Real(id),
        item_id::Synth(id) => WireId::Synth(id),
    })
}

fn set_item_id(mut out: item_id::Builder<'_>, id: WireId) {
    match id {
        WireId::Real(id) => out.set_real(id),
        WireId::Synth(id) => out.set_synth(id),
    }
}

fn wire_kind(kind: MediaKind) -> Kind {
    match kind {
        MediaKind::Movie => Kind::Movie,
        MediaKind::Episode => Kind::Episode,
        MediaKind::Audio => Kind::Audio,
        MediaKind::Book => Kind::Book,
    }
}

/// One stored item as an `ItemSummary`. The display fields come from the
/// Jellyfin projection, so the two APIs cannot disagree on a series name,
/// year or index number.
fn write_item(
    mut out: item_summary::Builder<'_>,
    item: &MediaItem,
    user_data: UserItemData,
    server_id: &str,
) {
    let dto = BaseItemDto::from_domain_with_user_data(item, server_id, user_data);
    set_item_id(out.reborrow().init_id(), WireId::Real(item.id));
    out.set_kind(wire_kind(item.kind));
    out.set_name(dto.name.as_str());
    if let Some(year) = dto.production_year.and_then(|y| u16::try_from(y).ok()) {
        out.reborrow().init_year().set_some(year);
    }
    // A book's runtime is 0 by design (no time axis).
    out.set_runtime_ms(dto.run_time_ticks / 10_000);
    if let Some(tag) = dto.image_tags.get("Primary").and_then(|t| t.as_str()) {
        let mut image = out.reborrow().init_images(1).get(0);
        image.set_kind("Primary");
        image.set_tag(tag);
        image.set_url(format!("/Items/{}/Images/Primary?tag={tag}", dto.id).as_str());
    }
    let mut state = out.reborrow().init_user_state();
    state.set_played(user_data.played);
    state.set_favorite(user_data.is_favorite);
    state.set_position_ms(user_data.last_played_position_ticks / 10_000);
    state.set_play_count(user_data.play_count);
    if let Some(parent) = dto.parent_id.as_deref().and_then(WireId::parse) {
        set_item_id(out.reborrow().init_parent_id(), parent);
    }
    if let Some(series) = &dto.series_name {
        out.set_series_name(series.as_str());
    }
    if let Some(n) = dto.index_number {
        out.reborrow().init_index_number().set_some(n);
    }
    if let Some(n) = dto.parent_index_number {
        out.reborrow().init_parent_index_number().set_some(n);
    }
}

fn write_view(mut item: item_summary::Builder<'_>, id: WireId, view: &LibraryView) {
    set_item_id(item.reborrow().init_id(), id);
    item.set_kind(Kind::Library);
    item.set_name(view.name.as_str());
    item.set_library_kind(wire_library_kind(view.kind));
    if let Some(tag) = &view.primary_tag {
        let mut image = item.init_images(1).get(0);
        image.set_kind("Primary");
        image.set_tag(tag.as_str());
        image.set_url(format!("/Items/{}/Images/Primary?tag={tag}", view.id).as_str());
    }
}

fn wire_library_kind(kind: LibraryKind) -> WireKind {
    match kind {
        LibraryKind::Mixed => WireKind::Mixed,
        LibraryKind::Movies => WireKind::Movies,
        LibraryKind::TvShows => WireKind::TvShows,
        LibraryKind::Music => WireKind::Music,
        LibraryKind::Books => WireKind::Books,
    }
}
