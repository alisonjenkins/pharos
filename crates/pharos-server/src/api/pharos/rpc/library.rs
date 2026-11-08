//! `Library`: what a signed-in user may browse. Each method calls one
//! `services::library` function and encodes its answer (US4).

use std::rc::Rc;

use pharos_api::pharos_capnp::{
    credit, home_result, item_detail, item_id, item_summary, items_result, library as wire,
    person_credit, person_result, track, user_state, version, CreditKind, Kind,
    LibraryKind as WireKind, SegmentKind, Stereo3d, StereoFilter, SubtitleDelivery,
};
use pharos_api::{server, Denial};
use pharos_core::{LibraryKind, MediaItem, MediaKind, MediaProbe, UserItemData, Video3DFormat};
use pharos_jellyfin_api::dto::{BaseItemDto, WireId};
use pharos_jellyfin_api::image_tag_sig::ImageTagKey;

use super::session::Session;
use crate::api::jellyfin::items::SynthPage;
use crate::api::jellyfin::media_source;
use crate::services::library::{
    self, DetailError, GroupError, Grouping, ItemQuery, KindFilter, LibraryView, ListError,
    ListPage, QueryPage, QUERY_DEFAULT, QUERY_MAX,
};
use crate::services::person::{self, PersonError};
use crate::services::search::Hit;
use crate::services::{lyrics, mix, segments, watch};
use crate::state::AppState;

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
            write_view(
                items.reborrow().get(i),
                *id,
                view,
                &self.session.state.image_tag_key,
            );
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
            &state.image_tag_key,
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
            &state.image_tag_key,
        );
        Ok(())
    }

    async fn query(
        self: Rc<Self>,
        params: wire::QueryParams,
        results: &mut wire::QueryResults,
    ) -> Result<(), capnp::Error> {
        let (parent, query, grouping) = read_query(params.get()?.get_q()?)?;
        let user = match self.session.current_user().await {
            Ok(user) => user,
            Err(denial) => {
                denial.write(results.get().init_result().init_error());
                return Ok(());
            }
        };
        let state = &self.session.state;
        let target = browse_target(state, parent.as_deref()).await;
        if let Some(grouping) = grouping {
            let listed = library::grouped(state, &user, parent.as_deref(), grouping, &query).await;
            record_browse(
                parent.as_deref(),
                target,
                listed.as_ref().ok().map(|p| (p.items.len(), p.total)),
                query.start_index,
            );
            write_grouped(
                results.get().init_result(),
                listed,
                grouping,
                parent.as_deref(),
                query.start_index,
                &state.image_tag_key,
            );
            return Ok(());
        }
        let listed = match library::query(state, &user, parent.as_deref(), &query).await {
            Ok(page) => library::with_user_data(state, &user, page).await,
            Err(e) => Err(e),
        }
        .map_err(ListError::Store);
        record_query(QUERY_SHAPE_STORED, listed.is_ok());
        record_browse(
            parent.as_deref(),
            target,
            listed.as_ref().ok().map(|p| (p.items.len(), p.total)),
            query.start_index,
        );
        write_listing(
            results.get().init_result(),
            listed,
            "query",
            &state.server_id,
            &state.image_tag_key,
        );
        Ok(())
    }

    async fn index(
        self: Rc<Self>,
        params: wire::IndexParams,
        results: &mut wire::IndexResults,
    ) -> Result<(), capnp::Error> {
        let (parent, query, grouping) = read_query(params.get()?.get_q()?)?;
        let user = match self.session.current_user().await {
            Ok(user) => user,
            Err(denial) => {
                denial.write(results.get().init_result().init_error());
                return Ok(());
            }
        };
        let indexed = library::index(
            &self.session.state,
            &user,
            parent.as_deref(),
            grouping,
            &query,
        )
        .await;
        let out = results.get().init_result();
        let index = match indexed {
            Ok(index) => index,
            Err(GroupError::SeasonsOutsideSeries) => {
                Denial::NotPermitted {
                    what: "seasons outside a series",
                }
                .write(out.init_error());
                return Ok(());
            }
            Err(GroupError::Store(error)) => {
                tracing::warn!(parent = parent.as_deref().unwrap_or("none"), %error, "native library: list index unavailable");
                Denial::Unavailable {
                    dependency: "media store",
                }
                .write(out.init_error());
                return Ok(());
            }
        };
        let mut out = out.init_index();
        let mut letters = out.reborrow().init_letters(len_u32(&index.letters));
        for (i, (letter, start)) in (0..).zip(&index.letters) {
            let mut entry = letters.reborrow().get(i);
            entry.set_letter(letter.as_str());
            entry.set_start(*start);
        }
        let mut genres = out.init_genres(len_u32(&index.genres));
        for (i, genre) in (0..).zip(&index.genres) {
            genres.set(i, genre.as_str());
        }
        Ok(())
    }

    async fn lyrics(
        self: Rc<Self>,
        params: wire::LyricsParams,
        results: &mut wire::LyricsResults,
    ) -> Result<(), capnp::Error> {
        let p = params.get()?;
        let id = read_item_id(p.get_id()?)?;
        let want_words = p.get_want_words();
        let user = match self.session.current_user().await {
            Ok(user) => user,
            Err(denial) => {
                denial.write(results.get().init_result().init_error());
                return Ok(());
            }
        };
        let out = results.get().init_result();
        // Only a song has lyrics, and no song has a synthetic id.
        let WireId::Real(id) = id else {
            out.init_lyrics();
            return Ok(());
        };
        let found = match lyrics::for_item(&self.session.state, &user, id, want_words).await {
            Ok(found) => found,
            Err(e) => {
                item_denial(id, &e, "lyrics").write(out.init_error());
                return Ok(());
            }
        };
        let mut out = out.init_lyrics();
        let Some((found, words_pending)) = found else {
            return Ok(());
        };
        out.set_synced(found.synced());
        out.set_source(found.origin.label());
        out.set_words_pending(words_pending);
        let mut lines = out.init_lines(len_u32(&found.lines));
        for (i, line) in (0..).zip(&found.lines) {
            let mut w = lines.reborrow().get(i);
            w.set_start_ms(line.start_ms.unwrap_or(0));
            w.set_text(line.text.as_str());
            let mut words = w.init_words(len_u32(&line.words));
            for (j, word) in (0..).zip(&line.words) {
                let mut ww = words.reborrow().get(j);
                ww.set_at(word.at);
                ww.set_start_ms(word.start_ms);
            }
        }
        Ok(())
    }

    async fn mix(
        self: Rc<Self>,
        params: wire::MixParams,
        results: &mut wire::MixResults,
    ) -> Result<(), capnp::Error> {
        // Read now (the params do not outlive the await below), refuse
        // after the user is known, so a malformed ask is counted and logged
        // like any other refusal.
        let read = params
            .get()
            .and_then(|p| super::mix::read_ask(p.get_spec()?, p.get_recent()?, p.get_count()));
        let user = match self.session.current_user().await {
            Ok(user) => user,
            Err(denial) => {
                denial.write(results.get().init_result().init_error());
                return Ok(());
            }
        };
        let out = results.get().init_result();
        let (field, error) = match read {
            Ok(Ok(ask)) => {
                run_mix(&self.session.state, out, &user, ask).await;
                return Ok(());
            }
            Ok(Err(super::mix::BadField(field))) => (field, String::new()),
            Err(e) => ("spec", e.to_string()),
        };
        metrics::counter!("pharos_mix_total", "outcome" => "invalid").increment(1);
        tracing::warn!(user.id = %user.id, outcome = "invalid", field, %error, "mix: no batch");
        Denial::Invalid { field }.write(out.init_error());
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
        let out = results.get().init_result();
        let hits =
            match crate::services::search::search(state, &user, text, params.get_limit()).await {
                Ok(hits) => hits,
                Err(e) => {
                    write_listing(
                        out,
                        Err(ListError::Store(e)),
                        "search",
                        &state.server_id,
                        &state.image_tag_key,
                    );
                    return Ok(());
                }
            };
        let stored: Vec<MediaItem> = hits
            .iter()
            .filter_map(|hit| match hit {
                Hit::Stored(item) => Some(item.as_ref().clone()),
                Hit::Group(..) | Hit::Person(..) => None,
            })
            .collect();
        let page = QueryPage {
            items: stored,
            total: 0,
            start: 0,
        };
        let mut played = match library::with_user_data(state, &user, page).await {
            Ok(listed) => listed.items.into_iter().map(|(_, data)| data),
            Err(e) => {
                write_listing(
                    out,
                    Err(ListError::Store(e)),
                    "search",
                    &state.server_id,
                    &state.image_tag_key,
                );
                return Ok(());
            }
        };
        let mut page = out.init_page();
        page.set_total(len_u32(&hits));
        page.set_start(0);
        let mut items = page.init_items(len_u32(&hits));
        for (i, hit) in (0..items.len()).zip(&hits) {
            match hit {
                Hit::Group(id, dto) => {
                    write_synth_summary(items.reborrow().get(i), *id, dto, &state.image_tag_key)
                }
                Hit::Person(id, person) => {
                    write_person_summary(items.reborrow().get(i), *id, person)
                }
                Hit::Stored(item) => write_item(
                    items.reborrow().get(i),
                    item,
                    played.next().unwrap_or_default(),
                    &state.server_id,
                    &state.image_tag_key,
                ),
            }
        }
        Ok(())
    }

    async fn detail(
        self: Rc<Self>,
        params: wire::DetailParams,
        results: &mut wire::DetailResults,
    ) -> Result<(), capnp::Error> {
        let params = params.get()?;
        let id = read_item_id(params.get_id()?)?;
        let asked_version = params.get_version()?.to_str()?.trim().to_owned();
        let knows_image_delivery =
            params.get_ctx()?.get_schema_version() >= IMAGE_DELIVERY_SCHEMA_VERSION;
        let user = match self.session.current_user().await {
            Ok(user) => user,
            Err(denial) => {
                denial.write(results.get().init_result().init_error());
                return Ok(());
            }
        };
        let state = &self.session.state;
        let id = match id {
            WireId::Real(id) => id,
            synth @ WireId::Synth(_) => {
                match synth_detail(state, &user, synth, "detail").await {
                    Ok(dto) => {
                        let played = synth_played(state, &user, synth, &dto).await;
                        let cast = synth_cast(state, synth, &dto).await;
                        let mut out = results.get().init_result().init_detail();
                        write_synth_detail(out.reborrow(), synth, &dto, &state.image_tag_key);
                        write_cast(out.reborrow().init_cast(len(&cast)), &cast);
                        if played {
                            let data = UserItemData {
                                played,
                                ..UserItemData::default()
                            };
                            write_user_state(out.get_summary()?.init_user_state(), data);
                        }
                    }
                    Err(denial) => denial.write(results.get().init_result().init_error()),
                }
                return Ok(());
            }
        };
        let (item, user_data) = match library::detail(state, &user, id).await {
            Ok(found) => found,
            Err(e) => {
                item_denial(id, &e, "detail").write(results.get().init_result().init_error());
                return Ok(());
            }
        };
        let has_versions = matches!(item.kind, MediaKind::Movie | MediaKind::Episode);
        // The versions list describes every file; the rest of the detail
        // (tracks, fonts, subtitle URLs) describes the one asked for.
        let all = item.clone();
        let item = match select_version(media_source::Route::NativeDetail, item, &asked_version) {
            Ok(item) => item,
            Err(denial) => {
                denial.write(results.get().init_result().init_error());
                return Ok(());
            }
        };
        // Spec 028 — a viewer who opens a video gets its subtitles styled
        // ahead of the backfill.
        if let Some(queue) = state.subtitle_styling.as_ref() {
            if matches!(item.kind, MediaKind::Movie | MediaKind::Episode) {
                queue.request(item.id);
            }
        }
        let dto = BaseItemDto::from_domain_with_user_data(&item, &state.server_id, user_data);
        let mut out = results.get().init_result().init_detail();
        let mut summary = out.reborrow().init_summary();
        write_item(
            summary.reborrow(),
            &item,
            user_data,
            &state.server_id,
            &state.image_tag_key,
        );
        // A film sits straight under its library, as Jellyfin's detail says,
        // so the UI can name the library above it.
        if !summary.has_parent_id() {
            if let Some(library) = crate::api::jellyfin::items::library_parent_id(state, &item)
                .as_deref()
                .and_then(WireId::parse)
            {
                set_item_id(summary.init_parent_id(), library);
            }
        }
        out.set_overview(dto.overview.as_deref().unwrap_or_default());
        let mut artists = out.reborrow().init_artists(len(&dto.artists));
        for (i, artist) in (0..).zip(&dto.artists) {
            artists.set(i, artist.as_str());
        }
        out.set_album(dto.album.as_deref().unwrap_or_default());
        let count = u32::try_from(dto.genres.len()).unwrap_or(u32::MAX);
        let mut genres = out.reborrow().init_genres(count);
        for (i, genre) in (0..count).zip(&dto.genres) {
            genres.set(i, genre.as_str());
        }
        let count = u32::try_from(dto.chapters.len()).unwrap_or(u32::MAX);
        let mut chapters = out.reborrow().init_chapters(count);
        for (i, chapter) in (0..count).zip(&dto.chapters) {
            let mut c = chapters.reborrow().get(i);
            c.set_name(chapter.name.as_str());
            c.set_start_ms(chapter.start_position_ticks / 10_000);
        }
        let draws_images = knows_image_delivery && watch::extracts_image_subtitles(state);
        // Styling is generated from the primary's subtitles.
        let styling = if watch::is_video(&item) && item.edition.is_none() {
            crate::styled_track::current(&state.stores, &item).await
        } else {
            None
        };
        let tracks = watch::tracks(&item, draws_images, styling.as_ref()).await;
        write_tracks(
            out.reborrow().init_audio_tracks(len(&tracks.audio)),
            &tracks.audio,
        );
        write_tracks(
            out.reborrow().init_subtitle_tracks(len(&tracks.subtitles)),
            &tracks.subtitles,
        );
        let trickplay = watch::trickplay(state, &item);
        let mut sets = out.reborrow().init_trickplay(len(&trickplay));
        for (i, t) in (0..).zip(&trickplay) {
            let mut w = sets.reborrow().get(i);
            w.set_width(t.width);
            w.set_height(t.height);
            w.set_interval_ms(t.interval_ms);
            w.set_thumb_count(t.thumb_count);
            w.set_tile_count(t.tile_count);
            w.set_grid(t.grid);
            w.set_tile_url(t.tile_url.as_str());
        }
        let fonts = watch::fonts(&item);
        let mut list = out.reborrow().init_fonts(len(&fonts));
        for (i, f) in (0..).zip(&fonts) {
            let mut w = list.reborrow().get(i);
            w.set_index(f.index);
            w.set_filename(f.filename.as_str());
            w.set_mime(f.mime.as_str());
            w.set_url(f.url.as_str());
        }
        if has_versions {
            write_versions(
                out.reborrow().init_versions(version_count(&all)),
                &all,
                user.policy.admin,
            );
        }
        let segments: Vec<_> = segments::for_item(state, &item)
            .await
            .into_iter()
            .filter_map(|s| segment_kind(s.kind).map(|k| (k, s)))
            .collect();
        let mut list = out.reborrow().init_segments(len(&segments));
        for (i, (kind, s)) in (0..).zip(&segments) {
            let mut w = list.reborrow().get(i);
            w.set_kind(*kind);
            w.set_start_ms(s.start_ms);
            w.set_end_ms(s.end_ms);
        }
        // The cast decorates the page; a store error leaves it empty rather
        // than failing the whole detail.
        let cast = library::cast(state, id).await.unwrap_or_else(|e| {
            tracing::warn!(media.id = id, error = %e, "native detail: cast unavailable");
            Vec::new()
        });
        write_cast(out.init_cast(len(&cast)), &cast);
        tracing::debug!(
            media.id = id,
            audio_tracks = tracks.audio.len(),
            subtitle_tracks = tracks.subtitles.len(),
            trickplay_widths = trickplay.len(),
            segments = segments.len(),
            cast = cast.len(),
            "native detail: player facts"
        );
        Ok(())
    }

    async fn person(
        self: Rc<Self>,
        params: wire::PersonParams,
        results: &mut wire::PersonResults,
    ) -> Result<(), capnp::Error> {
        let id = read_item_id(params.get()?.get_id()?)?;
        let user = match self.session.current_user().await {
            Ok(user) => user,
            Err(denial) => {
                denial.write(results.get().init_result().init_error());
                return Ok(());
            }
        };
        let state = &self.session.state;
        let not_found = Denial::NotFound { what: "person" };
        let page = match person::page(state, &user, &id.to_string()).await {
            Ok(page) => page,
            Err(PersonError::NotFound | PersonError::Hidden) => {
                not_found.write(results.get().init_result().init_error());
                return Ok(());
            }
            Err(PersonError::Store(_)) => {
                Denial::Unavailable {
                    dependency: "media store",
                }
                .write(results.get().init_result().init_error());
                return Ok(());
            }
        };
        // Each show's own projection, for its poster; one that fails is named
        // from its episodes instead, and logged.
        use futures_util::StreamExt as _;
        let shows: Vec<_> = futures_util::stream::iter(page.shows.iter().map(|show| {
            let user = &user;
            async move {
                let series = WireId::parse(&show.series_id)?;
                match synth_detail(state, user, series, "person").await {
                    Ok(dto) => Some((series, dto)),
                    Err(_) => {
                        tracing::warn!(series.id = %series, "person page: show named from its episodes");
                        None
                    }
                }
            }
        }))
        .buffered(PERSON_SHOWS_AT_ONCE)
        .collect()
        .await;
        write_person(
            results.get().init_result(),
            id,
            &page,
            &shows,
            &state.server_id,
            &state.image_tag_key,
        );
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
        write_home(
            results.get().init_result(),
            home,
            &state.server_id,
            &state.image_tag_key,
        );
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
            &state.image_tag_key,
        );
        Ok(())
    }
}

