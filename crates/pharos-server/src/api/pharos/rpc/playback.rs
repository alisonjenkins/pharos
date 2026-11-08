//! `Playback` and `PlaybackGrant`: a native player negotiating an item and
//! reporting where it is. Negotiation is `services::playback::grant`, the
//! reports `services::play_state`, the same as the Jellyfin routes (US4).

use std::cell::Cell;
use std::rc::Rc;

use pharos_api::pharos_capnp::{
    capabilities, decision, done, opt_u32, playback as wire, playback_grant as grant_wire,
    start_tracks_result, stream_choice,
};
use pharos_api::{server, Denial};
use pharos_jellyfin_api::dto::WireId;

use super::library::{item_denial, read_item_id, IMAGE_DELIVERY_SCHEMA_VERSION};
use super::session::Session;
use crate::services::access::ItemHidden;
use crate::services::play_state::{self, Report};
use crate::services::playback::{self, Capabilities, GrantError, GrantRequest};

/// Jellyfin ticks (100 ns) per millisecond.
const TICKS_PER_MS: u64 = 10_000;

pub(super) struct Playback {
    session: Rc<Session>,
}

impl Playback {
    pub(super) fn new(session: Rc<Session>) -> Self {
        Self { session }
    }
}

impl server::playback::Handler for Playback {
    async fn negotiate(
        self: Rc<Self>,
        params: wire::NegotiateParams,
        results: &mut wire::NegotiateResults,
    ) -> Result<(), capnp::Error> {
        let params = params.get()?;
        let id = read_item_id(params.get_id()?)?;
        // A client older than image delivery is sent `burn` for an image
        // track by `Library.detail`, so it must not be granted one to draw.
        let knows_image_delivery =
            params.get_ctx()?.get_schema_version() >= IMAGE_DELIVERY_SCHEMA_VERSION;
        let capabilities = read_capabilities(params.get_caps()?, knows_image_delivery)?;
        // Negotiation always plays an audio track; "none" is refused rather
        // than read as the default.
        let audio_stream_index = match params.get_audio()?.which()? {
            stream_choice::Index(i) => Ok(Some(i)),
            stream_choice::Default(()) => Ok(None),
            stream_choice::None(()) => Err(Denial::NotPermitted {
                what: "playing with no audio track",
            }),
        };
        let subtitle_stream_index = match params.get_subtitle()?.which()? {
            stream_choice::Index(i) => Some(i64::from(i)),
            stream_choice::None(()) => Some(-1),
            stream_choice::Default(()) => None,
        };
        // A version that is not text only costs the listing its version.
        let client_version = params
            .get_ctx()?
            .get_client_version()?
            .to_string()
            .unwrap_or_default();
        let version = params.get_version()?.to_str()?.trim().to_owned();
        let session = &self.session;
        let granted = match (session.current_user().await, id, audio_stream_index) {
            (Err(denial), _, _) | (_, _, Err(denial)) => Err(denial),
            (Ok(user), WireId::Real(id), Ok(audio_stream_index)) => {
                let request = GrantRequest {
                    capabilities: &capabilities,
                    audio_stream_index,
                    subtitle_stream_index,
                    token: &session.token,
                    client_is_remote: session.client_is_remote(),
                    version: &version,
                };
                match playback::grant(&session.state, &user, id, &request).await {
                    Ok(grant) => {
                        start_session(session, &user, &grant, client_version).await;
                        Ok(grant)
                    }
                    Err(GrantError::Item(e)) => Err(item_denial(id, &e, "negotiate")),
                    Err(GrantError::NotPlayable) => Err(Denial::NotPermitted {
                        what: "streaming a book",
                    }),
                    Err(GrantError::Version(e)) => Err(super::library::version_denial(e)),
                }
            }
            (Ok(_), WireId::Synth(_), Ok(_)) => Err(Denial::NotFound { what: "item" }),
        };
        let mut out = results.get().init_result();
        match granted {
            Ok(grant) => {
                let grant = Grant {
                    session: Rc::clone(&self.session),
                    play_session_id: grant.play_session_id,
                    item_wire_id: grant.item_wire_id,
                    decision: grant.negotiated.decision_kind(),
                    direct_play_block: grant.negotiated.direct_play_block.label(),
                    downgrade: grant.negotiated.downgrade.unwrap_or("none"),
                    stream_url: grant.stream_url,
                    audio: grant.audio_stream_index,
                    subtitle: grant.subtitle_stream_index,
                    reported_ms: Cell::new(None),
                    stopped: Cell::new(false),
                };
                let conn = self.session.conn();
                out.set_grant(server::playback_grant::client(grant, &conn));
            }
            Err(denial) => denial.write(out.init_error()),
        }
        Ok(())
    }

