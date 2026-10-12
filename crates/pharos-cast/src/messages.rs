use std::fmt;

use serde::{Deserialize, Serialize};

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AccessToken(String);

impl AccessToken {
    pub fn new(token: impl Into<String>) -> Self {
        Self(token.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for AccessToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[redacted]")
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReceiverCommand {
    #[serde(flatten)]
    pub body: CommandBody,
    pub server_address: String,
    pub access_token: AccessToken,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receiver_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_bitrate: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "command", content = "options")]
pub enum CommandBody {
    PlayNow(PlayNowOptions),
    Pause,
    Unpause,
    Stop,
    NextTrack,
    PreviousTrack,
    Seek(SeekOptions),
    SetAudioStreamIndex(AudioStreamOptions),
}

impl CommandBody {
    pub const fn name(&self) -> &'static str {
        match self {
            CommandBody::PlayNow(_) => "PlayNow",
            CommandBody::Pause => "Pause",
            CommandBody::Unpause => "Unpause",
            CommandBody::Stop => "Stop",
            CommandBody::NextTrack => "NextTrack",
            CommandBody::PreviousTrack => "PreviousTrack",
            CommandBody::Seek(_) => "Seek",
            CommandBody::SetAudioStreamIndex(_) => "SetAudioStreamIndex",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlayNowOptions {
    pub items: Vec<PlayItem>,
    pub start_index: u32,
    pub start_position_ticks: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_source_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_stream_index: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subtitle_stream_index: Option<i32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlayItem {
    #[serde(rename = "Id")]
    pub id: String,
    #[serde(rename = "Type")]
    pub kind: String,
}

/// Seconds, as the receiver expects.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SeekOptions {
    pub position: f64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioStreamOptions {
    pub index: i32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ReceiverReport {
    #[serde(rename = "playbackstart")]
    PlaybackStart(PlaybackReport),
    #[serde(rename = "playbackprogress")]
    PlaybackProgress(PlaybackReport),
    #[serde(rename = "playbackstop")]
    PlaybackStop(PlaybackReport),
    #[serde(rename = "playstatechange")]
    PlayStateChange(PlaybackReport),
    #[serde(rename = "error")]
    Error(ErrorReport),
    #[serde(rename = "playbackerror")]
    PlaybackError(ErrorReport),
    #[serde(rename = "connectionerror")]
    ConnectionError(ErrorReport),
    #[serde(other)]
    Other,
}

impl ReceiverReport {
    pub fn play_state(&self) -> Option<&PlayState> {
        match self {
            ReceiverReport::PlaybackStart(r)
            | ReceiverReport::PlaybackProgress(r)
            | ReceiverReport::PlaybackStop(r)
            | ReceiverReport::PlayStateChange(r) => Some(&r.data.play_state),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct PlaybackReport {
    #[serde(default)]
    pub data: ReportData,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ReportData {
    #[serde(rename = "PlayState", default)]
    pub play_state: PlayState,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct PlayState {
    #[serde(
        rename = "PositionTicks",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub position_ticks: Option<i64>,
    #[serde(rename = "IsPaused", default, skip_serializing_if = "Option::is_none")]
    pub is_paused: Option<bool>,
    #[serde(rename = "ItemId", default, skip_serializing_if = "Option::is_none")]
    pub item_id: Option<String>,
    #[serde(
        rename = "PlaySessionId",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub play_session_id: Option<String>,
    #[serde(
        rename = "PlayMethod",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub play_method: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ErrorReport {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use serde_json::{json, Value};

    fn envelope(body: CommandBody) -> ReceiverCommand {
        ReceiverCommand {
            body,
            server_address: "http://pharos:8096".into(),
            access_token: AccessToken::new("sekrit-token-123"),
            receiver_name: Some("Living Room TV".into()),
            user_id: None,
            device_id: None,
            server_id: None,
            server_version: None,
            max_bitrate: Some(20_000_000),
        }
    }

    #[test]
    fn play_now_matches_spike_example() {
        let cmd = envelope(CommandBody::PlayNow(PlayNowOptions {
            items: vec![PlayItem {
                id: "abc".into(),
                kind: "Movie".into(),
            }],
            start_index: 0,
            start_position_ticks: 0,
            media_source_id: Some("ms".into()),
            audio_stream_index: Some(1),
            subtitle_stream_index: Some(3),
        }));
        let expected = json!({
            "command": "PlayNow",
            "serverAddress": "http://pharos:8096",
            "accessToken": "sekrit-token-123",
            "receiverName": "Living Room TV",
            "maxBitrate": 20000000,
            "options": {
                "items": [{"Id": "abc", "Type": "Movie"}],
                "startIndex": 0,
                "startPositionTicks": 0,
                "mediaSourceId": "ms",
                "audioStreamIndex": 1,
                "subtitleStreamIndex": 3
            }
        });
        assert_eq!(serde_json::to_value(&cmd).unwrap(), expected);
        let back: ReceiverCommand = serde_json::from_value(expected).unwrap();
        assert_eq!(back, cmd);
    }

    #[test]
    fn option_less_commands_have_no_options_key() {
        for body in [
            CommandBody::Pause,
            CommandBody::Unpause,
            CommandBody::Stop,
            CommandBody::NextTrack,
            CommandBody::PreviousTrack,
        ] {
            let v = serde_json::to_value(envelope(body.clone())).unwrap();
            assert_eq!(v["command"], body.name());
            assert!(v.get("options").is_none(), "{body:?} sent options");
            let back: ReceiverCommand = serde_json::from_value(v).unwrap();
            assert_eq!(back.body, body);
        }
    }

    #[test]
    fn seek_and_audio_index_round_trip() {
        let seek =
            serde_json::to_value(envelope(CommandBody::Seek(SeekOptions { position: 120.0 })))
                .unwrap();
        assert_eq!(seek["options"], json!({"position": 120.0}));
        let audio = serde_json::to_value(envelope(CommandBody::SetAudioStreamIndex(
            AudioStreamOptions { index: 2 },
        )))
        .unwrap();
        assert_eq!(audio["command"], "SetAudioStreamIndex");
        assert_eq!(audio["options"], json!({"index": 2}));
        let back: ReceiverCommand = serde_json::from_value(seek).unwrap();
        assert_eq!(
            back.body,
            CommandBody::Seek(SeekOptions { position: 120.0 })
        );
    }

    #[test]
    fn minimal_command_decodes_without_optional_fields() {
        let cmd: ReceiverCommand = serde_json::from_value(json!({
            "command": "Pause",
            "serverAddress": "http://x",
            "accessToken": "t"
        }))
        .unwrap();
        assert_eq!(cmd.body, CommandBody::Pause);
        assert_eq!(cmd.receiver_name, None);
    }

    #[test]
    fn debug_never_leaks_access_token() {
        let cmd = envelope(CommandBody::Pause);
        let shown = format!("{cmd:?} {:?}", cmd.access_token);
        assert!(!shown.contains("sekrit-token-123"), "{shown}");
        assert!(shown.contains("[redacted]"));
        assert_eq!(cmd.access_token.expose(), "sekrit-token-123");
    }

    #[test]
    fn progress_report_decodes_with_unknown_fields() {
        let r: ReceiverReport = serde_json::from_value(json!({
            "type": "playbackprogress",
            "extra": [1, 2],
            "data": {
                "PlayState": {
                    "PositionTicks": 15000000,
                    "IsPaused": false,
                    "ItemId": "abc",
                    "PlaySessionId": "ps1",
                    "PlayMethod": "Transcode",
                    "VolumeLevel": 80,
                    "CanSeek": true
                },
                "NowPlayingItem": {"Id": "abc"}
            }
        }))
        .unwrap();
        let ps = r.play_state().unwrap();
        assert!(matches!(r, ReceiverReport::PlaybackProgress(_)));
        assert_eq!(ps.position_ticks, Some(15_000_000));
        assert_eq!(ps.is_paused, Some(false));
        assert_eq!(ps.item_id.as_deref(), Some("abc"));
        assert_eq!(ps.play_session_id.as_deref(), Some("ps1"));
        assert_eq!(ps.play_method.as_deref(), Some("Transcode"));
    }

    #[test]
    fn report_with_missing_optional_fields_decodes() {
        let r: ReceiverReport = serde_json::from_value(json!({
            "type": "playstatechange",
            "data": {"PlayState": {"IsPaused": true}}
        }))
        .unwrap();
        let ps = r.play_state().unwrap();
        assert_eq!(ps.is_paused, Some(true));
        assert_eq!(ps.position_ticks, None);
        assert_eq!(ps.play_session_id, None);

        let bare: ReceiverReport = serde_json::from_value(json!({"type": "playbackstop"})).unwrap();
        assert_eq!(bare.play_state(), Some(&PlayState::default()));
    }

    #[test]
    fn pascal_case_field_names_round_trip_exactly() {
        let report = ReceiverReport::PlaybackStart(PlaybackReport {
            data: ReportData {
                play_state: PlayState {
                    position_ticks: Some(1),
                    is_paused: Some(true),
                    item_id: Some("i".into()),
                    play_session_id: Some("p".into()),
                    play_method: Some("DirectPlay".into()),
                },
            },
        });
        let v = serde_json::to_value(&report).unwrap();
        let ps = &v["data"]["PlayState"];
        for key in [
            "PositionTicks",
            "IsPaused",
            "ItemId",
            "PlaySessionId",
            "PlayMethod",
        ] {
            assert!(ps.get(key).is_some(), "missing {key} in {v}");
        }
        assert_eq!(v["type"], "playbackstart");
        let back: ReceiverReport = serde_json::from_value(v).unwrap();
        assert_eq!(back, report);
    }

    #[test]
    fn error_reports_carry_message() {
        for (ty, want) in [
            ("error", "e"),
            ("playbackerror", "p"),
            ("connectionerror", "c"),
        ] {
            let r: ReceiverReport =
                serde_json::from_value(json!({"type": ty, "message": want})).unwrap();
            let msg = match &r {
                ReceiverReport::Error(e)
                | ReceiverReport::PlaybackError(e)
                | ReceiverReport::ConnectionError(e) => e.message.as_deref(),
                other => panic!("wrong variant {other:?}"),
            };
            assert_eq!(msg, Some(want));
            let v: Value = serde_json::to_value(&r).unwrap();
            assert_eq!(v["type"], ty);
        }
        let no_msg: ReceiverReport = serde_json::from_value(json!({"type": "error"})).unwrap();
        assert_eq!(no_msg, ReceiverReport::Error(ErrorReport { message: None }));
    }

    #[test]
    fn unknown_report_types_decode_as_other() {
        for ty in ["volumechange", "repeatmodechange", "somethingnew"] {
            let r: ReceiverReport =
                serde_json::from_value(json!({"type": ty, "data": {"x": 1}})).unwrap();
            assert_eq!(r, ReceiverReport::Other);
        }
    }
}
