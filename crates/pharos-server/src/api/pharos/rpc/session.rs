//! `Session`: a signed-in user's root capability.

use std::rc::Rc;

use actix_web::web;
use pharos_api::pharos_capnp::session as wire;
use pharos_api::server;
use pharos_core::User;

use crate::state::AppState;

pub(super) struct Session {
    // Read by the capability getters as they land (phase 4).
    #[allow(dead_code)]
    state: web::Data<AppState>,
    user: User,
}

impl Session {
    pub(super) fn new(state: web::Data<AppState>, user: User) -> Self {
        Self { state, user }
    }
}

impl server::session::Handler for Session {
    async fn me(
        self: Rc<Self>,
        _params: wire::MeParams,
        results: &mut wire::MeResults,
    ) -> Result<(), capnp::Error> {
        let mut me = results.get().init_result().init_me();
        me.reborrow().init_id().set_uuid(self.user.id.0.as_bytes());
        me.set_name(self.user.name.as_str());
        me.set_is_admin(self.user.policy.admin);
        Ok(())
    }
}