    async fn start_tracks(
        self: Rc<Self>,
        params: wire::StartTracksParams,
        results: &mut wire::StartTracksResults,
    ) -> Result<(), capnp::Error> {
        let params = params.get()?;
        let id = read_item_id(params.get_id()?)?;
        let knows_image_delivery =
            params.get_ctx()?.get_schema_version() >= IMAGE_DELIVERY_SCHEMA_VERSION;
        let capabilities = read_capabilities(params.get_caps()?, knows_image_delivery)?;
        let session = &self.session;
        let picked = match (session.current_user().await, id) {
            (Err(denial), _) => Err(denial),
            (Ok(user), WireId::Real(id)) => {
                playback::start_tracks(&session.state, &user, id, &capabilities)
                    .await
                    .map_err(|e| match e {
                        GrantError::Item(e) => item_denial(id, &e, "startTracks"),
                        GrantError::NotPlayable => Denial::NotPermitted {
                            what: "streaming a book",
                        },
                        GrantError::Version(e) => super::library::version_denial(e),
                    })
            }
            (Ok(_), WireId::Synth(_)) => Err(Denial::NotFound { what: "item" }),
        };
        write_start_tracks(results.get().init_result(), picked);
        Ok(())
    }
}

fn write_start_tracks(
    out: start_tracks_result::Builder<'_>,
    picked: Result<(Option<u32>, Option<u32>), Denial>,
) {
    match picked {
        Ok((audio, subtitle)) => {
            let mut tracks = out.init_tracks();
            set_opt(tracks.reborrow().init_audio(), audio);
            set_opt(tracks.init_subtitle(), subtitle);
        }
        Err(denial) => denial.write(out.init_error()),
    }
}

/// The client name `/Sessions` lists a native player under.
const NATIVE_CLIENT: &str = "Pharos";
/// The device name it lists it under: the connection states none.
const NATIVE_DEVICE_NAME: &str = "Pharos native";

/// Lists the grant in `/Sessions` as `/Sessions/Playing` would, so the
/// dashboard shows it and the grant's reports and stop have an entry to move
/// and end. A registry fault is logged and the grant stands: the player can
/// still play, it just is not listed.
async fn start_session(
    session: &Session,
    user: &pharos_core::User,
    grant: &playback::Grant,
    client_version: String,
) {
    let Some(device_id) = session.device_id().await else {
        tracing::warn!(
            play_session_id = %pharos_core::secret_prefix(&grant.play_session_id),
            "native playback: session not listed, device unknown"
        );
        return;
    };
    let started = play_state::start(
        &session.state,
        play_state::Start {
            play_session_id: grant.play_session_id.clone(),
            user,
            device_id,
            device_name: NATIVE_DEVICE_NAME.into(),
            client: NATIVE_CLIENT.into(),
            version: if client_version.is_empty() {
                "0".into()
            } else {
                client_version
            },
            item_wire_id: grant.item_wire_id.clone(),
            position_ticks: 0,
            supports_remote_control: false,
        },
    )
    .await;
    if let Err(e) = started {
        tracing::warn!(
            play_session_id = %pharos_core::secret_prefix(&grant.play_session_id),
            error = %e,
            "native playback: session not listed"
        );
    }
}

