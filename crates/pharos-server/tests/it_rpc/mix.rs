//! `Library.mix`: dynamic mixes over the native API (spec 024, B440/V380).

use pharos_api::pharos_capnp::{mix_result, session, MixWeight};
use pharos_core::{
    LibraryStore as _, MediaItem, MediaKind, MediaProbe, MediaStore as _, UserPolicy,
};
use pharos_server::services::mix::{self, MixAsk, Rule, Seed, Test, Weight};

use crate::common::Fixture;
use crate::telemetry::{connect, counter, session_for, subscriber, Captured};

/// Puts `n` songs per (artist, genre) under `root`, ids from `first`.
async fn put_songs(fx: &Fixture, first: u64, root: &str, rows: &[(&str, &str, u32)], n: u64) {
    let mut id = first;
    for (artist, genre, year) in rows {
        for k in 0..n {
            fx.state
                .stores
                .put(MediaItem {
                    id,
                    path: format!("{root}/{artist}/{k}.flac").into(),
                    title: format!("{artist} {k}"),
                    kind: MediaKind::Audio,
                    probe: MediaProbe {
                        artist: Some((*artist).to_owned()),
                        album: Some(format!("{artist} LP")),
                        genre: Some((*genre).to_owned()),
                        year: Some(*year),
                        duration_ms: Some(200_000),
                        ..MediaProbe::default()
                    },
                    ..MediaItem::default()
                })
                .await
                .unwrap();
            id += 1;
        }
    }
}

struct Wire {
    picks: Vec<(u64, Vec<String>)>,
    exhausted: bool,
    relaxed: Vec<String>,
}

/// One rule as `(weight, share, genre)`; every test here is a genre test.
async fn wire_mix(
    session: &session::Client,
    rules: &[(MixWeight, u8, &str)],
    recent: &[u64],
    count: u32,
) -> Result<Wire, String> {
    let library = session.library_request().send().pipeline.get_library();
    let mut req = library.mix_request();
    {
        let mut p = req.get();
        p.reborrow().init_ctx();
        let mut spec = p.reborrow().init_spec();
        spec.set_artist_gap(1);
        spec.reborrow().init_seed().set_none(());
        let mut list = spec.init_rules(rules.len() as u32);
        for (i, (weight, share, genre)) in rules.iter().enumerate() {
            let mut r = list.reborrow().get(i as u32);
            r.set_weight(*weight);
            r.set_share(*share);
            r.init_test().set_genre(*genre);
        }
        let mut ids = p.reborrow().init_recent(recent.len() as u32);
        for (i, id) in recent.iter().enumerate() {
            ids.reborrow().get(i as u32).set_real(*id);
        }
        p.set_count(count);
    }
    let reply = req.send().promise.await.expect("mix");
    match reply.get().unwrap().get_result().unwrap().which().unwrap() {
        mix_result::Batch(b) => {
            let b = b.unwrap();
            let picks = b
                .get_picks()
                .unwrap()
                .iter()
                .map(|p| {
                    let id = match p.get_item().unwrap().get_id().unwrap().which().unwrap() {
                        pharos_api::pharos_capnp::item_id::Real(id) => id,
                        pharos_api::pharos_capnp::item_id::Synth(_) => panic!("a synth pick"),
                    };
                    let reasons = p
                        .get_reasons()
                        .unwrap()
                        .iter()
                        .map(|r| r.unwrap().to_string().unwrap())
                        .collect();
                    (id, reasons)
                })
                .collect();
            let relaxed = b
                .get_relaxed()
                .unwrap()
                .iter()
                .map(|r| r.unwrap().to_string().unwrap())
                .collect();
            Ok(Wire {
                picks,
                exhausted: b.get_exhausted(),
                relaxed,
            })
        }
        mix_result::Error(e) => Err(e.unwrap().get_reason().unwrap().to_string().unwrap()),
    }
}

