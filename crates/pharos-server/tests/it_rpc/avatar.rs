//! V362: `Session.setAvatar` stores the server's own square re-encode of a
//! picture and answers its tag, which `Session.me` then reports;
//! `removeAvatar` clears it. What is not a picture is refused and leaves
//! nothing behind.

use std::io::Cursor;

use pharos_api::pharos_capnp::{avatar_result, done, error, me_result, session};

use crate::common::seed_rich;
use crate::login::{client, login};

fn png(width: u32, height: u32) -> Vec<u8> {
    let img = image::RgbImage::from_pixel(width, height, image::Rgb([30, 90, 200]));
    let mut out = Cursor::new(Vec::new());
    image::DynamicImage::from(img)
        .write_to(&mut out, image::ImageFormat::Png)
        .unwrap();
    out.into_inner()
}

/// The tag, or the error's kind name.
async fn set(session: &session::Client, image: &[u8]) -> Result<String, &'static str> {
    let mut req = session.set_avatar_request();
    req.get().init_ctx();
    req.get().set_image(image);
    let reply = req.send().promise.await.unwrap();
    match reply.get().unwrap().get_result().unwrap().which().unwrap() {
        avatar_result::Tag(t) => Ok(t.unwrap().to_string().unwrap()),
        avatar_result::Error(e) => Err(match e.unwrap().get_kind().which().unwrap() {
            error::kind::Invalid(_) => "invalid",
            error::kind::Limit(_) => "limit",
            _ => "other",
        }),
    }
}

async fn me_tag(session: &session::Client) -> String {
    let mut req = session.me_request();
    req.get().init_ctx();
    let reply = req.send().promise.await.unwrap();
    match reply.get().unwrap().get_result().unwrap().which().unwrap() {
        me_result::Me(me) => me.unwrap().get_avatar_tag().unwrap().to_string().unwrap(),
        me_result::Error(_) => panic!("me refused"),
    }
}

#[actix_web::test]
async fn a_set_avatar_is_the_servers_square_re_encode_and_me_reports_its_tag() {
    let fx = seed_rich().await;
    let (session, _) = login(&client(&fx, 16), "guest", "hunter2")
        .await
        .ok()
        .unwrap();
    assert_eq!(me_tag(&session).await, "", "no avatar yet");

    let tag = set(&session, &png(800, 600)).await.unwrap();
    assert_eq!(me_tag(&session).await, tag);
    let stored = pharos_server::avatar::load(&fx.state.stores, fx.user_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.tag, tag);
    assert_eq!(stored.content_type, "image/jpeg");
    let back = image::load_from_memory(&stored.bytes).unwrap();
    assert_eq!((back.width(), back.height()), (512, 512));

    let mut req = session.remove_avatar_request();
    req.get().init_ctx();
    let reply = req.send().promise.await.unwrap();
    assert!(matches!(
        reply.get().unwrap().get_result().unwrap().which().unwrap(),
        done::Ok(())
    ));
    assert_eq!(me_tag(&session).await, "");
}

#[actix_web::test]
async fn what_is_not_a_picture_or_is_too_big_is_refused() {
    let fx = seed_rich().await;
    let (session, _) = login(&client(&fx, 16), "guest", "hunter2")
        .await
        .ok()
        .unwrap();
    assert_eq!(
        set(&session, b"<svg xmlns='http://www.w3.org/2000/svg'/>").await,
        Err("invalid")
    );
    assert_eq!(set(&session, b"").await, Err("invalid"));
    let big = vec![0u8; 768 * 1024 + 1];
    assert_eq!(set(&session, &big).await, Err("limit"));
    assert_eq!(me_tag(&session).await, "");
}
