#![allow(clippy::unwrap_used, clippy::expect_used)]
//! 045 / ADR 0128: a user makes, lists and revokes their e-reader passwords
//! over the native API; the password a create answers opens the OPDS check,
//! and once revoked it opens nothing.
//! Disarm check: with `revoke_reader_password` writing `ok` without calling
//! the service, `a_user_makes_lists_and_revokes_a_reader_password` sees the
//! revoked password still listed.

use std::net::{IpAddr, Ipv4Addr};

use base64::Engine as _;
use pharos_api::pharos_capnp::{
    created_reader_password_result, done, error, reader_passwords_result, session,
};
use pharos_server::services::reader_password;

const IP: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);

/// (id, label, created ms, last used ms) per password, as served.
async fn listed(session: &session::Client) -> Vec<(String, String, i64, i64)> {
    let mut req = session.reader_passwords_request();
    req.get().init_ctx();
    let reply = req.send().promise.await.unwrap();
    match reply.get().unwrap().get_result().unwrap().which().unwrap() {
        reader_passwords_result::Passwords(list) => list
            .unwrap()
            .iter()
            .map(|p| {
                (
                    p.get_id().unwrap().to_string().unwrap(),
                    p.get_label().unwrap().to_string().unwrap(),
                    p.get_created_at_ms(),
                    p.get_last_used_at_ms(),
                )
            })
            .collect(),
        reader_passwords_result::Error(_) => panic!("readerPasswords refused"),
    }
}

/// (id, password, opds path), or the refusal's kind.
async fn create(
    session: &session::Client,
    label: &str,
) -> Result<(String, String, String), &'static str> {
    let mut req = session.create_reader_password_request();
    req.get().init_ctx();
    req.get().set_label(label);
    let reply = req.send().promise.await.unwrap();
    match reply.get().unwrap().get_result().unwrap().which().unwrap() {
        created_reader_password_result::Created(c) => {
            let c = c.unwrap();
            Ok((
                c.get_listed()
                    .unwrap()
                    .get_id()
                    .unwrap()
                    .to_string()
                    .unwrap(),
                c.get_password().unwrap().to_string().unwrap(),
                c.get_opds_path().unwrap().to_string().unwrap(),
            ))
        }
        created_reader_password_result::Error(e) => Err(kind(e.unwrap())),
    }
}

async fn revoke(session: &session::Client, id: &str) -> Result<(), &'static str> {
    let mut req = session.revoke_reader_password_request();
    req.get().init_ctx();
    req.get().set_id(id);
    let reply = req.send().promise.await.unwrap();
    match reply.get().unwrap().get_result().unwrap().which().unwrap() {
        done::Ok(()) => Ok(()),
        done::Error(e) => Err(kind(e.unwrap())),
    }
}

fn kind(e: error::Reader<'_>) -> &'static str {
    match e.get_kind().which().unwrap() {
        error::kind::NotFound(_) => "not_found",
        error::kind::Invalid(_) => "invalid",
        error::kind::Limit(_) => "limit",
        _ => "other",
    }
}

fn basic(name: &str, password: &str) -> String {
    let pair = format!("{name}:{password}");
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(pair)
    )
}

#[actix_web::test]
async fn a_user_makes_lists_and_revokes_a_reader_password() {
    let (server, fx) = crate::revocation::start().await;
    let (client, _closed) = crate::revocation::connect(&server).await;
    let session = crate::revocation::signed_in(&client, &fx.user_token).await;

    assert!(listed(&session).await.is_empty());
    let (id, password, opds_path) = create(&session, "  Reading tablet ").await.unwrap();
    assert_eq!(opds_path, "/opds/");
    let typed = pharos_core::TypedReaderPassword::parse(&password).unwrap();
    assert_eq!(
        typed.id.as_str(),
        id,
        "the shown password starts with its id"
    );

    let list = listed(&session).await;
    assert_eq!(list.len(), 1, "{list:?}");
    assert_eq!(
        (list[0].0.as_str(), list[0].1.as_str()),
        (id.as_str(), "Reading tablet")
    );
    assert!(list[0].2 > 0, "created is set");
    assert_eq!(list[0].3, 0, "never used yet");

    let header = basic("guest", &password);
    let (user, used) = reader_password::authenticate(&fx.state, Some(&header), IP)
        .await
        .unwrap();
    assert_eq!((user.id, used.as_str()), (fx.user_id, id.as_str()));
    assert!(listed(&session).await[0].3 > 0, "the check records its use");

    assert_eq!(revoke(&session, &id).await, Ok(()));
    assert!(listed(&session).await.is_empty());
    assert!(
        reader_password::authenticate(&fx.state, Some(&header), IP)
            .await
            .is_err(),
        "a revoked password opens nothing"
    );
    assert_eq!(revoke(&session, &id).await, Err("not_found"));
    assert_eq!(revoke(&session, "../etc").await, Err("not_found"));
}

#[actix_web::test]
async fn a_bad_label_or_one_too_many_is_refused() {
    let (server, fx) = crate::revocation::start().await;
    let (client, _closed) = crate::revocation::connect(&server).await;
    let session = crate::revocation::signed_in(&client, &fx.user_token).await;

    assert_eq!(create(&session, "   ").await, Err("invalid"));
    assert_eq!(create(&session, &"x".repeat(65)).await, Err("invalid"));
    assert_eq!(create(&session, "tab\there").await, Err("invalid"));
    for n in 0..reader_password::MAX_LIVE_PER_USER {
        create(&session, &format!("reader {n}")).await.unwrap();
    }
    assert_eq!(create(&session, "one more").await, Err("limit"));
    assert_eq!(
        listed(&session).await.len(),
        reader_password::MAX_LIVE_PER_USER
    );
}