/// A wire `ItemQuery` as the parent's id, the service query and the grouping
/// it names, shared by `query` and `index`.
fn read_query(
    q: pharos_api::pharos_capnp::item_query::Reader<'_>,
) -> Result<(Option<String>, ItemQuery, Option<Grouping>), capnp::Error> {
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
    let sort_by = text(q.get_sort_by()?)?;
    // A first key of date added takes an order token per key, so the
    // listing takes the per-key chain: with one, the Jellyfin route reads it
    // by its legacy rule, newest first when asked ascending (B316). Any other
    // order keeps its one token, and with it the legacy tie-breaks.
    let by_added = crate::services::library::Sections::of(sort_by.as_deref())
        == crate::services::library::Sections::Added;
    let order = if q.get_descending() {
        "Descending"
    } else {
        "Ascending"
    };
    let sort_order = if by_added {
        Some(format!("{order},{order}"))
    } else {
        q.get_descending().then(|| order.to_owned())
    };
    let query = ItemQuery {
        start_index: q.get_start(),
        limit: Some(limit),
        kinds: kind_filter(q.get_kinds()?),
        sort_by,
        sort_order,
        name_starts_with: text(q.get_name_starts_with()?)?,
        filters: (!filters.is_empty()).then(|| filters.join(",")),
        genre: text(q.get_genre()?)?,
        // An enum value this build does not know narrows nothing.
        is_3d: match q.get_stereo() {
            Ok(StereoFilter::Only3d) => Some(true),
            Ok(StereoFilter::Only2d) => Some(false),
            Ok(StereoFilter::Any) | Err(_) => None,
        },
        ..ItemQuery::default()
    };
    let grouping = grouping_of(q.get_kinds()?);
    Ok((parent, query, grouping))
}