/// One negotiated playback: its decision, and the reports that move it.
struct Grant {
    session: Rc<Session>,
    play_session_id: String,
    item_wire_id: String,
    decision: &'static str,
    direct_play_block: &'static str,
    downgrade: &'static str,
    stream_url: String,
    audio: Option<u32>,
    subtitle: Option<u32>,
    /// The position of the last report, which a stop on release saves.
    reported_ms: Cell<Option<u64>>,
    /// Set by `stop`: the playback has ended and a release has nothing to do.
    stopped: Cell<bool>,
}

/// The tracks a report names: `None` says nothing about that track.
#[derive(Default, Clone, Copy)]
struct Playing {
    audio: Option<i32>,
    subtitle: Option<i32>,
}

impl Grant {
    fn at(&self, position_ms: u64, is_paused: bool, playing: Playing) -> Report<'_> {
        Report {
            play_session_id: Some(&self.play_session_id),
            item_wire_id: Some(&self.item_wire_id),
            position_ticks: position_ms.saturating_mul(TICKS_PER_MS),
            is_paused,
            audio_stream_index: playing.audio,
            subtitle_stream_index: playing.subtitle,
        }
    }
}

/// A report's track: an index, `-1` for "none", nothing for `default` or for
/// an index no stream can have, which must not overwrite a remembered pick.
fn reported(choice: stream_choice::Reader<'_>) -> Result<Option<i32>, capnp::Error> {
    Ok(match choice.which()? {
        stream_choice::Index(i) => i32::try_from(i).ok(),
        stream_choice::None(()) => Some(-1),
        stream_choice::Default(()) => None,
    })
}

impl server::playback_grant::Handler for Grant {
    async fn decision(
        self: Rc<Self>,
        _params: grant_wire::DecisionParams,
        results: &mut grant_wire::DecisionResults,
    ) -> Result<(), capnp::Error> {
        let out = results.get().init_result();
        match self.session.current_user().await {
            Ok(_) => write_decision(out.init_decision(), &self),
            Err(denial) => denial.write(out.init_error()),
        }
        Ok(())
    }

    async fn report(
        self: Rc<Self>,
        params: grant_wire::ReportParams,
        results: &mut grant_wire::ReportResults,
    ) -> Result<(), capnp::Error> {
        let params = params.get()?;
        let playing = Playing {
            audio: reported(params.get_audio()?)?,
            subtitle: reported(params.get_subtitle()?)?,
        };
        let report = self.at(params.get_position_ms(), params.get_paused(), playing);
        let reported = match self.session.current_user().await {
            Ok(user) => play_state::report(&self.session.state, &user, report)
                .await
                .map_err(report_denial),
            Err(denial) => Err(denial),
        };
        if reported.is_ok() {
            self.reported_ms.set(Some(params.get_position_ms()));
        }
        write_done(results.get().init_result(), reported);
        Ok(())
    }

    async fn stop(
        self: Rc<Self>,
        params: grant_wire::StopParams,
        results: &mut grant_wire::StopResults,
    ) -> Result<(), capnp::Error> {
        let report = self.at(params.get()?.get_position_ms(), false, Playing::default());
        let stopped = match self.session.current_user().await {
            Ok(user) => play_state::stop(&self.session.state, &user, report)
                .await
                .map_err(report_denial),
            Err(denial) => Err(denial),
        };
        // A stop that failed leaves the release to try again.
        self.stopped.set(stopped.is_ok());
        write_done(results.get().init_result(), stopped);
        Ok(())
    }
}

