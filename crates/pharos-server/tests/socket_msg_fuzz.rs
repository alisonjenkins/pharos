//! V17 + V18: SyncPlay socket message deserialisers never panic on
//! adversarial JSON.
//!
//! The socket pump is one tokio task per connected client. A panic
//! in the serde path kills the task, the client's `WebSocket.send`
//! starts erroring silently, and the user wedges on a frozen
//! SyncPlay UI. So: every from_str / from_value path must return
//! `Err`, never panic, on arbitrary input.
//!
//! Proptest generates two flavours:
//!   1. completely arbitrary UTF-8 strings (most reject as invalid
//!      JSON, but the rare valid-JSON-with-bad-shape inputs are the
//!      real catch).
//!   2. valid JSON object shells with random key/value combinations
//!      to stress the field-by-field deserialiser.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use pharos_server::api::jellyfin::socket_messages::{
    Inbound, SyncPlayBufferingData, SyncPlayJoinData, SyncPlayPlayData, SyncPlaySeekData,
};
use proptest::prelude::*;

fn json_object_strategy() -> impl Strategy<Value = serde_json::Value> {
    proptest::collection::hash_map(
        proptest::string::string_regex("[A-Za-z][A-Za-z0-9]{0,8}").unwrap(),
        prop_oneof![
            Just(serde_json::Value::Null),
            any::<bool>().prop_map(serde_json::Value::Bool),
            any::<i64>().prop_map(|n| serde_json::Value::from(n)),
            // String values are kept short so the resulting JSON
            // stays bounded.
            proptest::string::string_regex("[\\x20-\\x7e]{0,16}")
                .unwrap()
                .prop_map(serde_json::Value::String),
        ],
        0..6,
    )
    .prop_map(|m| {
        let mut o = serde_json::Map::new();
        for (k, v) in m {
            o.insert(k, v);
        }
        serde_json::Value::Object(o)
    })
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 512,
        max_shrink_iters: 256,
        .. ProptestConfig::default()
    })]

    /// 1. Arbitrary UTF-8 — most reject as malformed JSON, but the
    ///    rare valid-shaped ones still must not panic.
    #[test]
    fn inbound_from_arbitrary_string_never_panics(
        s in proptest::string::string_regex(".{0,256}").unwrap()
    ) {
        let _: Result<Inbound, _> = serde_json::from_str(&s);
    }

    /// 2. Valid JSON objects with random keys/values — exercises the
    ///    field-by-field deserialiser. None of these are valid
    ///    Inbound (missing `MessageType` will reject), but the
    ///    error path itself must not panic.
    #[test]
    fn inbound_from_random_object_never_panics(
        obj in json_object_strategy()
    ) {
        let s = obj.to_string();
        let _: Result<Inbound, _> = serde_json::from_str(&s);
    }

    /// 3. Specifically: a well-formed Inbound shell with random
    ///    `Data` payloads. This is the one the socket pump actually
    ///    feeds into `from_value::<SyncPlay*Data>` — the most
    ///    likely panic site if any.
    #[test]
    fn syncplay_subtypes_from_random_data_never_panic(
        data in json_object_strategy()
    ) {
        let _: Result<SyncPlayJoinData, _> = serde_json::from_value(data.clone());
        let _: Result<SyncPlayPlayData, _> = serde_json::from_value(data.clone());
        let _: Result<SyncPlaySeekData, _> = serde_json::from_value(data.clone());
        let _: Result<SyncPlayBufferingData, _> = serde_json::from_value(data);
    }
}