/// The home page, or `unavailable` with its cause logged.
fn write_home(
    out: home_result::Builder<'_>,
    home: Result<library::Home, ListError>,
    server_id: &str,
    key: &ImageTagKey,
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
        write_view(rail.reborrow().init_view(), *id, view, key);
        write_items(rail.init_latest(len_u32(items)), items, server_id, key);
    }
    write_items(
        out.reborrow().init_resume(len_u32(&home.resume.items)),
        &home.resume.items,
        server_id,
        key,
    );
    write_items(
        out.reborrow()
            .init_listening(len_u32(&home.listening.items)),
        &home.listening.items,
        server_id,
        key,
    );
    write_items(
        out.init_next_up(len_u32(&home.next_up.items)),
        &home.next_up.items,
        server_id,
        key,
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
    key: &ImageTagKey,
) {
    for (i, (item, user_data)) in (0..out.len()).zip(items) {
        write_item(out.reborrow().get(i), item, *user_data, server_id, key);
    }
}

fn list_error_detail(e: &ListError) -> String {
    match e {
        ListError::Hidden(hidden) => hidden.label().to_owned(),
        ListError::Store(e) => e.to_string(),
    }
}

/// Runs a readable mix ask and writes its batch or refusal into `out`.
async fn run_mix(
    state: &AppState,
    out: pharos_api::pharos_capnp::mix_result::Builder<'_>,
    user: &pharos_core::User,
    ask: mix::MixAsk,
) {
    let now = crate::sync_recovery::now_unix_secs();
    match mix::mix(state, user, ask, now).await {
        Ok(mixed) => super::mix::write_batch(
            out.init_batch(),
            &mixed,
            &state.server_id,
            &state.image_tag_key,
        ),
        Err(mix::MixError::Invalid(e)) => {
            Denial::Invalid { field: e.label() }.write(out.init_error())
        }
        Err(mix::MixError::SeedNotFound(_)) => {
            Denial::NotFound { what: "seed song" }.write(out.init_error());
        }
        Err(mix::MixError::Store(_)) => Denial::Unavailable {
            dependency: "media store",
        }
        .write(out.init_error()),
    }
}

