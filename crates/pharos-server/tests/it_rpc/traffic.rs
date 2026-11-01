//! T027: `pharos_rpc_bytes_total{direction}` counts the bytes `rpc::serve`
//! reads and writes. Driven over an in-memory pipe on the test thread, so
//! the thread-local recorder sees the real path. Disarm check: with `serve`
//! given the raw reader and writer, both counts read 0.

use capnp_rpc::{rpc_twoparty_capnp::Side, twoparty, RpcSystem};
use metrics_util::debugging::{DebugValue, DebuggingRecorder};
use pharos_api::pharos_capnp::bootstrap;
use pharos_api::Connection;
use pharos_server::api::pharos::rpc;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use crate::bootstrap::LOCAL;
use crate::common::seed_rich;

#[actix_web::test]
async fn bytes_in_and_out_are_counted() {
    let fx = seed_rich().await;
    let recorder = DebuggingRecorder::new();
    let snapshotter = pharos_test_metrics::snapshotter(&recorder);
    let _metrics = metrics::set_default_local_recorder(&recorder);

    let (server_end, client_end) = tokio::io::duplex(64 * 1024);
    let (sr, sw) = tokio::io::split(server_end);
    let ctx = rpc::ConnectionCtx::admit(None, LOCAL, &rpc::limits::ConnectionLimits::default(), 16)
        .unwrap();
    let boot = rpc::bootstrap_client(fx.state.clone(), ctx, &Connection::new());
    let local = tokio::task::LocalSet::new();
    local.spawn_local(rpc::serve(sr, sw, boot));

    let (cr, cw) = tokio::io::split(client_end);
    let network = twoparty::VatNetwork::new(
        cr.compat(),
        cw.compat_write(),
        Side::Client,
        Default::default(),
    );
    let mut system = RpcSystem::new(Box::new(network), None);
    let client: bootstrap::Client = system.bootstrap(Side::Server);
    local.spawn_local(system);
    local
        .run_until(async move {
            let mut req = client.server_info_request();
            req.get().init_ctx();
            req.send().promise.await.expect("serverInfo answers");
        })
        .await;

    let bytes = |direction: &str| -> u64 {
        snapshotter
            .snapshot()
            .into_vec()
            .into_iter()
            .filter(|(k, ..)| {
                let key = k.key();
                key.name() == "pharos_rpc_bytes_total"
                    && key
                        .labels()
                        .any(|l| l.key() == "direction" && l.value() == direction)
            })
            .map(|(.., v)| match v {
                DebugValue::Counter(n) => n,
                other => panic!("{other:?}"),
            })
            .sum()
    };
    assert!(bytes("in") > 0, "the request was read");
    assert!(bytes("out") > 0, "the reply was written");
}
