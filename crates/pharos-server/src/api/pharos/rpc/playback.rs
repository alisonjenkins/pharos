//! `Playback` and `PlaybackGrant`: a native player negotiating an item and
//! reporting where it is. Negotiation is `services::playback::grant`, the
//! reports `services::play_state`, the same as the Jellyfin routes (US4).

use std::rc::Rc;

use pharos_api::pharos_capnp::{
    capabilities, decision, done, opt_u32, playback as wire, playback_grant as grant_wire,
    stream_choice,
};
use pharos_api::{server, Denial};
use pharos_jellyfin_api::dto::WireId;

use super::library::{item_denial, read_item_id};
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
        let capabilities = read_capabilities(params.get_caps()?)?;
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
                };
                playback::grant(&session.state, &user, id, &request)
                    .await
                    .map_err(|e| match e {
                        GrantError::Item(e) => item_denial(id, &e, "negotiate"),
                        GrantError::NotPlayable => Denial::NotPermitted {
                            what: "streaming a book",
                        },
                    })
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
                };
                let conn = self.session.conn();
                out.set_grant(server::playback_grant::client(grant, &conn));
            }
            Err(denial) => denial.write(out.init_error()),
        }
        Ok(())
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
        write_done(results.get().init_result(), stopped);
        Ok(())
    }
}

fn read_capabilities(caps: capabilities::Reader<'_>) -> Result<Capabilities, capnp::Error> {
    let texts = |list: capnp::text_list::Reader<'_>| -> Result<Vec<String>, capnp::Error> {
        list.iter().map(|t| Ok(t?.to_str()?.to_owned())).collect()
    };
    Ok(Capabilities {
        video_codecs: texts(caps.get_video_codecs()?)?,
        audio_codecs: texts(caps.get_audio_codecs()?)?,
        containers: texts(caps.get_containers()?)?,
        max_bitrate: caps.get_max_bitrate(),
        image_subtitles: texts(caps.get_image_subtitles()?)?,
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
