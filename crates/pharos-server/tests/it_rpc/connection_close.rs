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
    // Barrier: every call has reached its handler on the server. The server
    // shares this thread, so once the count is reached and the test yields,
    // each handler has run to its first await: the saturated pool.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while conn.in_flight() < max + 1 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "only {} of {} calls reached the server",
            conn.in_flight(),
            max + 1
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
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

/// The same through the real `/pharos/rpc` WebSocket: the peer goes away
/// (the tab closes) while its calls are queued, and the socket's own close
/// path must release them.
#[actix_web::test]
async fn a_websocket_peer_closing_mid_call_gives_the_pool_back() {
    use actix_web::App;
    use awc::ws;
    use futures_util::{SinkExt as _, StreamExt as _};
    use pharos_server::config::NativeApiConfig;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let fx = seed_rich().await;
    let pool = sqlite_of(&fx.state.stores).pool().clone();
    let max = pool.options().get_max_connections();
    let state = fx.state.clone();
    let limits = rpc::limits::ConnectionLimits::default();
    let server = actix_test::start(move || {
        let cfg = NativeApiConfig {
            enabled: true,
            ..NativeApiConfig::default()
        };
        App::new()
            .app_data(state.clone())
            .configure(rpc::configure(cfg, limits.clone()))
    });

    let (_resp, framed) = awc::Client::new()
        .ws(crate::connect::ws_url(&server))
        .connect()
        .await
        .expect("upgrade");
    let (mut sink, mut stream) = framed.split();
    let (rpc_end, pump_end) = tokio::io::duplex(64 * 1024);
    let (mut from_rpc, mut to_rpc) = tokio::io::split(pump_end);
    let outbound = actix_web::rt::spawn(async move {
        let mut buf = vec![0u8; 64 * 1024];
        while let Ok(n) = from_rpc.read(&mut buf).await {
            if n == 0
                || sink
                    .send(ws::Message::Binary(buf[..n].to_vec().into()))
                    .await
                    .is_err()
            {
                break;
            }
        }
        sink
    });
    let inbound = actix_web::rt::spawn(async move {
        while let Some(Ok(frame)) = stream.next().await {
            if let ws::Frame::Binary(bytes) = frame {
                if to_rpc.write_all(&bytes).await.is_err() {
                    break;
                }
            }
        }
    });
    let (r, w) = tokio::io::split(rpc_end);
    let network = twoparty::VatNetwork::new(
        r.compat(),
        w.compat_write(),
        Side::Client,
        Default::default(),
    );
    let mut system = RpcSystem::new(Box::new(network), None);
    let client: bootstrap::Client = system.bootstrap(Side::Server);
    let client_rpc = actix_web::rt::spawn(system);

    let mut held = Vec::new();
    for _ in 0..max {
        held.push(pool.acquire().await.unwrap());
    }
    // As many parked calls as one connection may run, more than the pool
    // has slots.
    let mut calls = Vec::new();
    for _ in 0..pharos_api::instrument::MAX_CALLS_IN_FLIGHT {
        let mut req = client.resume_request();
        req.get().init_ctx();
        req.get().set_device_token(&fx.user_token);
        calls.push(actix_web::rt::spawn(req.send().promise));
    }
    // Barrier, read from outside the server: a call past the cap is refused
    // with `limit` only once every parked call has reached its handler.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !refused_at_the_cap(&client).await {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the parked calls never all reached the server"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        calls.iter().all(|c| !c.is_finished()),
        "the calls must be parked on the pool when the peer goes"
    );

    // The tab closes: the socket goes away with nothing more sent.
    outbound.abort();
    inbound.abort();
    client_rpc.abort();
    for c in &calls {
        c.abort();
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    drop(held);
    tokio::time::sleep(Duration::from_millis(100)).await;

    let got = tokio::time::timeout(Duration::from_secs(2), pool.acquire()).await;
    assert!(
        matches!(got, Ok(Ok(_))),
        "pool wedged after the peer closed: size={} idle={}",
        pool.size(),
        pool.num_idle()
    );
}

/// Whether a `serverInfo` call is refused for the connection's
/// calls-in-flight cap.
async fn refused_at_the_cap(client: &bootstrap::Client) -> bool {
    use pharos_api::pharos_capnp::{error, server_info_result};
    let mut req = client.server_info_request();
    req.get().init_ctx();
    let Ok(reply) = req.send().promise.await else {
        return false;
    };
    let Ok(result) = reply.get().and_then(|r| r.get_result()) else {
        return false;
    };
    let Ok(server_info_result::Error(Ok(err))) = result.which() else {
        return false;
    };
    matches!(
        err.get_kind().which(),
        Ok(error::kind::Limit(Ok(limit)))
            if limit.get_name().ok().and_then(|n| n.to_str().ok()) == Some("calls_in_flight")
    )
}
