//! 014 over the native API as pharos-ui calls it: `pharos_client::group`
//! against a real server, with updates arriving on the callback.

use std::time::Duration;

use pharos_client::api::ItemRef;
use pharos_client::group::{GroupUpdate, NativeGroup};
use pharos_client::{CallInfo, ClientError};
use tokio::sync::mpsc;

use crate::group::{rig, Rig};
use crate::playback::MOVIE;

fn call(device: &str) -> CallInfo {
    CallInfo {
        device: device.into(),
        ..CallInfo::default()
    }
}

async fn group(rig: &Rig, token: &str) -> NativeGroup {
    NativeGroup::new(&rig.session(token).await)
}

/// Every update a member was pushed, kept so a wait for one kind does not
/// lose the others.
struct Seen {
    rx: mpsc::UnboundedReceiver<GroupUpdate>,
    log: Vec<GroupUpdate>,
}

fn collector() -> (impl Fn(GroupUpdate) + 'static, Seen) {
    let (tx, rx) = mpsc::unbounded_channel();
    (
        move |u| {
            let _ = tx.send(u);
        },
        Seen { rx, log: vec![] },
    )
}

/// The first update `want` accepts, from those already seen or arriving
/// within a second; a panic otherwise.
async fn until<T>(seen: &mut Seen, want: impl Fn(&GroupUpdate) -> Option<T>) -> T {
    if let Some(t) = seen.log.iter().find_map(&want) {
        return t;
    }
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let update = seen.rx.recv().await.expect("sink open");
            let found = want(&update);
            seen.log.push(update);
            if let Some(t) = found {
                return t;
            }
        }
    })
    .await
    .expect("the wanted update within a second")
}

/// Create, list, join, the clock and every membership command, as the group
/// store sends them, with the pushed updates read into plain types.
/// Disarm check: with `read_update` mapping `Pause` to `Play`, the pause
/// assertion fails.
#[actix_web::test]
async fn a_client_creates_lists_joins_and_drives_a_group() {
    let rig = rig().await;
    let a = group(&rig, &rig.fx.admin_token).await;
    let b = group(&rig, &rig.fx.user_token).await;
    let (a_sink, mut a_seen) = collector();
    let a_call = call("dev-a");
    let a_member = a
        .create(&a_call, ItemRef::Real(MOVIE), 5_000, a_sink)
        .await
        .expect("create");

    let listed = b.list(&call("dev-b")).await.expect("list");
    assert_eq!(listed.len(), 1, "{listed:?}");
    assert_eq!(listed[0].item, Some(ItemRef::Real(MOVIE)));
    assert_eq!(listed[0].name, "ali's Group");

    let (b_sink, mut b_seen) = collector();
    let b_call = call("dev-b");
    let b_member = b.join(&b_call, &listed[0].id, b_sink).await.expect("join");
    let joined = until(&mut b_seen, |u| match u {
        GroupUpdate::Joined(g) => Some(g.clone()),
        _ => None,
    })
    .await;
    assert_eq!(joined.id, listed[0].id);
    assert_eq!(joined.item, Some(ItemRef::Real(MOVIE)));
    let arrived = until(&mut a_seen, |u| match u {
        GroupUpdate::Member(m) => Some(m.name.clone()),
        _ => None,
    })
    .await;
    assert_eq!(arrived, "guest");

    let sample = a.clock(&a_call, 1_000).await.expect("clock");
    assert_eq!(sample.client_ms_echo, 1_000);
    assert!(sample.recv_ms > 0 && sample.send_ms >= sample.recv_ms);
    a_member
        .clock_report(&a_call, 1_000, sample.recv_ms, sample.send_ms, 1_020)
        .await
        .expect("clock report");
    a_member.ping(&a_call).await.expect("ping");
    b_member.ping(&b_call).await.expect("ping");

    // Both ack the queue entry, which opens the readiness gate.
    for (member, call, seen) in [
        (&a_member, &a_call, &mut a_seen),
        (&b_member, &b_call, &mut b_seen),
    ] {
        let playlist_item_id = until(seen, |u| match u {
            GroupUpdate::Item {
                playlist_item_id, ..
            } => Some(playlist_item_id.clone()),
            _ => None,
        })
        .await;
        member
            .buffering(call, 5_000, true, &playlist_item_id)
            .await
            .expect("buffering");
        member
            .ready(call, 5_000, false, &playlist_item_id)
            .await
            .expect("ready");
    }

    a_member.unpause(&a_call).await.expect("unpause");
    until(&mut b_seen, |u| {
        matches!(u, GroupUpdate::Play { .. }).then_some(())
    })
    .await;
    a_member.seek(&a_call, 20_000).await.expect("seek");
    until(&mut b_seen, |u| match u {
        GroupUpdate::Seek { position_ms, .. } => (*position_ms == 20_000).then_some(()),
        _ => None,
    })
    .await;
    a_member.pause(&a_call).await.expect("pause");
    until(&mut b_seen, |u| {
        matches!(u, GroupUpdate::Pause { .. }).then_some(())
    })
    .await;

    b_member.leave(&b_call).await.expect("leave");
    until(&mut b_seen, |u| {
        matches!(u, GroupUpdate::Ended).then_some(())
    })
    .await;
}

/// A group that has ended is `notFound` to a joiner, which the UI shows as
/// "This group has ended"; it never becomes a new group.
#[actix_web::test]
async fn joining_a_missing_group_is_not_found() {
    let rig = rig().await;
    let b = group(&rig, &rig.fx.user_token).await;
    let (sink, _seen) = collector();
    let err = b
        .join(&call("dev-b"), "00000000-0000-0000-0000-000000000000", sink)
        .await
        .err()
        .expect("refused");
    assert!(
        matches!(
            err,
            ClientError::Api {
                kind: "notFound",
                ..
            }
        ),
        "{err:?}"
    );
}
