//! `Library`: what a signed-in user may browse. Each method calls one
//! `services::library` function and encodes its answer (US4).

use std::rc::Rc;

use pharos_api::pharos_capnp::{
    home_result, item_id, item_summary, items_result, library as wire, user_state, Kind,
    LibraryKind as WireKind,
};
use pharos_api::{server, Denial};
use pharos_core::{LibraryKind, MediaItem, MediaKind, UserItemData};
use pharos_jellyfin_api::dto::{BaseItemDto, WireId};

use super::session::Session;
use crate::services::library::{
    self, DetailError, ItemQuery, KindFilter, LibraryView, ListError, ListPage, QueryPage,
    QUERY_DEFAULT, QUERY_MAX,
};

pub(super) struct Library {
    session: Rc<Session>,
}

impl Library {
    pub(super) fn new(session: Rc<Session>) -> Self {
        Self { session }
    }
}

impl server::library::Handler for Library {
    async fn views(
        self: Rc<Self>,
        _params: wire::ViewsParams,
        results: &mut wire::ViewsResults,
    ) -> Result<(), capnp::Error> {
        let user = match self.session.current_user().await {
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
        let user = match self.session.current_user().await {
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
        let user = match self.session.current_user().await {
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

    async fn query(
        self: Rc<Self>,
        params: wire::QueryParams,
        results: &mut wire::QueryResults,
    ) -> Result<(), capnp::Error> {
        let q = params.get()?.get_q()?;
        let parent = if q.has_parent_id() {
            Some(read_item_id(q.get_parent_id()?)?.to_string())
        } else {
            None
        };
        let text = |t: capnp::text::Reader<'_>| -> Result<Option<String>, capnp::Error> {
            let t = t.to_str()?.trim();
            Ok((!t.is_empty()).then(|| t.to_owned()))
        };
        let mut filters = Vec::new();
        for f in q.get_filters()? {
            filters.push(f?.to_str()?.to_owned());
        }
        let limit = match q.get_limit() {
            0 => QUERY_DEFAULT,
            n => n.min(QUERY_MAX),
        };
        let query = ItemQuery {
            start_index: q.get_start(),
            limit: Some(limit),
            kinds: kind_filter(q.get_kinds()?),
            sort_by: text(q.get_sort_by()?)?,
            sort_order: q.get_descending().then(|| "Descending".to_owned()),
            name_starts_with: text(q.get_name_starts_with()?)?,
            filters: (!filters.is_empty()).then(|| filters.join(",")),
            ..ItemQuery::default()
        };
        let user = match self.session.current_user().await {
            Ok(user) => user,
            Err(denial) => {
                denial.write(results.get().init_result().init_error());
                return Ok(());
            }
        };
        let state = &self.session.state;
        let listed = match library::query(state, &user, parent.as_deref(), &query).await {
            Ok(page) => library::with_user_data(state, &user, page).await,
            Err(e) => Err(e),
        }
        .map_err(ListError::Store);
        write_listing(
            results.get().init_result(),
            listed,
            "query",
            &state.server_id,
        );
        Ok(())
    }

    async fn search(
        self: Rc<Self>,
        params: wire::SearchParams,
        results: &mut wire::SearchResults,
    ) -> Result<(), capnp::Error> {
        let params = params.get()?;
        let text = params.get_text()?.to_str()?;
        let user = match self.session.current_user().await {
            Ok(user) => user,
            Err(denial) => {
                denial.write(results.get().init_result().init_error());
                return Ok(());
            }
        };
        let state = &self.session.state;
        let listed = match library::search(state, &user, text, params.get_limit()).await {
            Ok(items) => {
                let total = u32::try_from(items.len()).unwrap_or(u32::MAX);
                library::with_user_data(
                    state,
                    &user,
                    QueryPage {
                        items,
                        total,
                        start: 0,
                    },
                )
                .await
            }
            Err(e) => Err(e),
        }
        .map_err(ListError::Store);
        write_listing(
            results.get().init_result(),
            listed,
            "search",
            &state.server_id,
        );
        Ok(())
    }

    async fn detail(
        self: Rc<Self>,
        params: wire::DetailParams,
        results: &mut wire::DetailResults,
    ) -> Result<(), capnp::Error> {
        let id = read_item_id(params.get()?.get_id()?)?;
        let user = match self.session.current_user().await {
            Ok(user) => user,
            Err(denial) => {
                denial.write(results.get().init_result().init_error());
                return Ok(());
            }
        };
        let item_not_found = Denial::NotFound { what: "item" };
        // Synthesised items (series, season, album…) have no detail here yet.
        let WireId::Real(id) = id else {
            item_not_found.write(results.get().init_result().init_error());
            return Ok(());
        };
        let state = &self.session.state;
        let (item, user_data) = match library::detail(state, &user, id).await {
            Ok(found) => found,
            Err(e) => {
                item_denial(id, &e, "detail").write(results.get().init_result().init_error());
                return Ok(());
            }
        };
        let dto = BaseItemDto::from_domain_with_user_data(&item, &state.server_id, user_data);
        let mut out = results.get().init_result().init_detail();
        write_item(
            out.reborrow().init_summary(),
            &item,
            user_data,
            &state.server_id,
        );
        out.set_overview(dto.overview.as_deref().unwrap_or_default());
        let count = u32::try_from(dto.genres.len()).unwrap_or(u32::MAX);
        let mut genres = out.reborrow().init_genres(count);
        for (i, genre) in (0..count).zip(&dto.genres) {
            genres.set(i, genre.as_str());
        }
        let count = u32::try_from(dto.chapters.len()).unwrap_or(u32::MAX);
        let mut chapters = out.init_chapters(count);
        for (i, chapter) in (0..count).zip(&dto.chapters) {
            let mut c = chapters.reborrow().get(i);
            c.set_name(chapter.name.as_str());
            c.set_start_ms(chapter.start_position_ticks / 10_000);
        }
        Ok(())
    }

    async fn home(
        self: Rc<Self>,
        params: wire::HomeParams,
        results: &mut wire::HomeResults,
    ) -> Result<(), capnp::Error> {
        let params = params.get()?;
        // 0 is each field's default: the caller asked for no particular size.
        let size = |n: u32| Some(n).filter(|&n| n > 0);
        let (latest, resume, next_up) = (
            size(params.get_latest_per_view()),
            size(params.get_resume_limit()),
            size(params.get_next_up_limit()),
        );
        let user = match self.session.current_user().await {
            Ok(user) => user,
            Err(denial) => {
                denial.write(results.get().init_result().init_error());
                return Ok(());
            }
        };
        let state = &self.session.state;
        let home = library::home(state, &user, latest, resume, next_up).await;
        write_home(results.get().init_result(), home, &state.server_id);
        Ok(())
    }

    async fn next_up(
        self: Rc<Self>,
        params: wire::NextUpParams,
        results: &mut wire::NextUpResults,
    ) -> Result<(), capnp::Error> {
        // 0 is the field's default: the caller asked for no particular limit.
        let limit = Some(params.get()?.get_limit()).filter(|&n| n > 0);
        let user = match self.session.current_user().await {
            Ok(user) => user,
            Err(denial) => {
                denial.write(results.get().init_result().init_error());
                return Ok(());
            }
        };
        let state = &self.session.state;
        let listed = library::next_up(state, &user, None, limit).await;
        write_listing(
            results.get().init_result(),
            listed,
            "next_up",
            &state.server_id,
        );
        Ok(())
    }
}

/// The home page, or `unavailable` with its cause logged.
fn write_home(
    out: home_result::Builder<'_>,
    home: Result<library::Home, ListError>,
    server_id: &str,
) {
    let home = match home {
        Ok(home) => home,
        Err(e) => {
            tracing::warn!(method = "home", error = %list_error_detail(&e), "native library: listing unavailable");
            Denial::Unavailable {
                dependency: "media store",
            }
            .write(out.init_error());
            return;
        }
    };
    let rails: Vec<(WireId, &LibraryView, &Rows)> = home
        .rails
        .iter()
        .filter_map(|(view, items)| match WireId::parse(&view.id) {
            Some(id) => Some((id, view, items.as_slice())),
            None => {
                tracing::warn!(id = %view.id, name = %view.name, "native library: view id is not a wire id; skipped");
                None
            }
        })
        .collect();
    let mut out = out.init_home();
    let count = u32::try_from(rails.len()).unwrap_or(u32::MAX);
    let mut list = out.reborrow().init_rails(count);
    for (i, (id, view, items)) in (0..count).zip(&rails) {
        let mut rail = list.reborrow().get(i);
        write_view(rail.reborrow().init_view(), *id, view);
        write_items(rail.init_latest(len_u32(items)), items, server_id);
    }
    write_items(
        out.reborrow().init_resume(len_u32(&home.resume.items)),
        &home.resume.items,
        server_id,
    );
    write_items(
        out.init_next_up(len_u32(&home.next_up.items)),
        &home.next_up.items,
        server_id,
    );
}

/// Listed items with the user's play state, as the services return them.
type Rows = [(MediaItem, UserItemData)];

fn len_u32<T>(items: &[T]) -> u32 {
    u32::try_from(items.len()).unwrap_or(u32::MAX)
}

/// `items` into `out`, which holds `len_u32(items)` summaries.
fn write_items(
    mut out: capnp::struct_list::Builder<'_, item_summary::Owned>,
    items: &Rows,
    server_id: &str,
) {
    for (i, (item, user_data)) in (0..out.len()).zip(items) {
        write_item(out.reborrow().get(i), item, *user_data, server_id);
    }
}

fn list_error_detail(e: &ListError) -> String {
    match e {
        ListError::Hidden(hidden) => hidden.label().to_owned(),
        ListError::Store(e) => e.to_string(),
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
            tracing::warn!(method, error = %list_error_detail(&e), "native library: listing unavailable");
            Denial::Unavailable {
                dependency: "media store",
            }
            .write(out.init_error());
            return;
        }
    };
    let mut page = out.init_page();
    page.set_total(listed.total);
    page.set_start(listed.start);
    write_items(
        page.init_items(len_u32(&listed.items)),
        &listed.items,
        server_id,
    );
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

pub(super) fn read_item_id(id: item_id::Reader<'_>) -> Result<WireId, capnp::Error> {
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

/// Why a call on stored item `id` has no item to answer with, as the denial
/// it sends, logged. A hidden item answers as a missing one, so its id says
/// nothing about what exists.
pub(super) fn item_denial(id: u64, e: &DetailError, call: &'static str) -> Denial {
    use crate::services::access::ItemHidden;
    let item_not_found = Denial::NotFound { what: "item" };
    let unavailable = Denial::Unavailable {
        dependency: "media store",
    };
    let (denial, detail) = match e {
        DetailError::Hidden(ItemHidden::Unavailable(detail)) => (unavailable, detail.clone()),
        DetailError::Hidden(hidden) => (item_not_found, hidden.label().to_owned()),
        DetailError::NotFound => (item_not_found, "not_found".to_owned()),
        DetailError::Store(e) => (unavailable, e.to_string()),
    };
    if matches!(denial, Denial::Unavailable { .. }) {
        tracing::warn!(media.id = id, call, error = %detail, "native item call: store unavailable");
    } else {
        tracing::debug!(media.id = id, call, reason = %detail, "native item call: refused");
    }
    denial
}

pub(super) fn write_user_state(mut out: user_state::Builder<'_>, data: UserItemData) {
    out.set_played(data.played);
    out.set_favorite(data.is_favorite);
    out.set_position_ms(data.last_played_position_ticks / 10_000);
    out.set_play_count(data.play_count);
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
    write_user_state(out.reborrow().init_user_state(), user_data);
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
