//! CASTV2 wire format and transport: one protobuf message, 4-byte big-endian
//! length framing, TLS to port 8009.

use bytes::{Bytes, BytesMut};
use prost::Message;
use tokio_util::codec::{Decoder, Encoder, LengthDelimitedCodec};

/// Chromium's own receiver rejects frames above 64 KiB; 1 MiB leaves room for
/// large status payloads while a hostile length prefix cannot force a big
/// allocation.
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;

pub const NS_CONNECTION: &str = "urn:x-cast:com.google.cast.tp.connection";
pub const NS_HEARTBEAT: &str = "urn:x-cast:com.google.cast.tp.heartbeat";
pub const NS_RECEIVER: &str = "urn:x-cast:com.google.cast.receiver";
pub const NS_CONNECTSDK: &str = "urn:x-cast:com.connectsdk";

/// Field numbers and requiredness follow the public `cast_channel.proto`
/// (Chromium `components/cast_channel/proto`, as vendored by rust-cast):
///
/// ```text
/// required ProtocolVersion protocol_version = 1;   // CASTV2_1_0 = 0
/// required string source_id = 2;
/// required string destination_id = 3;
/// required string namespace = 4;
/// required PayloadType payload_type = 5;           // STRING = 0, BINARY = 1
/// optional string payload_utf8 = 6;
/// optional bytes payload_binary = 7;
/// ```
///
/// Fields 8 (`continued`) and 9 (`remaining_length`) belong to the deprecated
/// chunking versions and are not used by CASTV2_1_0.
#[derive(Clone, PartialEq, Message)]
pub struct CastMessage {
    #[prost(enumeration = "ProtocolVersion", required, tag = "1")]
    pub protocol_version: i32,
    #[prost(string, required, tag = "2")]
    pub source_id: String,
    #[prost(string, required, tag = "3")]
    pub destination_id: String,
    #[prost(string, required, tag = "4")]
    pub namespace: String,
    #[prost(enumeration = "PayloadType", required, tag = "5")]
    pub payload_type: i32,
    #[prost(string, optional, tag = "6")]
    pub payload_utf8: Option<String>,
    #[prost(bytes = "vec", optional, tag = "7")]
    pub payload_binary: Option<Vec<u8>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, prost::Enumeration)]
#[repr(i32)]
pub enum ProtocolVersion {
    Castv21_0 = 0,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, prost::Enumeration)]
#[repr(i32)]
pub enum PayloadType {
    String = 0,
    Binary = 1,
}

impl CastMessage {
    pub fn string(
        source_id: &str,
        destination_id: &str,
        namespace: &str,
        payload: impl Into<String>,
    ) -> Self {
        Self {
            protocol_version: ProtocolVersion::Castv21_0 as i32,
            source_id: source_id.to_owned(),
            destination_id: destination_id.to_owned(),
            namespace: namespace.to_owned(),
            payload_type: PayloadType::String as i32,
            payload_utf8: Some(payload.into()),
            payload_binary: None,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("cast frame exceeds the {MAX_FRAME_BYTES}-byte limit: {0}")]
    Codec(#[source] std::io::Error),
    #[error("cast frame is not a valid CastMessage: {0}")]
    Decode(#[from] prost::DecodeError),
}

impl From<std::io::Error> for FrameError {
    fn from(e: std::io::Error) -> Self {
        FrameError::Codec(e)
    }
}

/// Length-delimited codec for CASTV2: 4-byte big-endian prefix, bounded.
#[derive(Debug)]
pub struct CastCodec {
    inner: LengthDelimitedCodec,
}

impl Default for CastCodec {
    fn default() -> Self {
        Self {
            inner: LengthDelimitedCodec::builder()
                .big_endian()
                .length_field_length(4)
                .max_frame_length(MAX_FRAME_BYTES)
                .new_codec(),
        }
    }
}

impl Encoder<CastMessage> for CastCodec {
    type Error = FrameError;

    fn encode(&mut self, item: CastMessage, dst: &mut BytesMut) -> Result<(), FrameError> {
        let body = Bytes::from(item.encode_to_vec());
        Ok(self.inner.encode(body, dst)?)
    }
}

impl Decoder for CastCodec {
    type Item = CastMessage;
    type Error = FrameError;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<CastMessage>, FrameError> {
        match self.inner.decode(src)? {
            Some(frame) => Ok(Some(CastMessage::decode(frame.freeze())?)),
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod codec_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn sample() -> CastMessage {
        CastMessage::string("sender-0", "receiver-0", NS_HEARTBEAT, r#"{"type":"PING"}"#)
    }

    #[test]
    fn frame_round_trips_with_big_endian_prefix() {
        let mut codec = CastCodec::default();
        let mut buf = BytesMut::new();
        codec.encode(sample(), &mut buf).unwrap();
        let declared = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
        assert_eq!(declared, buf.len() - 4);
        let back = codec.decode(&mut buf).unwrap().unwrap();
        assert_eq!(back, sample());
        assert!(buf.is_empty());
    }

    #[test]
    fn partial_frame_waits_for_more_bytes() {
        let mut codec = CastCodec::default();
        let mut full = BytesMut::new();
        codec.encode(sample(), &mut full).unwrap();
        let mut head = full.split_to(full.len() - 3);
        assert!(codec.decode(&mut head).unwrap().is_none());
        head.extend_from_slice(&full);
        assert_eq!(codec.decode(&mut head).unwrap().unwrap(), sample());
    }

    #[test]
    fn oversized_length_is_rejected_without_allocating_it() {
        let mut codec = CastCodec::default();
        // Declares ~4 GiB but supplies 8 bytes: the check must fire on the
        // prefix alone, before any buffer of that size is reserved.
        let mut buf = BytesMut::from(&[0xFF, 0xFF, 0xFF, 0xF0, 1, 2, 3, 4][..]);
        let err = codec.decode(&mut buf).unwrap_err();
        assert!(matches!(err, FrameError::Codec(_)), "{err}");
        assert!(
            buf.capacity() < MAX_FRAME_BYTES,
            "capacity {}",
            buf.capacity()
        );
    }

    #[test]
    fn garbage_body_is_a_decode_error_not_a_panic() {
        let mut codec = CastCodec::default();
        let mut buf = BytesMut::from(&[0, 0, 0, 3, 0xFF, 0xFF, 0xFF][..]);
        let err = codec.decode(&mut buf).unwrap_err();
        assert!(matches!(err, FrameError::Decode(_)), "{err}");
    }

    #[test]
    fn wire_tags_match_cast_channel_proto() {
        // Field 1 varint 0, field 2 "a", field 3 "b", field 4 "n",
        // field 5 varint 0, field 6 "p".
        let m = CastMessage::string("a", "b", "n", "p");
        assert_eq!(
            m.encode_to_vec(),
            [0x08, 0, 0x12, 1, b'a', 0x1A, 1, b'b', 0x22, 1, b'n', 0x28, 0, 0x32, 1, b'p']
        );
    }
}