/// A listing's page, or `unavailable` with its cause logged.
fn write_listing(
    out: items_result::Builder<'_>,
    listed: Result<ListPage, ListError>,
    method: &'static str,
    server_id: &str,
    key: &ImageTagKey,
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
        key,
    );
}

/// The grouping a `kinds` list asks for: exactly one kind pharos only
/// synthesises (series, season, album, artist), named once or more. Any
/// other list is a stored-row listing, filtered by [`kind_filter`].
fn grouping_of(kinds: capnp::enum_list::Reader<'_, Kind>) -> Option<Grouping> {
    let mut found = None;
    for kind in kinds.iter() {
        let grouping = match kind.ok()? {
            Kind::Series => Grouping::Series,
            Kind::Season => Grouping::Season,
            Kind::Album => Grouping::Album,
            Kind::Artist => Grouping::Artist,
            _ => return None,
        };
        if found.is_some_and(|g| g != grouping) {
            return None;
        }
        found = Some(grouping);
    }
    found
}

/// The `shape` label of `pharos_rpc_query_total` for a stored-row listing;
/// a grouped one carries [`Grouping::label`].
const QUERY_SHAPE_STORED: &str = "stored";

/// Counts one native `Library.query` under
/// `pharos_rpc_query_total{shape,outcome}`: which listing answered it, and
/// whether it had a page to send.
fn record_query(shape: &'static str, ok: bool) {
    metrics::counter!(
        "pharos_rpc_query_total",
        "shape" => shape,
        "outcome" => if ok { "ok" } else { "error" }
    )
    .increment(1);
}

/// What a native browse is listing the children of. The `level` label of
/// `pharos_rpc_browse_total`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowseLevel {
    /// A library view.
    Library,
    /// A series: its seasons.
    Series,
    /// A season: its episodes.
    Season,
    /// Anything else: no parent, a genre, a collection, an unknown id.
    Other,
}

impl BrowseLevel {
    pub fn label(self) -> &'static str {
        match self {
            BrowseLevel::Library => "library",
            BrowseLevel::Series => "series",
            BrowseLevel::Season => "season",
            BrowseLevel::Other => "other",
        }
    }
}

/// The `kind` label of `pharos_rpc_browse_total`: the library kind browsed,
/// `tvshows` for the levels under a TV library, `none` for a parent that is
/// no library.
fn browse_kind_label(kind: Option<LibraryKind>) -> &'static str {
    kind.map_or("none", LibraryKind::collection_type)
}

/// The level and library kind a `Library.query` parent names, resolved by
/// the same `resolve_parent_filter` the listing uses. A store error counts as
/// `other`: the listing reports it itself.
async fn browse_target(
    state: &AppState,
    parent: Option<&str>,
) -> (BrowseLevel, Option<LibraryKind>) {
    use crate::api::jellyfin::items::{resolve_parent_filter, ParentResolution};
    use pharos_core::ParentFilter;
    match resolve_parent_filter(state, parent).await {
        Ok(ParentResolution::Filter(ParentFilter::Library { wire_id })) => {
            let kind = state
                .libraries()
                .iter()
                .find(|l| l.wire_id == wire_id)
                .map(|l| l.kind);
            (BrowseLevel::Library, kind)
        }
        Ok(ParentResolution::PathPrefix(root)) => {
            let kind = state
                .libraries()
                .iter()
                .find(|l| l.root_path == root)
                .map_or(LibraryKind::Mixed, |l| l.kind);
            (BrowseLevel::Library, Some(kind))
        }
        Ok(ParentResolution::Filter(ParentFilter::Series { .. })) => {
            (BrowseLevel::Series, Some(LibraryKind::TvShows))
        }
        Ok(ParentResolution::Filter(ParentFilter::Season { .. })) => {
            (BrowseLevel::Season, Some(LibraryKind::TvShows))
        }
        _ => (BrowseLevel::Other, None),
    }
}

/// Records a native browse: a count under `pharos_rpc_browse_total{level,kind}`
/// for every query, and, when it had a page (`listed` is its item count and
/// total), a line with the parent, level and counts. An empty container logs
/// at INFO (a library, series or season that lists nothing is the symptom of
/// a browse that cannot reach its children); everything else at DEBUG. A
/// query without a page is logged by its refusal.
fn record_browse(
    parent: Option<&str>,
    target: (BrowseLevel, Option<LibraryKind>),
    listed: Option<(usize, u32)>,
    start: u32,
) {
    let (level, kind) = target;
    let kind = browse_kind_label(kind);
    metrics::counter!(
        "pharos_rpc_browse_total",
        "level" => level.label(),
        "kind" => kind
    )
    .increment(1);
    let Some((count, total)) = listed else {
        return;
    };
    let parent = parent.unwrap_or("");
    if count == 0 && level != BrowseLevel::Other {
        tracing::info!(
            parent,
            level = level.label(),
            kind,
            count,
            total,
            start,
            "native browse: empty listing"
        );
    } else {
        tracing::debug!(
            parent,
            level = level.label(),
            kind,
            count,
            total,
            start,
            "native browse"
        );
    }
}

