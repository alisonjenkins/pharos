//! `Group`: a native client listing, creating and joining the same SyncPlay
//! groups jellyfin-web uses (spec 014). The groups, access rules and queue
//! checks are the Jellyfin handlers' own (`api::jellyfin::syncplay`).

use std::rc::Rc;
use std::time::{SystemTime, UNIX_EPOCH};

use pharos_api::pharos_capnp::{group as wire, group_summary};
use pharos_api::{server, Denial};
use pharos_jellyfin_api::dto::WireId;

use super::library::set_item_id;
use super::session::Session;
use crate::api::jellyfin::syncplay::{
    listed_groups, sync_play_allows, GroupHiddenRoute, ListedGroup, SyncPlayNeed,
};
use crate::group_ops::SyncDeps;

pub(super) struct Group {
    session: Rc<Session>,
}

impl Group {
    pub(super) fn new(session: Rc<Session>) -> Self {
        Self { session }
    }

    /// The session's current user and the running SyncPlay machinery, or why
    /// neither is available.
    async fn open(&self) -> Result<(pharos_core::User, &SyncDeps), Denial> {
        let user = self.session.current_user().await?;
        let deps = self.session.state.sync.get().ok_or(Denial::Unavailable {
            dependency: "syncplay",
        })?;
        Ok((user, deps))
    }
}

/// Server wall-clock, unix ms: the base `atServerMs` is expressed in.
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

fn write_summary(mut out: group_summary::Builder<'_>, group: &ListedGroup) {
    out.reborrow().init_id().set_value(group.id.as_str());
    out.set_name(group.name.as_str());
    if let Some(WireId::Real(id)) = group.item_id.as_deref().and_then(WireId::parse) {
        set_item_id(out.reborrow().init_item(), WireId::Real(id));
    }
    let mut members = out.init_members(u32::try_from(group.participants.len()).unwrap_or(u32::MAX));
    for (i, name) in (0u32..).zip(&group.participants) {
        members.reborrow().get(i).set_name(name.as_str());
    }
}

impl server::group::Handler for Group {
    async fn list(
        self: Rc<Self>,
        _params: wire::ListParams,
        results: &mut wire::ListResults,
    ) -> Result<(), capnp::Error> {
        let out = results.get().init_result();
        let listed = async {
            let (user, deps) = self.open().await?;
            if !sync_play_allows(&user, SyncPlayNeed::Join, "rpc_list") {
                return Err(Denial::NotPermitted { what: "syncplay" });
            }
            Ok(listed_groups(
                &self.session.state,
                &deps.registry,
                &user,
                GroupHiddenRoute::RpcList,
            )
            .await)
        }
        .await;
        match listed {
            Ok(groups) => {
                let mut list = out.init_groups(u32::try_from(groups.len()).unwrap_or(u32::MAX));
                for (i, group) in (0u32..).zip(&groups) {
                    write_summary(list.reborrow().get(i), group);
                }
            }
            Err(denial) => denial.write(out.init_error()),
        }
        Ok(())
    }

    async fn create(
        self: Rc<Self>,
        _params: wire::CreateParams,
        _results: &mut wire::CreateResults,
    ) -> Result<(), capnp::Error> {
        Err(capnp::Error::unimplemented(
            "Group.create is not served yet".to_string(),
        ))
    }

    async fn join(
        self: Rc<Self>,
        _params: wire::JoinParams,
        _results: &mut wire::JoinResults,
    ) -> Result<(), capnp::Error> {
        Err(capnp::Error::unimplemented(
            "Group.join is not served yet".to_string(),
        ))
    }

    async fn clock(
        self: Rc<Self>,
        params: wire::ClockParams,
        results: &mut wire::ClockResults,
    ) -> Result<(), capnp::Error> {
        let client_ms = params.get()?.get_client_ms();
        let recv = now_ms();
        let out = results.get().init_result();
        match self.session.current_user().await {
            Ok(_) => {
                let mut sample = out.init_sample();
                sample.set_client_ms_echo(client_ms);
                sample.set_recv_ms(recv);
                sample.set_send_ms(now_ms());
            }
            Err(denial) => denial.write(out.init_error()),
        }
        Ok(())
    }
}
