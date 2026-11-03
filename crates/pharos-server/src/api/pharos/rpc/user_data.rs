//! `UserData`: a signed-in user's played and favourite state. Each method
//! calls one `services::user_data` function and encodes its answer (US5).

use std::rc::Rc;

use pharos_api::pharos_capnp::{user_data as wire, user_state_result};
use pharos_api::{server, Denial};
use pharos_core::UserItemData;
use pharos_jellyfin_api::dto::WireId;

use super::library::{item_denial, read_item_id, wire_denial, write_user_state};
use super::session::Session;
use crate::services::user_data;

pub(super) struct UserData {
    session: Rc<Session>,
}

impl UserData {
    pub(super) fn new(session: Rc<Session>) -> Self {
        Self { session }
    }
}

impl server::user_data::Handler for UserData {
    async fn set_played(
        self: Rc<Self>,
        params: wire::SetPlayedParams,
        results: &mut wire::SetPlayedResults,
    ) -> Result<(), capnp::Error> {
        let params = params.get()?;
        let (id, played) = (read_item_id(params.get_id()?)?, params.get_played());
        let written = match (self.session.current_user().await, id) {
            (Err(denial), _) => Err(denial),
            (Ok(user), WireId::Real(id)) => {
                user_data::set_played(&self.session.state, &user, id, played)
                    .await
                    .map(|(_, data)| data)
                    .map_err(|e| item_denial(id, &e, "set_played"))
            }
            // A series or season: every episode in it the user may see.
            (Ok(user), synth @ WireId::Synth(_)) => {
                user_data::update_folder(&self.session.state, &user, &synth.to_string(), |d| {
                    user_data::apply_played(d, played)
                })
                .await
                .map_err(|e| wire_denial(synth, &e, "set_played"))
            }
        };
        write_result(results.get().init_result(), written);
        Ok(())
    }

    async fn set_favorite(
        self: Rc<Self>,
        params: wire::SetFavoriteParams,
        results: &mut wire::SetFavoriteResults,
    ) -> Result<(), capnp::Error> {
        let params = params.get()?;
        let (id, favorite) = (read_item_id(params.get_id()?)?, params.get_favorite());
        let written = match (self.session.current_user().await, id) {
            (Err(denial), _) => Err(denial),
            (Ok(user), WireId::Real(id)) => {
                user_data::set_favorite(&self.session.state, &user, id, favorite)
                    .await
                    .map(|(_, data)| data)
                    .map_err(|e| item_denial(id, &e, "set_favorite"))
            }
            (Ok(_), WireId::Synth(_)) => Err(synth_denial()),
        };
        write_result(results.get().init_result(), written);
        Ok(())
    }
}

/// Synthesised items (series, season, album…) have no native favourite yet.
fn synth_denial() -> Denial {
    Denial::NotFound { what: "item" }
}

fn write_result(out: user_state_result::Builder<'_>, written: Result<UserItemData, Denial>) {
    match written {
        Ok(data) => write_user_state(out.init_state(), data),
        Err(denial) => denial.write(out.init_error()),
    }
}