/// A grouped listing's page, each group read from its Jellyfin DTO the way
/// `Library.detail` reads it; or the refusal, logged and counted.
fn write_grouped(
    out: items_result::Builder<'_>,
    listed: Result<SynthPage, GroupError>,
    grouping: Grouping,
    parent: Option<&str>,
    start: u32,
    key: &ImageTagKey,
) {
    record_query(grouping.label(), listed.is_ok());
    let listed = match listed {
        Ok(listed) => listed,
        Err(GroupError::SeasonsOutsideSeries) => {
            tracing::info!(
                shape = grouping.label(),
                parent = parent.unwrap_or("none"),
                "native library: seasons asked for outside a series; refused"
            );
            Denial::NotPermitted {
                what: "seasons outside a series",
            }
            .write(out.init_error());
            return;
        }
        Err(GroupError::Store(error)) => {
            tracing::warn!(shape = grouping.label(), parent = parent.unwrap_or("none"), %error, "native library: grouped listing unavailable");
            Denial::Unavailable {
                dependency: "media store",
            }
            .write(out.init_error());
            return;
        }
    };
    let groups: Vec<(WireId, &serde_json::Value)> = listed
        .items
        .iter()
        .filter_map(|dto| match WireId::parse(dto_str(dto, "Id")) {
            Some(id) => Some((id, dto)),
            None => {
                tracing::warn!(
                    shape = grouping.label(),
                    id = dto_str(dto, "Id"),
                    "native library: group id is not a wire id; skipped"
                );
                None
            }
        })
        .collect();
    let mut page = out.init_page();
    page.set_total(listed.total);
    page.set_start(start);
    let mut items = page.init_items(len_u32(&groups));
    for (i, (id, dto)) in (0..items.len()).zip(&groups) {
        write_synth_summary(items.reborrow().get(i), *id, dto, key);
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

pub(super) fn read_item_id(id: item_id::Reader<'_>) -> Result<WireId, capnp::Error> {
    Ok(match id.which()? {
        item_id::Real(id) => WireId::Real(id),
        item_id::Synth(id) => WireId::Synth(id),
    })
}

pub(super) fn set_item_id(mut out: item_id::Builder<'_>, id: WireId) {
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

/// Why a native call on an item answered with a denial. The `reason` label of
/// `pharos_rpc_item_refused_total`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefusalReason {
    /// The policy withholds the item; carries `ItemHidden::label`.
    Hidden(&'static str),
    /// No item has the id.
    NotFound,
    /// The store could not say.
    StoreUnavailable,
    /// A synthesised item the user's policy withholds.
    SynthHidden,
}

impl RefusalReason {
    pub fn label(self) -> &'static str {
        match self {
            RefusalReason::Hidden(label) => label,
            RefusalReason::NotFound => "not_found",
            RefusalReason::StoreUnavailable => "store_unavailable",
            RefusalReason::SynthHidden => "synth_hidden",
        }
    }
}

/// The `id_kind` label of `pharos_rpc_item_refused_total`: `real` for a store
/// row, `synth` for a synthesised group id.
fn id_kind_label(id: WireId) -> &'static str {
    match id {
        WireId::Real(_) => "real",
        WireId::Synth(_) => "synth",
    }
}

/// Records that native `call` refused item `id`: one INFO line carrying the
/// id and the reason (WARN when the store was the cause), and a count under
/// `pharos_rpc_item_refused_total{call,id_kind,reason}`.
pub(super) fn record_refusal(call: &'static str, id: WireId, reason: RefusalReason, detail: &str) {
    let id_kind = id_kind_label(id);
    metrics::counter!(
        "pharos_rpc_item_refused_total",
        "call" => call,
        "id_kind" => id_kind,
        "reason" => reason.label()
    )
    .increment(1);
    if reason == RefusalReason::StoreUnavailable {
        tracing::warn!(item.id = %id, call, id_kind, reason = reason.label(), error = %detail, "native item call: store unavailable");
    } else {
        tracing::info!(item.id = %id, call, id_kind, reason = reason.label(), detail, "native item call: refused");
    }
}

/// The synthesised item `id` (library view, series, season, album…) as the
/// Jellyfin `/Items/{id}` projection, resolved by the same
/// `synth_item_json` so the two APIs agree and apply the same access rules.
/// A refusal is recorded under `call` and answers like an id that names
/// nothing.
async fn synth_detail(
    state: &AppState,
    user: &pharos_core::User,
    id: WireId,
    call: &'static str,
) -> Result<serde_json::Value, Denial> {
    let item_not_found = Denial::NotFound { what: "item" };
    match crate::api::jellyfin::items::synth_item_json(state, user, &id.to_string()).await {
        Ok(Some(dto)) => Ok(dto),
        Ok(None) => {
            record_refusal(
                call,
                id,
                RefusalReason::NotFound,
                "no such synthesised item",
            );
            Err(item_not_found)
        }
        Err(e) if e.as_response_error().status_code().is_server_error() => {
            record_refusal(call, id, RefusalReason::StoreUnavailable, &e.to_string());
            Err(Denial::Unavailable {
                dependency: "media store",
            })
        }
        Err(e) => {
            record_refusal(call, id, RefusalReason::SynthHidden, &e.to_string());
            Err(item_not_found)
        }
    }
}

/// Whether `user` has watched all of synthesised series or season `id`;
/// false for any other kind. A store that cannot say leaves it unwatched:
/// the page still opens, and marking it is still offered.
async fn synth_played(
    state: &AppState,
    user: &pharos_core::User,
    id: WireId,
    dto: &serde_json::Value,
) -> bool {
    if !matches!(synth_kind(dto), Kind::Series | Kind::Season) {
        return false;
    }
    crate::services::user_data::folder_played(state, user, &id.to_string())
        .await
        .unwrap_or_else(|e| {
            tracing::warn!(item.id = %id, error = ?e, "detail: watched state unavailable");
            false
        })
}

/// The regular cast of a synthesised series or season; empty for any other
/// synthesised item, or when the store cannot answer.
async fn synth_cast(
    state: &AppState,
    id: WireId,
    dto: &serde_json::Value,
) -> Vec<pharos_core::ItemPerson> {
    if !matches!(synth_kind(dto), Kind::Series | Kind::Season) {
        return Vec::new();
    }
    let wire_id = id.to_string();
    library::ensemble(state, &wire_id)
        .await
        .unwrap_or_else(|e| {
            tracing::warn!(wire_id, error = %e, "native detail: ensemble cast unavailable");
            Vec::new()
        })
}

/// `cast` as wire credits, each with the person's id and photo (see
/// [`portrait_url`]).
fn write_cast(
    mut out: capnp::struct_list::Builder<'_, credit::Owned>,
    cast: &[pharos_core::ItemPerson],
) {
    for (i, p) in (0..).zip(cast) {
        let mut w = out.reborrow().get(i);
        w.set_name(p.name.as_str());
        w.set_character(p.character.as_deref().unwrap_or_default());
        w.set_kind(credit_kind(p.kind));
        if let Some(url) = portrait_url(&p.wire_id, p.thumb_url.as_deref()) {
            w.set_image_url(url.as_str());
        }
        if let Some(person) = WireId::parse(&p.wire_id) {
            set_item_id(w.init_person_id(), person);
        }
    }
}

/// A person's photo on this origin, or `None`. Only a portrait the image route
/// passes through from here gets one: the UI's CSP would block a redirect to
/// anywhere else. The tag is the one the Jellyfin routes hand out for a
/// person; portrait routes are not tag-checked (ADR-0073).
fn portrait_url(wire_id: &str, thumb_url: Option<&str>) -> Option<String> {
    let thumb = thumb_url.filter(|u| crate::portrait::is_proxied(u))?;
    let tag = pharos_jellyfin_api::dto::person_image_tag_for(thumb);
    Some(format!("/Items/{wire_id}/Images/Primary?tag={tag}"))
}

/// How many of a person's shows are projected at once: a long-running guest
/// star can be in dozens, and each projection reads the store.
const PERSON_SHOWS_AT_ONCE: usize = 8;

/// A person as a search row: their name and photo, kind `person`.
fn write_person_summary(
    mut summary: item_summary::Builder<'_>,
    id: WireId,
    person: &pharos_core::Person,
) {
    set_item_id(summary.reborrow().init_id(), id);
    summary.set_kind(Kind::Person);
    summary.set_name(person.name.as_str());
    if let Some(url) = portrait_url(&person.wire_id, person.thumb_url.as_deref()) {
        let mut image = summary.reborrow().init_images(1).get(0);
        image.set_kind("Primary");
        image.set_url(url.as_str());
    }
}

fn credit_kind(kind: pharos_core::PersonKind) -> CreditKind {
    match kind {
        pharos_core::PersonKind::GuestStar => CreditKind::GuestStar,
        _ => CreditKind::Actor,
    }
}

/// `page` as the wire `Person`. `shows` holds, for each of `page.shows` in
/// order, the show's own projection; a show that has none (withheld, or the
/// store could not say) is named from its episodes instead.
fn write_person(
    out: person_result::Builder<'_>,
    id: WireId,
    page: &person::PersonPage,
    shows: &[Option<(WireId, serde_json::Value)>],
    server_id: &str,
    key: &ImageTagKey,
) {
    let mut out = out.init_person();
    set_item_id(out.reborrow().init_id(), id);
    out.set_name(page.person.name.as_str());
    if let Some(url) = portrait_url(&page.person.wire_id, page.person.thumb_url.as_deref()) {
        out.set_image_url(url.as_str());
    }
    if let Some(d) = &page.details {
        let text = |v: &Option<String>| v.clone().unwrap_or_default();
        out.set_biography(text(&d.biography).as_str());
        out.set_birth_date(text(&d.birth_date).as_str());
        out.set_death_date(text(&d.death_date).as_str());
        out.set_birth_place(text(&d.birth_place).as_str());
        out.set_imdb_id(text(&d.imdb_id).as_str());
        out.set_wikidata_id(text(&d.wikidata_id).as_str());
    }
    let mut films = out.reborrow().init_films(len(&page.films));
    for (i, film) in (0..).zip(&page.films) {
        write_appearance(films.reborrow().get(i), film, server_id, key);
    }
    let mut list = out.init_shows(len(&page.shows));
    for (i, show) in (0..).zip(&page.shows) {
        let mut w = list.reborrow().get(i);
        match shows.get(i as usize).and_then(Option::as_ref) {
            Some((series, dto)) => {
                write_synth_summary(w.reborrow().init_series(), *series, dto, key)
            }
            None => {
                let mut summary = w.reborrow().init_series();
                if let Some(series) = WireId::parse(&show.series_id) {
                    set_item_id(summary.reborrow().init_id(), series);
                }
                summary.set_kind(Kind::Series);
                summary.set_name(show.series_name.as_str());
            }
        }
        let mut characters = w.reborrow().init_characters(len(&show.characters));
        for (j, name) in (0..).zip(&show.characters) {
            characters.set(j, name.as_str());
        }
        let mut episodes = w.init_episodes(len(&show.episodes));
        for (j, episode) in (0..).zip(&show.episodes) {
            write_appearance(episodes.reborrow().get(j), episode, server_id, key);
        }
    }
}

fn write_appearance(
    mut out: person_credit::Builder<'_>,
    appearance: &person::Appearance,
    server_id: &str,
    key: &ImageTagKey,
) {
    write_item(
        out.reborrow().init_item(),
        &appearance.item,
        appearance.user_data,
        server_id,
        key,
    );
    out.set_character(appearance.character.as_deref().unwrap_or_default());
    out.set_kind(credit_kind(appearance.kind));
}

/// The text field `key` of a Jellyfin DTO, empty when absent.
fn dto_str<'a>(dto: &'a serde_json::Value, key: &str) -> &'a str {
    dto.get(key).and_then(|v| v.as_str()).unwrap_or_default()
}

