//! T023/T024: `Bootstrap.serverInfo` and `resume` → `Session.me`, over the
//! upgrade cookie or an explicit device token; a bad token yields a session
//! whose every call fails `revoked` (research R1).

use std::rc::Rc;

use pharos_api::pharos_capnp::{bootstrap, done, error, me_result, server_info_result};
use pharos_api::Connection;
use pharos_server::api::pharos::rpc;

use crate::common::seed_rich;

fn me_id(
    reply: &capnp::capability::Response<pharos_api::pharos_capnp::session::me_results::Owned>,
) -> Vec<u8> {
    match reply.get().unwrap().get_result().unwrap().which().unwrap() {
        me_result::Me(me) => me.unwrap().get_id().unwrap().get_uuid().unwrap().to_vec(),
        me_result::Error(e) => panic!("me failed: {:?}", e.unwrap().get_reason()),
    }
}

async fn resume(
    client: &bootstrap::Client,
    token: &str,
) -> pharos_api::pharos_capnp::session::Client {
    let mut req = client.resume_request();
    req.get().init_ctx();
    req.get().set_device_token(token);
    req.send().pipeline.get_result().get_session()
}

#[actix_web::test]
async fn server_info_reports_name_version_and_schema_version() {
    let fx = seed_rich().await;
    let conn = Rc::new(Connection::new());
    let client = rpc::bootstrap_client(fx.state.clone(), None, &conn);
    let mut req = client.server_info_request();
    req.get().init_ctx();
    let reply = req.send().promise.await.unwrap();
    let server_info_result::Info(info) =
        reply.get().unwrap().get_result().unwrap().which().unwrap()
    else {
        panic!("serverInfo failed");
    };
    let info = info.unwrap();
    assert_eq!(
        info.get_schema_version(),
        pharos_api::pharos_capnp::SCHEMA_VERSION
    );
    assert_eq!(
        info.get_name().unwrap().to_str().unwrap(),
        fx.state.server_name
    );
    assert!(!info.get_version().unwrap().to_str().unwrap().is_empty());
}

#[actix_web::test]
async fn resume_with_a_device_token_reaches_me() {
    let fx = seed_rich().await;
    let conn = Rc::new(Connection::new());
    let client = rpc::bootstrap_client(fx.state.clone(), None, &conn);
    let session = resume(&client, &fx.user_token).await;
    let mut me = session.me_request();
    me.get().init_ctx();
    let reply = me.send().promise.await.unwrap();
    assert_eq!(me_id(&reply), fx.user_id.0.as_bytes().to_vec());
}

#[actix_web::test]
async fn resume_with_an_empty_token_uses_the_upgrade_cookie() {
    let fx = seed_rich().await;
    let conn = Rc::new(Connection::new());
    let client = rpc::bootstrap_client(fx.state.clone(), Some(fx.admin_token.clone()), &conn);
    let session = resume(&client, "").await;
    let mut me = session.me_request();
    me.get().init_ctx();
    let reply = me.send().promise.await.unwrap();
    assert_eq!(me_id(&reply), fx.admin_id.0.as_bytes().to_vec());
}

#[actix_web::test]
async fn a_bad_token_yields_a_revoked_session_and_says_why() {
    let fx = seed_rich().await;
    let conn = Rc::new(Connection::new());
    let client = rpc::bootstrap_client(fx.state.clone(), None, &conn);

    let mut req = client.resume_request();
    req.get().init_ctx();
    req.get().set_device_token("not-a-token");
    let promise = req.send();
    // Pipelined before the resume resolves: one flight even on refusal.
    let mut me = promise.pipeline.get_result().get_session().me_request();
    me.get().init_ctx();
    let me_reply = me.send().promise.await.unwrap();

    let reply = promise.promise.await.unwrap();
    let status = reply
        .get()
        .unwrap()
        .get_result()
        .unwrap()
        .get_status()
        .unwrap();
    let done::Error(err) = status.which().unwrap() else {
        panic!("a refused resume must say so in status");
    };
    assert!(matches!(
        err.unwrap().get_kind().which().unwrap(),
        error::kind::Revoked(())
    ));

    let me_result::Error(err) = me_reply
        .get()
        .unwrap()
        .get_result()
        .unwrap()
        .which()
        .unwrap()
    else {
        panic!("me on a refused session must fail");
    };
    assert!(matches!(
        err.unwrap().get_kind().which().unwrap(),
        error::kind::Revoked(())
    ));
}

#[actix_web::test]
async fn no_cookie_and_no_token_is_refused() {
    let fx = seed_rich().await;
    let conn = Rc::new(Connection::new());
    let client = rpc::bootstrap_client(fx.state.clone(), None, &conn);
    let mut req = client.resume_request();
    req.get().init_ctx();
    let reply = req.send().promise.await.unwrap();
    let status = reply
        .get()
        .unwrap()
        .get_result()
        .unwrap()
        .get_status()
        .unwrap();
    assert!(matches!(status.which().unwrap(), done::Error(_)));
}

/// The same dispatch served over a byte stream, as the WebSocket adapter and
/// native TCP clients use it.
#[actix_web::test]
async fn served_over_a_byte_stream() {
    use capnp_rpc::{rpc_twoparty_capnp::Side, twoparty, RpcSystem};
    use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

    let fx = seed_rich().await;
    let (server_end, client_end) = tokio::io::duplex(64 * 1024);
    let conn = Rc::new(Connection::new());
    let bootstrap = rpc::bootstrap_client(fx.state.clone(), None, &conn);
    let (sr, sw) = tokio::io::split(server_end);
    actix_web::rt::spawn(rpc::serve(sr, sw, bootstrap));

    let (cr, cw) = tokio::io::split(client_end);
    let network = twoparty::VatNetwork::new(
        cr.compat(),
        cw.compat_write(),
        Side::Client,
        Default::default(),
    );
    let mut system = RpcSystem::new(Box::new(network), None);
    let client: bootstrap::Client = system.bootstrap(Side::Server);
    actix_web::rt::spawn(async move {
        let _ = system.await;
    });
    let mut req = client.server_info_request();
    req.get().init_ctx();
    let reply = req.send().promise.await.unwrap();
    assert!(matches!(
        reply.get().unwrap().get_result().unwrap().which().unwrap(),
        server_info_result::Info(_)
    ));
}