impl Drop for Grant {
    /// A grant released without a stop, which is how a closed tab or a lost
    /// connection looks from here, ends its playback as `stop` would: the
    /// transcode and the session's now-playing go, and the last reported
    /// position is saved. With no report there is no position to save, so
    /// the stored one is left alone.
    fn drop(&mut self) {
        if self.stopped.get() || crate::state::is_shutting_down() {
            return;
        }
        // The grant can be dropped as the runtime goes down.
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let session = Rc::clone(&self.session);
        let play_session_id = std::mem::take(&mut self.play_session_id);
        let psid = pharos_core::PlaySessionId::from(play_session_id.as_str());
        let item_wire_id = std::mem::take(&mut self.item_wire_id);
        let reported_ms = self.reported_ms.get();
        actix_web::rt::spawn(async move {
            let report = Report {
                play_session_id: Some(&play_session_id),
                item_wire_id: reported_ms.map(|_| item_wire_id.as_str()),
                position_ticks: reported_ms.unwrap_or(0).saturating_mul(TICKS_PER_MS),
                is_paused: false,
                audio_stream_index: None,
                subtitle_stream_index: None,
            };
            // A revoked session saves no position but still ends the
            // stream and the now-playing it owns.
            let (ended, saved) = match session.current_user().await {
                Ok(user) => match play_state::stop(&session.state, &user, report).await {
                    // The item was hidden from the user since the last
                    // report: `stop` wrote nothing and ended nothing, so
                    // end the playback without saving a position.
                    Err(play_state::ReportError::Withheld(hidden)) => {
                        tracing::info!(%play_session_id, ?hidden, "playback: released grant's item is withheld, ending without a position");
                        (play_state::end(&session.state, user.id, &psid).await, false)
                    }
                    other => (other, reported_ms.is_some()),
                },
                Err(_) => (
                    play_state::end(&session.state, session.owner(), &psid).await,
                    false,
                ),
            };
            let ended = ended.map_err(|e| e.to_string());
            match ended {
                Ok(()) => {
                    tracing::info!(%play_session_id, saved, "playback: grant released without a stop, playback ended")
                }
                Err(error) => {
                    tracing::warn!(%play_session_id, %error, "playback: grant released without a stop, ending it failed")
                }
            }
        });
    }
}

fn read_capabilities(
    caps: capabilities::Reader<'_>,
    knows_image_delivery: bool,
) -> Result<Capabilities, capnp::Error> {
    let texts = |list: capnp::text_list::Reader<'_>| -> Result<Vec<String>, capnp::Error> {
        list.iter().map(|t| Ok(t?.to_str()?.to_owned())).collect()
    };
    Ok(Capabilities {
        video_codecs: texts(caps.get_video_codecs()?)?,
        audio_codecs: texts(caps.get_audio_codecs()?)?,
        containers: texts(caps.get_containers()?)?,
        max_bitrate: caps.get_max_bitrate(),
        hdr: caps.get_hdr(),
        image_subtitles: if knows_image_delivery {
            texts(caps.get_image_subtitles()?)?
        } else {
            Vec::new()
        },
    })
}

fn write_decision(mut out: decision::Builder<'_>, grant: &Grant) {
    out.set_method(grant.decision);
    out.set_direct_play_block(grant.direct_play_block);
    out.set_downgrade(grant.downgrade);
    out.set_stream_url(grant.stream_url.as_str());
    out.reborrow()
        .init_play_session()
        .set_value(grant.play_session_id.as_str());
    set_opt(out.reborrow().init_audio(), grant.audio);
    set_opt(out.init_subtitle(), grant.subtitle);
}

fn set_opt(mut out: opt_u32::Builder<'_>, value: Option<u32>) {
    match value {
        Some(v) => out.set_some(v),
        None => out.set_none(()),
    }
}

fn report_denial(e: play_state::ReportError) -> Denial {
    match e {
        play_state::ReportError::Withheld(ItemHidden::Unavailable(_)) => Denial::Unavailable {
            dependency: "item store",
        },
        play_state::ReportError::Withheld(_) => Denial::NotFound { what: "item" },
        play_state::ReportError::RegistryDown(_) => {
            tracing::warn!(error = %e, "native playback report: session registry unavailable");
            Denial::Unavailable {
                dependency: "session registry",
            }
        }
    }
}

fn write_done(mut out: done::Builder<'_>, result: Result<(), Denial>) {
    match result {
        Ok(()) => out.set_ok(()),
        Err(denial) => denial.write(out.init_error()),
    }
}