fn synth_kind(dto: &serde_json::Value) -> Kind {
    use pharos_jellyfin_api::kinds::ItemType;
    match ItemType::from_wire(dto_str(dto, "Type")) {
        Some(ItemType::Series) => Kind::Series,
        Some(ItemType::Season) => Kind::Season,
        Some(ItemType::MusicAlbum) => Kind::Album,
        Some(ItemType::MusicArtist) => Kind::Artist,
        Some(ItemType::Playlist) => Kind::Playlist,
        Some(ItemType::CollectionFolder | ItemType::UserView) => Kind::Library,
        _ => Kind::Collection,
    }
}

/// A synthesised item's `ItemDetail` from its Jellyfin DTO: its summary,
/// overview and genres, whichever it has.
fn write_synth_detail(
    mut out: item_detail::Builder<'_>,
    id: WireId,
    dto: &serde_json::Value,
    key: &ImageTagKey,
) {
    write_synth_summary(out.reborrow().init_summary(), id, dto, key);
    out.set_overview(dto_str(dto, "Overview"));
    let genres: Vec<&str> = dto
        .get("Genres")
        .and_then(|g| g.as_array())
        .map(|g| g.iter().filter_map(|g| g.as_str()).collect())
        .unwrap_or_default();
    let count = u32::try_from(genres.len()).unwrap_or(u32::MAX);
    let mut list = out.init_genres(count);
    for (i, genre) in (0..count).zip(&genres) {
        list.set(i, genre);
    }
}

/// A synthesised item's `ItemSummary` from its Jellyfin DTO: name, kind,
/// year, Primary image, parentage and index, whichever it has. A season's
/// parent is its series (`SeriesId`: season DTOs carry no `ParentId`).
fn write_synth_summary(
    mut summary: item_summary::Builder<'_>,
    id: WireId,
    dto: &serde_json::Value,
    key: &ImageTagKey,
) {
    let kind = synth_kind(dto);
    set_item_id(summary.reborrow().init_id(), id);
    summary.set_kind(kind);
    summary.set_name(dto_str(dto, "Name"));
    if kind == Kind::Library {
        summary.set_library_kind(match dto_str(dto, "CollectionType") {
            "movies" => WireKind::Movies,
            "tvshows" => WireKind::TvShows,
            "music" => WireKind::Music,
            "books" => WireKind::Books,
            _ => WireKind::Mixed,
        });
    }
    if let Some(year) = dto
        .get("ProductionYear")
        .and_then(|y| y.as_u64())
        .and_then(|y| u16::try_from(y).ok())
    {
        summary.reborrow().init_year().set_some(year);
    }
    if let Some(tag) = dto.pointer("/ImageTags/Primary").and_then(|t| t.as_str()) {
        // ADR-0073: the same signed tag the Jellyfin routes hand out (V712).
        let id = dto_str(dto, "Id");
        let tag = key.sign(id, "primary", tag);
        let mut image = summary.reborrow().init_images(1).get(0);
        image.set_kind("Primary");
        image.set_tag(tag.as_str());
        image.set_url(format!("/Items/{id}/Images/Primary?tag={tag}").as_str());
    }
    if let Some(parent) =
        WireId::parse(dto_str(dto, "ParentId")).or_else(|| WireId::parse(dto_str(dto, "SeriesId")))
    {
        set_item_id(summary.reborrow().init_parent_id(), parent);
    }
    summary.set_series_name(dto_str(dto, "SeriesName"));
    if let Some(n) = dto
        .get("IndexNumber")
        .and_then(|n| n.as_u64())
        .and_then(|n| u32::try_from(n).ok())
    {
        summary.reborrow().init_index_number().set_some(n);
    }
    if let Some(score) = dto
        .get("CriticRating")
        .and_then(serde_json::Value::as_f64)
        .and_then(|r| tomatometer(r as f32))
    {
        summary.reborrow().init_critic_rating().set_some(score);
    }
    if let Some(tenths) = dto
        .get("PharosImdbRating")
        .and_then(serde_json::Value::as_f64)
        .and_then(|r| imdb_tenths(r as f32))
    {
        summary.init_imdb_rating().set_some(tenths);
    }
}

