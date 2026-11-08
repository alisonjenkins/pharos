//! An `RpcSystem` that disconnects when it is dropped (B730, V680).
//!
//! capnp-rpc keeps each incoming call's future in the connection's answer
//! table, and that future holds the connection's state through its `Results`:
//! a reference cycle. Only `ConnectionState::disconnect` breaks it, and that
//! runs when the `RpcSystem` itself sees its transport end. A server that drops
//! the `RpcSystem` first (the socket closed, a deadline fired) skips it, so
//! every call still running at that moment lives on, unpolled, for the life of
//! the process, holding whatever it was waiting on. A call parked on a database
//! pool keeps that pool's slot: eight of them and the pool is wedged with every
//! connection idle.
//!
//! [`DisconnectOnDrop`] runs the disconnect from its `Drop`, so dropping the
//! system drops those calls too, and each one is counted `cancelled`.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};

use capnp_rpc::{Disconnector, RpcSystem};

/// An `RpcSystem` that is driven the same way, and whose drop disconnects the
/// connection, releasing every call still in flight.
pub struct DisconnectOnDrop<VatId: 'static> {
    system: Pin<Box<RpcSystem<VatId>>>,
    disconnector: Option<Disconnector<VatId>>,
}

impl<VatId: 'static> DisconnectOnDrop<VatId> {
    pub fn new(system: RpcSystem<VatId>) -> Self {
        let disconnector = Some(system.get_disconnector());
        Self {
            system: Box::pin(system),
            disconnector,
        }
    }
}

impl<VatId: 'static> Future for DisconnectOnDrop<VatId> {
    type Output = Result<(), capnp::Error>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.system.as_mut().poll(cx)
    }
}

impl<VatId: 'static> Drop for DisconnectOnDrop<VatId> {
    fn drop(&mut self) {
        let Some(disconnector) = self.disconnector.take() else {
            return;
        };
        // The first poll disconnects synchronously, which empties the answer
        // table and so drops the calls. What it then waits for, the transport's
        // shutdown, needs the system driven, and the system is going away, so
        // one poll is all there is. On a connection that already ended it is a
        // no-op.
        let mut disconnector = Box::pin(disconnector);
        let waker = Waker::from(Arc::new(NoopWake));
        let _ = disconnector.as_mut().poll(&mut Context::from_waker(&waker));
    }
}

struct NoopWake;

impl Wake for NoopWake {
    fn wake(self: Arc<Self>) {}
}
