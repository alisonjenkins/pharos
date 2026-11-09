//! B938: a damaged file is badged in listings and its detail says what is
//! damaged and where, for every user, not only admins.

use pharos_api::pharos_capnp::{detail_result, items_result, Kind};
use pharos_core::{LibraryStore as _, MediaItem, MediaKind, MediaStore as _};
use pharos_server::integrity_memo::{IntegrityMemo, INTEGRITY_SCAN_VERSION};
use pharos_transcode::integrity::{DamageRange, IntegrityReport};

const DAMAGED: u64 = 301;
const CLEAN: u64 = 302;

fn memo() -> IntegrityMemo {
    let memo = IntegrityMemo::load("/nonexistent/integrity.json", INTEGRITY_SCAN_VERSION);
    memo.record(
        DAMAGED,
        Some((1, 1)),
        IntegrityReport {
            demux_errors: 2,
            first_fault_ms: Some(600_000),
            first_fault: Some("invalid as first byte of an EBML number".into()),
            scanned_ms: 2_400_000,
            declared_ms: Some(2_400_000),
            complete: true,
            ranges: Some(vec![
                DamageRange {
                    start_ms: 600_000,
                    end_ms: 1_140_000,
                },
                DamageRange {
                    start_ms: 2_000_000,
                    end_ms: 2_000_040,
                },
            ]),
            ..Default::default()
        },
    );
    memo.record(
        CLEAN,
        Some((1, 1)),
        IntegrityReport {
            packets: 10,
            scanned_ms: 2_400_000,
            complete: true,
            ranges: Some(Vec::new()),
            ..Default::default()
        },
    );
    memo
}

/// Disarm check: with `write_item` leaving `damaged` unset, the listing
/// assertion fails; with `detail` not writing `damage`, the detail one does.
#[actix_web::test]
async fn a_damaged_file_is_badged_and_its_detail_says_where() {
    let (server, fx) = crate::revocation::start_with_integrity_memo(memo()).await;
    for (id, title) in [(DAMAGED, "Broken Film"), (CLEAN, "Whole Film")] {
        fx.state
            .stores
            .put(MediaItem {
                id,
                path: format!("/libA/{title}.mkv").into(),
                title: title.into(),
                kind: MediaKind::Movie,
                ..MediaItem::default()
            })
            .await
            .unwrap();
    }
    fx.state.stores.backfill_library_ids().await.unwrap();
    let (client, _closed) = crate::revocation::connect(&server).await;
    // A plain user, not an admin: damage is shown to everyone.
    let session = crate::revocation::signed_in(&client, &fx.user_token).await;
    let library = session.library_request().send().pipeline.get_library();

    let mut req = library.query_request();
    req.get().init_ctx();
    let mut q = req.get().init_q();
    q.reborrow().init_kinds(1).set(0, Kind::Movie);
    q.set_limit(100);
    let reply = req.send().promise.await.expect("query");
    let items_result::Page(page) = reply.get().unwrap().get_result().unwrap().which().unwrap()
    else {
        panic!("query refused");
    };
    let mut badges: Vec<(String, bool)> = page
        .unwrap()
        .get_items()
        .unwrap()
        .iter()
        .map(|i| (i.get_name().unwrap().to_string().unwrap(), i.get_damaged()))
        .filter(|(name, _)| name.ends_with(" Film"))
        .collect();
    badges.sort();
    assert_eq!(
        badges,
        vec![("Broken Film".into(), true), ("Whole Film".into(), false)]
    );

    let detail = |id: u64| {
        let mut req = library.detail_request();
        req.get().init_ctx();
        req.get().init_id().set_real(id);
        req.send().promise
    };
    let reply = detail(DAMAGED).await.expect("detail");
    let detail_result::Detail(d) = reply.get().unwrap().get_result().unwrap().which().unwrap()
    else {
        panic!("detail refused");
    };
    let d = d.unwrap();
    assert!(d.get_summary().unwrap().get_damaged());
    let damage = d.get_damage().unwrap();
    assert_eq!(damage.get_verdict().unwrap().to_str().unwrap(), "damaged");
    assert_eq!(
        damage.get_reason().unwrap().to_str().unwrap(),
        "invalid as first byte of an EBML number"
    );
    let ranges: Vec<(u64, u64)> = damage
        .get_ranges()
        .unwrap()
        .iter()
        .map(|r| (r.get_start_ms(), r.get_end_ms()))
        .collect();
    assert_eq!(ranges, vec![(600_000, 1_140_000), (2_000_000, 2_000_040)]);
    assert!(damage.get_checked_at() > 0);
    let versions = d.get_versions().unwrap();
    assert!(
        versions.get(0).get_damaged(),
        "the primary file is the damaged one"
    );

    let reply = detail(CLEAN).await.expect("detail");
    let detail_result::Detail(d) = reply.get().unwrap().get_result().unwrap().which().unwrap()
    else {
        panic!("detail refused");
    };
    let d = d.unwrap();
    assert!(!d.get_summary().unwrap().get_damaged());
    assert!(
        d.get_damage().unwrap().get_verdict().unwrap().is_empty(),
        "a clean verdict is no damage"
    );
}
