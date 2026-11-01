//! 017 research R1 (decided 2026-11-01): `Bootstrap.resume` returns its
//! session as a plain field, so `resume → session.<call>` pipelines in one
//! flight, and a refused resume is still observed by its `status`.

use std::rc::Rc;

use capnp_rpc::rpc_twoparty_capnp::Side;
use capnp_rpc::{twoparty, RpcSystem};
use pharos_api::pharos_capnp::{bootstrap, me_result, session};
use pharos_api::server;
use pharos_api::Connection;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use crate::support::{block_on, fill_ctx, observe};

struct Me;

impl server::session::Handler for Me {
    async fn me(
        self: Rc<Self>,
        _params: session::MeParams,
        results: &mut session::MeResults,
    ) -> Result<(), capnp::Error> {
        results.get().init_result().init_me().set_name("alice");
        Ok(())
    }
}

/// Accepts the token "good"; refuses anything else with `revoked`.
struct Boot(Rc<Connection>);

impl server::bootstrap::Handler for Boot {
    async fn resume(
        self: Rc<Self>,
        params: bootstrap::ResumeParams,
        results: &mut bootstrap::ResumeResults,
    ) -> Result<(), capnp::Error> {
        let token = params.get()?.get_device_token()?.to_str()?;
        let mut result = results.get().init_result();
        if token == "good" {
            result.set_session(server::session::client(Me, &self.0));
            result.init_status().set_ok(());
        } else {
            let mut err = result.init_status().init_error();
            err.set_reason("token not recognised");
            err.get_kind().set_revoked(());
        }
        Ok(())
    }
}

#[test]
fn session_calls_pipeline_through_an_unresolved_resume() {
    let name = block_on(async {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (client_io, server_io) = tokio::io::duplex(64 * 1024);
                let (sr, sw) = tokio::io::split(server_io);
                let conn = Connection::new();
                let cap = server::bootstrap::client(Boot(Rc::clone(&conn)), &conn);
                let server_net = twoparty::VatNetwork::new(
                    sr.compat(),
                    sw.compat_write(),
                    Side::Server,
                    Default::default(),
                );
                tokio::task::spawn_local(RpcSystem::new(Box::new(server_net), Some(cap.client)));

                let (cr, cw) = tokio::io::split(client_io);
                let client_net = twoparty::VatNetwork::new(
                    cr.compat(),
                    cw.compat_write(),
                    Side::Client,
                    Default::default(),
                );
                let mut rpc = RpcSystem::new(Box::new(client_net), None);
                let boot: bootstrap::Client = rpc.bootstrap(Side::Server);
                tokio::task::spawn_local(rpc);

                let mut resume = boot.resume_request();
                fill_ctx(resume.get().init_ctx());
                resume.get().set_device_token("good");
                // No await between resume and me: `me` is sent against the
                // promised session (the one-flight shape SC-001 needs).
                let session = resume.send().pipeline.get_result().get_session();
                let mut me = session.me_request();
                fill_ctx(me.get().init_ctx());
                let reply = me.send().promise.await?;
                match reply.get()?.get_result()?.which()? {
                    me_result::Me(m) => Ok::<_, capnp::Error>(m?.get_name()?.to_string()?),
                    me_result::Error(_) => Ok("error".to_owned()),
                }
            })
            .await
    });
    assert_eq!(name.unwrap(), "alice");
}

#[test]
fn a_refused_resume_is_counted_by_its_status() {
    let (result, o) = observe("info", || {
        let conn = Connection::new();
        let boot = server::bootstrap::client(Boot(Rc::clone(&conn)), &conn);
        block_on(async move {
            let mut resume = boot.resume_request();
            fill_ctx(resume.get().init_ctx());
            resume.get().set_device_token("stale");
            resume.send().promise.await.map(|_| ())
        })
    });
    result.unwrap();
    assert_eq!(
        o.counter(
            "pharos_rpc_calls_total",
            &[
                ("interface", "bootstrap"),
                ("method", "resume"),
                ("outcome", "revoked")
            ],
        ),
        Some(1),
        "logs:\n{}",
        o.logs
    );
    let line = o.event("rpc call failed").expect("failure line");
    assert_eq!(line["fields"]["reason"], "token not recognised");
}