/// Required and excluded genres hold on the wire, preferred rules come back
/// as reasons, a mix that runs dry says so, and every call is counted under
/// `pharos_mix_total{outcome}` with one log line naming what it decided.
/// Disarm check: with `services::mix::mix` not counting, the counter
/// assertions fail; with the require filter removed from `select`, the
/// genre assertion does.
#[actix_web::test]
async fn native_mix_holds_hard_rules_names_reasons_and_is_counted() {
    let fx = crate::common::seed_rich().await;
    put_songs(
        &fx,
        1_000,
        "/libA/music",
        &[
            ("Portishead", "Trip-hop", 1994),
            ("Slayer", "Metal", 1986),
            ("Tricky", "Trip-hop", 1995),
        ],
        6,
    )
    .await;
    let logs = Captured::default();
    let _log = tracing::subscriber::set_default(subscriber(&logs));
    let recorder = metrics_util::debugging::DebuggingRecorder::new();
    let snapshotter = pharos_test_metrics::snapshotter(&recorder);
    let _metrics = metrics::set_default_local_recorder(&recorder);
    tokio::task::LocalSet::new()
        .run_until(async {
            let client = connect(&fx);
            let session = session_for(&client, &fx.user_token).await;
            let full = wire_mix(
                &session,
                &[
                    (MixWeight::Require, 0, "trip-hop"),
                    (MixWeight::Prefer, 0, "Trip-hop"),
                ],
                &[],
                8,
            )
            .await
            .unwrap();
            assert_eq!(full.picks.len(), 8);
            assert!(!full.exhausted);
            for (id, reasons) in &full.picks {
                assert!(
                    (1_000..1_006).contains(id) || (1_012..1_018).contains(id),
                    "{id}"
                );
                assert_eq!(reasons, &["Trip-hop".to_owned()]);
            }
            // Only 6 metal songs exist; asking for 10 comes back short.
            let short = wire_mix(&session, &[(MixWeight::Require, 0, "Metal")], &[], 10)
                .await
                .unwrap();
            assert_eq!(short.picks.len(), 6);
            assert!(short.exhausted);
            assert!(
                short.relaxed.contains(&"spacing".to_owned()),
                "{:?}",
                short.relaxed
            );
            let none = wire_mix(
                &session,
                &[
                    (MixWeight::Exclude, 0, "Trip-hop"),
                    (MixWeight::Exclude, 0, "Metal"),
                ],
                &[],
                5,
            )
            .await
            .unwrap();
            assert!(none.picks.is_empty() && none.exhausted);
            // A share on a require rule is refused, naming no input.
            let bad = wire_mix(&session, &[(MixWeight::Require, 30, "Metal")], &[], 5).await;
            assert!(bad.is_err());
        })
        .await;
    let outcome = |o| counter(&snapshotter, "pharos_mix_total", &[("outcome", o)]);
    assert_eq!(outcome("full"), 1);
    assert_eq!(outcome("short"), 1);
    assert_eq!(outcome("empty"), 1);
    assert_eq!(outcome("invalid"), 1);
    assert_eq!(
        counter(
            &snapshotter,
            "pharos_mix_relaxed_total",
            &[("step", "spacing")]
        ),
        1
    );
    let text = logs.text();
    assert!(
        text.contains("mix: batch picked")
            && text.contains("eligible=12")
            && text.contains("picked=8"),
        "{text}"
    );
    assert!(
        text.contains("mix: no batch") && text.contains("share 30%"),
        "{text}"
    );
}

/// A mix draws only from libraries the user may open (V380).
#[actix_web::test]
async fn a_mix_never_reaches_a_library_the_user_cannot_open() {
    let fx = crate::common::seed_rich().await;
    put_songs(&fx, 2_000, "/libA/music", &[("Seen", "Pop", 2001)], 5).await;
    put_songs(&fx, 2_100, "/libB/music", &[("Hidden", "Pop", 2001)], 5).await;
    // Songs put after the fixture's backfill carry no library id yet.
    fx.state.stores.backfill_library_ids().await.unwrap();
    let mut user = pharos_server::services::auth::user_for_token(&fx.state.stores, &fx.user_token)
        .await
        .unwrap();
    user.policy = UserPolicy {
        enable_all_folders: false,
        enabled_folders: vec![fx.lib_a_wire.clone()],
        ..user.policy
    };
    let ask = MixAsk {
        rules: vec![Rule {
            weight: Weight::Require,
            share: None,
            test: Test::Genre("Pop".into()),
        }],
        seed: Seed::None,
        artist_gap: 0,
        album_gap: 0,
        recent: vec![],
        count: 20,
    };
    let mixed = mix::mix(&fx.state, &user, ask, 0).await.unwrap();
    let ids: Vec<u64> = mixed.picks.iter().map(|p| p.item.id).collect();
    assert_eq!(ids.len(), 5, "{ids:?}");
    assert!(ids.iter().all(|id| (2_000..2_005).contains(id)), "{ids:?}");
    // A hidden seed answers as a missing one.
    let hidden_seed = MixAsk {
        rules: vec![],
        seed: Seed::Song(2_100),
        artist_gap: 0,
        album_gap: 0,
        recent: vec![],
        count: 1,
    };
    assert!(matches!(
        mix::mix(&fx.state, &user, hidden_seed, 0).await,
        Err(mix::MixError::SeedNotFound(2_100))
    ));
}
