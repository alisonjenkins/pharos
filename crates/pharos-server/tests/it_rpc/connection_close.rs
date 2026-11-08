//! B730 / V680: a native connection that ends while its calls wait on the
//! database must not keep the pool. Dropping the served future used to leak
//! the calls still running (capnp-rpc's answer-table cycle), and each leaked
//! call kept the pool slot it was queued for: eight of them wedged the sqlite
//! pool for good, with every connection idle.

use std::rc::Rc;
use std::time::Duration;

use capnp_rpc::{rpc_twoparty_capnp::Side, twoparty, RpcSystem};
use pharos_api::pharos_capnp::bootstrap;
use pharos_api::Connection;
use pharos_server::api::pharos::rpc;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use crate::bootstrap::LOCAL;
use crate::common::seed_rich;

fn sqlite_of(stores: &pharos_server::state::Stores) -> &pharos_store_sqlx::sqlite::SqliteStore {
    #[cfg(feature = "postgres")]
    let sqlite = match stores {
        pharos_store_sqlx::any::AnyStore::Sqlite(s) => s,
        pharos_store_sqlx::any::AnyStore::Postgres(_) => panic!("the fixture is sqlite"),
    };
    #[cfg(not(feature = "postgres"))]
    let sqlite = stores;
    sqlite
}

#[actix_web::test]
async fn a_connection_closed_mid_call_gives_the_pool_back() {
    let fx = seed_rich().await;
    let pool = sqlite_of(&fx.state.stores).pool().clone();
    let max = pool.options().get_max_connections();

    // Saturate the pool, so the calls below queue for a slot.
    let mut held = Vec::new();
    for _ in 0..max {
        held.push(pool.acquire().await.unwrap());
    }

    let (server_end, client_end) = tokio::io::duplex(64 * 1024);
    let conn = Rc::new(Connection::new());
    let ctx = rpc::ConnectionCtx::admit(None, LOCAL, &rpc::limits::ConnectionLimits::default(), 16)
        .unwrap();
    let boot = rpc::bootstrap_client(fx.state.clone(), ctx, &conn);
    let (sr, sw) = tokio::io::split(server_end);
    let served = actix_web::rt::spawn(rpc::serve(sr, sw, boot));

    let (cr, cw) = tokio::io::split(client_end);
    let network = twoparty::VatNetwork::new(
        cr.compat(),
        cw.compat_write(),
        Side::Client,
        Default::default(),
    );
    let mut system = RpcSystem::new(Box::new(network), None);
    let client: bootstrap::Client = system.bootstrap(Side::Server);
    let _client_rpc = actix_web::rt::spawn(system);

    // One more caller than the pool has slots, each resuming with a token
    // the server must look up.
    let mut calls = Vec::new();
    for _ in 0..=max {
        let mut req = client.resume_request();
        req.get().init_ctx();
        req.get().set_device_token(&fx.user_token);
        calls.push(actix_web::rt::spawn(req.send().promise));
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        calls.iter().all(|c| !c.is_finished()),
        "the calls must be parked on the pool when the connection ends"
    );

    // The socket side ends first, as when a tab closes: the served future
    // is dropped with the calls still queued.
    served.abort();
    tokio::time::sleep(Duration::from_millis(50)).await;
    drop(held);
    tokio::time::sleep(Duration::from_millis(100)).await;

    let got = tokio::time::timeout(Duration::from_secs(2), pool.acquire()).await;
    assert!(
        matches!(got, Ok(Ok(_))),
        "pool wedged after the connection closed: size={} idle={}",
        pool.size(),
        pool.num_idle()
    );
}
