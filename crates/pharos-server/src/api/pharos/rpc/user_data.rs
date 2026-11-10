//! `UserData`: a signed-in user's played and favourite state. Each method
//! calls one `services::user_data` function and encodes its answer (US5).

use std::rc::Rc;

use pharos_api::pharos_capnp::{user_data as wire, user_state_result};
use pharos_api::{server, Denial};
use pharos_core::{ReadPosition, UserItemData};
use pharos_jellyfin_api::dto::WireId;

use super::library::{item_denial, read_item_id, wire_denial, write_read_state, write_user_state};
use super::session::Session;
use crate::services::user_data::{self, Mark, ReadError};

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
            (Ok(user), synth @ WireId::Synth(_)) => user_data::set_folder(
                &self.session.state,
                &user,
                &synth.to_string(),
                Mark::Played(played),
            )
            .await
            .map_err(|e| wire_denial(synth, &e, "set_played")),
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
            (Ok(user), synth @ WireId::Synth(_)) => user_data::set_folder(
                &self.session.state,
                &user,
                &synth.to_string(),
                Mark::Favorite(favorite),
            )
            .await
            .map_err(|e| wire_denial(synth, &e, "set_favorite")),
        };
        write_result(results.get().init_result(), written);
        Ok(())
    }

    async fn set_read_position(
        self: Rc<Self>,
        params: wire::SetReadPositionParams,
        results: &mut wire::SetReadPositionResults,
    ) -> Result<(), capnp::Error> {
        let params = params.get()?;
        let id = read_item_id(params.get_id()?)?;
        let locator = params.get_locator()?.to_str()?;
        let fraction = params.get_fraction();
        let read_at_ms = params.get_read_at();
        let written = match (self.session.current_user().await, id) {
            (Err(denial), _) => Err(denial),
            (Ok(user), WireId::Real(id)) => user_data::set_read_position(
                &self.session.state,
                &user,
                id,
                locator,
                fraction,
                read_at_ms,
            )
            .await
            .map(|(_, data, position, _)| (data, Some(position)))
            .map_err(|e| match e {
                ReadError::Refused(refusal) => Denial::Invalid {
                    field: refusal.field(),
                },
                ReadError::Item(e) => item_denial(id, &e, "set_read_position"),
            }),
            // A series, season or album is never a book.
            (Ok(_), synth @ WireId::Synth(_)) => {
                tracing::info!(item.id = %synth, reason = "not_book", "read position refused, nothing stored");
                metrics::counter!("pharos_read_position_total", "outcome" => "not_book")
                    .increment(1);
                Err(Denial::Invalid { field: "id" })
            }
        };
        let out = results.get().init_result();
        match written {
            Ok((data, position)) => write_state(out.init_state(), data, position.as_ref()),
            Err(denial) => denial.write(out.init_error()),
        }
        Ok(())
    }
}

fn write_state(
    mut out: pharos_api::pharos_capnp::user_state::Builder<'_>,
    data: UserItemData,
    position: Option<&ReadPosition>,
) {
    write_user_state(out.reborrow(), data);
    if let Some(position) = position {
        write_read_state(out, position);
    }
}

fn write_result(out: user_state_result::Builder<'_>, written: Result<UserItemData, Denial>) {
    match written {
        Ok(data) => write_user_state(out.init_state(), data),
        Err(denial) => denial.write(out.init_error()),
    }
}
