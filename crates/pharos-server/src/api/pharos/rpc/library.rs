//! `Library`: what a signed-in user may browse. Each method calls one
//! `services::library` function and encodes its answer (US4).

use std::rc::Rc;

use pharos_api::pharos_capnp::{item_summary, library as wire, Kind, LibraryKind as WireKind};
use pharos_api::{server, Denial};
use pharos_core::{LibraryKind, User};
use pharos_jellyfin_api::dto::WireId;

use super::bootstrap::token_denial;
use super::session::Session;
use crate::services::auth::{user_for_token, TokenRefusal};
use crate::services::library::{self, LibraryView};

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
}

fn write_view(mut item: item_summary::Builder<'_>, id: WireId, view: &LibraryView) {
    match id {
        WireId::Real(id) => item.reborrow().init_id().set_real(id),
        WireId::Synth(id) => item.reborrow().init_id().set_synth(id),
    }
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
