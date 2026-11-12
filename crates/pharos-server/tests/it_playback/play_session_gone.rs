//! The 410 a stream request gets for a play session the registry no longer
//! holds must be countable by route. A browser-playback outage was diagnosed
//! from a proxy's status codes because the server said nothing of its own.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use actix_web::{test, App};
use pharos_server::api::jellyfin::hls;

use super::session_start_position::seed;

fn gone_count(route: &str) -> u64 {
    pharos_server::obs::render()
        .lines()
        .filter(|l| {
            l.starts_with("pharos_play_session_gone_total{")
                && l.contains(&format!("route=\"{route}\""))
        })
        .filter_map(|l| l.rsplit(' ').next()?.parse::<u64>().ok())
        .sum()
}

#[actix_web::test]
async fn a_410_for_a_gone_session_is_counted_per_route() {
    let _ = pharos_server::obs::init("info", None);
    let (state, token) = seed().await;
    let app = test::init_service(App::new().app_data(state.clone()).configure(hls::register)).await;

    let cases = [
        ("/videos/7/h264cmaf/init.mp4", "cmaf_init"),
        ("/videos/7/h264cmaf/3.m4s", "cmaf_segment"),
        ("/videos/7/vp9/init.mp4", "vp9_init"),
        ("/videos/7/vp9/3.m4s", "vp9_segment"),
    ];
    for (path, route) in cases {
        let before = gone_count(route);
        let res = test::call_service(
            &app,
            test::TestRequest::get()
                .uri(&format!(
                    "{path}?PlaySessionId=never-registered&api_key={token}"
                ))
                .to_request(),
        )
        .await;
        assert_eq!(res.status().as_u16(), 410, "{path}");
        assert_eq!(gone_count(route), before + 1, "{route} must be counted");
    }
}