/// Why a call on stored item `id` has no item to answer with, as the denial
/// it sends, recorded. A hidden item answers as a missing one, so its id says
/// nothing about what exists.
pub(super) fn item_denial(id: u64, e: &DetailError, call: &'static str) -> Denial {
    wire_denial(WireId::Real(id), e, call)
}

/// [`item_denial`] for any wire id, stored or synthesised.
pub(super) fn wire_denial(id: WireId, e: &DetailError, call: &'static str) -> Denial {
    use crate::services::access::ItemHidden;
    let item_not_found = Denial::NotFound { what: "item" };
    let unavailable = Denial::Unavailable {
        dependency: "media store",
    };
    let (denial, reason, detail) = match e {
        DetailError::Hidden(ItemHidden::Unavailable(detail)) => {
            (unavailable, RefusalReason::StoreUnavailable, detail.clone())
        }
        DetailError::Hidden(hidden) => (
            item_not_found,
            RefusalReason::Hidden(hidden.label()),
            hidden.label().to_owned(),
        ),
        DetailError::NotFound => (
            item_not_found,
            RefusalReason::NotFound,
            "not_found".to_owned(),
        ),
        DetailError::Store(e) => (unavailable, RefusalReason::StoreUnavailable, e.to_string()),
    };
    record_refusal(call, id, reason, &detail);
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
pub(super) fn write_item(
    mut out: item_summary::Builder<'_>,
    item: &MediaItem,
    user_data: UserItemData,
    server_id: &str,
    key: &ImageTagKey,
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
        let tag = key.sign(&dto.id, "primary", tag);
        let mut image = out.reborrow().init_images(1).get(0);
        image.set_kind("Primary");
        image.set_tag(tag.as_str());
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
    out.set_stereo(wire_stereo(item_stereo(item)));
    if let Some(score) = dto.critic_rating.and_then(tomatometer) {
        out.reborrow().init_critic_rating().set_some(score);
    }
    if item.kind == MediaKind::Audio {
        if let Some(artist) = card_artist(&dto) {
            out.set_artist(artist.as_str());
        }
    }
    if let Some(tenths) = item.metadata.ratings.imdb.and_then(imdb_tenths) {
        out.init_imdb_rating().set_some(tenths);
    }
}

/// Who a song's card names under its title: its track artists, else its album
/// artist. Only a song's: a music video's file often carries an artist tag
/// too, but its card is a video's (V750). A compilation's album artist ("Various Artists") says less than
/// the track's own, so the track's come first (V750).
fn card_artist(dto: &BaseItemDto) -> Option<String> {
    if !dto.artists.is_empty() {
        return Some(dto.artists.join(", "));
    }
    dto.album_artists.first().map(|a| a.name.clone())
}

/// An IMDb score (0–10) in tenths; `None` outside 0–10.
fn imdb_tenths(score: f32) -> Option<u16> {
    let tenths = (score * 10.0).round();
    if !(0.0..=100.0).contains(&tenths) {
        tracing::debug!(imdb_rating = score, "IMDb rating outside 0-10 not sent");
        return None;
    }
    Some(tenths as u16)
}

/// A stored `CriticRating` as a whole Tomatometer percentage; `None` for a
/// value outside 0–100, which no critic score can be.
fn tomatometer(rating: f32) -> Option<u16> {
    let rounded = rating.round();
    if !(0.0..=100.0).contains(&rounded) {
        tracing::debug!(
            critic_rating = rating,
            "critic rating outside 0-100 not sent"
        );
        return None;
    }
    Some(rounded as u16)
}

fn write_view(
    mut item: item_summary::Builder<'_>,
    id: WireId,
    view: &LibraryView,
    key: &ImageTagKey,
) {
    set_item_id(item.reborrow().init_id(), id);
    item.set_kind(Kind::Library);
    item.set_name(view.name.as_str());
    item.set_library_kind(wire_library_kind(view.kind));
    if let Some(tag) = &view.primary_tag {
        let tag = key.sign(&view.id, "primary", tag);
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

fn len<T>(v: &[T]) -> u32 {
    u32::try_from(v.len()).unwrap_or(u32::MAX)
}

/// The first schemaVersion with `SubtitleDelivery.image`. An older client
/// fails to read the whole detail on that value, so it is sent `burn`, and
/// `Playback.negotiate` ignores the image codecs it declares.
pub(super) const IMAGE_DELIVERY_SCHEMA_VERSION: u32 = 16;

fn write_tracks(mut out: capnp::struct_list::Builder<'_, track::Owned>, tracks: &[watch::Track]) {
    for (i, t) in (0..).zip(tracks) {
        let mut w = out.reborrow().get(i);
        w.set_index(t.index);
        w.set_language(t.language.as_deref().unwrap_or_default());
        w.set_title(t.title.as_deref().unwrap_or_default());
        w.set_codec(t.codec.as_deref().unwrap_or_default());
        w.set_is_default(t.is_default);
        w.set_is_forced(t.is_forced);
        w.set_is_hearing_impaired(t.is_hearing_impaired);
        w.set_channels(
            t.channels
                .map_or(0, |c| u16::try_from(c).unwrap_or(u16::MAX)),
        );
        w.set_styled_url(t.styled.as_deref().unwrap_or_default());
        match &t.drawn {
            Some(watch::Drawn::Vtt(url)) => {
                w.set_delivery(SubtitleDelivery::External);
                w.set_url(url.as_str());
            }
            Some(watch::Drawn::Images(url)) => {
                w.set_delivery(SubtitleDelivery::Image);
                w.set_url(url.as_str());
            }
            None => w.set_delivery(SubtitleDelivery::Burn),
        }
    }
}

/// The wire kind of a `services::segments` type; `None` for one the schema
/// does not know, which a client could not act on.
fn segment_kind(kind: &str) -> Option<SegmentKind> {
    Some(match kind {
        "Intro" => SegmentKind::Intro,
        "Outro" => SegmentKind::Outro,
        "Recap" => SegmentKind::Recap,
        "Preview" => SegmentKind::Preview,
        "Commercial" => SegmentKind::Commercial,
        _ => return None,
    })
}

/// `item` as the file `version` names, or the denial that says why not.
fn select_version(
    route: media_source::Route,
    item: MediaItem,
    version: &str,
) -> Result<MediaItem, Denial> {
    crate::services::playback::select_version(route, item, version).map_err(version_denial)
}

pub(super) fn version_denial(e: crate::services::playback::VersionError) -> Denial {
    use crate::services::playback::VersionError;
    match e {
        VersionError::NoSuchVersion => Denial::NotFound { what: "version" },
        VersionError::Unprobed => Denial::Unavailable {
            dependency: "the version's probe (rescan the library)",
        },
    }
}

/// The packing a tile badges: the primary's, else the first 3D edition's,
/// so the badge agrees with the store's `has_3d` (V720).
fn item_stereo(item: &MediaItem) -> Option<Video3DFormat> {
    item.probe.video_3d_format.or_else(|| {
        item.probe
            .alternate_sources
            .iter()
            .find_map(|a| a.video_3d_format)
    })
}

fn wire_stereo(format: Option<Video3DFormat>) -> Stereo3d {
    match format {
        None => Stereo3d::None,
        Some(Video3DFormat::HalfSideBySide) => Stereo3d::HalfSideBySide,
        Some(Video3DFormat::FullSideBySide) => Stereo3d::FullSideBySide,
        Some(Video3DFormat::HalfTopAndBottom) => Stereo3d::HalfTopAndBottom,
        Some(Video3DFormat::FullTopAndBottom) => Stereo3d::FullTopAndBottom,
        Some(Video3DFormat::Mvc) => Stereo3d::Mvc,
    }
}

fn version_count(item: &MediaItem) -> u32 {
    u32::try_from(item.probe.alternate_sources.len())
        .unwrap_or(u32::MAX)
        .saturating_add(1)
}

/// An item's playable files, the primary first. An edition's id is what
/// `detail` and `Playback.negotiate` take as `version`. Paths go to admins
/// only.
fn write_versions(
    mut out: capnp::struct_list::Builder<'_, version::Owned>,
    item: &MediaItem,
    admin: bool,
) {
    let mut primary = out.reborrow().get(0);
    primary.set_primary(true);
    primary.set_stereo(wire_stereo(item.probe.video_3d_format));
    write_version_facts(primary, &item.probe, &item.path, admin);
    for (i, alt) in (1..out.len()).zip(&item.probe.alternate_sources) {
        let mut v = out.reborrow().get(i);
        v.set_id(alt.id.as_str());
        v.set_name(alt.name.as_deref().unwrap_or_default());
        v.set_stereo(wire_stereo(alt.video_3d_format));
        match alt.probe.as_deref() {
            Some(probe) => write_version_facts(v, probe, &alt.path, admin),
            // Probed before editions carried their own probe: only the flat facts.
            None => {
                let flat = MediaProbe {
                    video_codec: alt.video_codec.clone(),
                    audio_codec: alt.audio_codec.clone(),
                    size_bytes: alt.size_bytes,
                    container: alt.container.clone(),
                    ..MediaProbe::default()
                };
                write_version_facts(v, &flat, &alt.path, admin);
            }
        }
    }
}

fn write_version_facts(
    mut v: version::Builder<'_>,
    probe: &MediaProbe,
    path: &std::path::Path,
    admin: bool,
) {
    v.set_width(probe.width.unwrap_or_default());
    v.set_height(probe.height.unwrap_or_default());
    v.set_video_codec(probe.video_codec.as_deref().unwrap_or_default());
    v.set_hdr(match probe.video_range_type() {
        pharos_core::VideoRangeType::Sdr => "",
        other => other.label(),
    });
    v.set_size_bytes(probe.size_bytes.unwrap_or_default());
    v.set_container(probe.container.as_deref().unwrap_or_default());
    if admin {
        v.set_path(path.to_string_lossy().as_ref());
    }
    if probe.audio_tracks.is_empty() {
        // A single-stream probe records only the flat codec and channels.
        if let Some(codec) = probe.audio_codec.as_deref() {
            let mut a = v.init_audio(1).get(0);
            a.set_codec(codec);
            a.set_channels(probe.audio_channels.unwrap_or_default());
        }
        return;
    }
    let mut audio = v.init_audio(len(&probe.audio_tracks));
    for (i, track) in (0..).zip(&probe.audio_tracks) {
        let mut a = audio.reborrow().get(i);
        a.set_language(track.language.as_deref().unwrap_or_default());
        a.set_codec(track.codec.as_deref().unwrap_or_default());
        a.set_channels(track.channels.unwrap_or_default());
    }
}

#[cfg(test)]
mod tests {
    use super::{imdb_tenths, tomatometer, BrowseLevel, RefusalReason};
    use crate::services::access::ItemHidden;

    #[test]
    fn an_imdb_score_goes_out_in_tenths_within_0_to_10() {
        for (stored, sent) in [
            (8.7, Some(87)),
            (8.66, Some(87)),
            (10.0, Some(100)),
            (0.0, Some(0)),
            (10.06, None),
            (-0.1, None),
            (f32::NAN, None),
        ] {
            assert_eq!(imdb_tenths(stored), sent, "{stored}");
        }
    }

    #[test]
    fn a_critic_rating_rounds_to_a_whole_percentage_within_0_to_100() {
        for (stored, sent) in [
            (0.0, Some(0)),
            (-0.4, Some(0)),
            (83.4, Some(83)),
            (83.5, Some(84)),
            (100.0, Some(100)),
            (100.4, Some(100)),
            (100.6, None),
            (-0.6, None),
            (f32::NAN, None),
            (f32::INFINITY, None),
        ] {
            assert_eq!(tomatometer(stored), sent, "{stored}");
        }
    }

    /// A renamed or merged label breaks dashboards silently.
    #[test]
    fn refusal_reason_labels_are_distinct() {
        let hidden = [
            ItemHidden::LibraryNotAllowed,
            ItemHidden::NoLibrary,
            ItemHidden::ParentalRating,
            ItemHidden::Tag,
            ItemHidden::Unauthenticated,
        ];
        let mut labels: Vec<&str> = hidden
            .iter()
            .map(|h| RefusalReason::Hidden(h.label()).label())
            .collect();
        labels.extend(
            [
                RefusalReason::NotFound,
                RefusalReason::StoreUnavailable,
                RefusalReason::SynthHidden,
            ]
            .map(RefusalReason::label),
        );
        let distinct: std::collections::HashSet<_> = labels.iter().collect();
        assert_eq!(distinct.len(), labels.len(), "{labels:?}");
    }

    /// A renamed or merged label breaks dashboards silently.
    #[test]
    fn browse_level_labels_are_distinct() {
        let labels = [
            BrowseLevel::Library,
            BrowseLevel::Series,
            BrowseLevel::Season,
            BrowseLevel::Other,
        ]
        .map(BrowseLevel::label);
        let distinct: std::collections::HashSet<_> = labels.iter().collect();
        assert_eq!(distinct.len(), labels.len(), "{labels:?}");
    }
}
