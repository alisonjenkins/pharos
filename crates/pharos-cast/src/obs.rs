use crate::events::{CastEvent, CastReason};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CorrectionKind {
    Seek,
    Pause,
    Resume,
}

impl CorrectionKind {
    pub const ALL: [CorrectionKind; 3] = [
        CorrectionKind::Seek,
        CorrectionKind::Pause,
        CorrectionKind::Resume,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            CorrectionKind::Seek => "seek",
            CorrectionKind::Pause => "pause",
            CorrectionKind::Resume => "resume",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnState {
    Connecting,
    Loading,
    Following,
    Lost,
    Ended,
    Failed,
}

impl ConnState {
    pub const ALL: [ConnState; 6] = [
        ConnState::Connecting,
        ConnState::Loading,
        ConnState::Following,
        ConnState::Lost,
        ConnState::Ended,
        ConnState::Failed,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            ConnState::Connecting => "connecting",
            ConnState::Loading => "loading",
            ConnState::Following => "following",
            ConnState::Lost => "lost",
            ConnState::Ended => "ended",
            ConnState::Failed => "failed",
        }
    }
}

pub fn describe() {
    metrics::describe_counter!(
        "pharos_cast_seat_total",
        "Cast seat lifecycle decisions by event and reason"
    );
    metrics::describe_histogram!(
        "pharos_cast_drift_seconds",
        "Seconds between a cast TV's reported position and the group target"
    );
    metrics::describe_counter!(
        "pharos_cast_correction_total",
        "Corrections sent to a cast TV to bring it back to the group, by kind"
    );
    metrics::describe_gauge!(
        "pharos_cast_connection",
        "Cast TV connection state per configured TV; 1 for the current state"
    );
}

const fn is_failure(event: CastEvent) -> bool {
    matches!(
        event,
        CastEvent::Rejected | CastEvent::Lost | CastEvent::HandoffFailed
    )
}

pub fn record_cast_event(
    event: CastEvent,
    reason: CastReason,
    user: &str,
    sender_device: &str,
    tv: &str,
    group: &str,
    detail: &str,
) {
    metrics::counter!(
        "pharos_cast_seat_total",
        "event" => event.label(),
        "reason" => reason.label(),
    )
    .increment(1);
    if is_failure(event) {
        tracing::warn!(
            event = event.label(),
            reason = reason.label(),
            user,
            sender_device,
            tv,
            group,
            detail,
            "cast seat"
        );
    } else {
        tracing::info!(
            event = event.label(),
            reason = reason.label(),
            user,
            sender_device,
            tv,
            group,
            detail,
            "cast seat"
        );
    }
}

pub fn record_drift(seconds: f64) {
    metrics::histogram!("pharos_cast_drift_seconds").record(seconds);
}

pub fn record_correction(kind: CorrectionKind) {
    metrics::counter!("pharos_cast_correction_total", "kind" => kind.label()).increment(1);
}

pub fn set_connection(tv: &str, state: ConnState, value: f64) {
    metrics::gauge!("pharos_cast_connection", "tv" => tv.to_owned(), "state" => state.label())
        .set(value);
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use metrics_util::debugging::{DebugValue, DebuggingRecorder};
    use std::collections::HashSet;
    use std::sync::{Arc, Mutex};
    use tracing_subscriber::layer::SubscriberExt;

    #[test]
    fn every_correction_kind_label_is_distinct_and_snake_case() {
        let labels: Vec<&str> = CorrectionKind::ALL.iter().map(|k| k.label()).collect();
        assert_eq!(labels, ["seek", "pause", "resume"]);
        assert_eq!(labels.iter().collect::<HashSet<_>>().len(), labels.len());
    }

    #[test]
    fn every_conn_state_label_is_distinct_and_snake_case() {
        let labels: Vec<&str> = ConnState::ALL.iter().map(|s| s.label()).collect();
        assert_eq!(
            labels,
            [
                "connecting",
                "loading",
                "following",
                "lost",
                "ended",
                "failed"
            ]
        );
        assert_eq!(labels.iter().collect::<HashSet<_>>().len(), labels.len());
    }

    fn labels_of(key: &metrics::Key) -> Vec<(String, String)> {
        key.labels()
            .map(|l| (l.key().to_string(), l.value().to_string()))
            .collect()
    }

    #[test]
    fn seat_counter_carries_event_and_reason_labels_on_success_and_failure() {
        let recorder = DebuggingRecorder::new();
        let snapshotter = pharos_test_metrics::snapshotter(&recorder);
        metrics::with_local_recorder(&recorder, || {
            record_cast_event(
                CastEvent::Ended,
                CastReason::UserStop,
                "alice",
                "phone",
                "living-room",
                "g1",
                "",
            );
            record_cast_event(
                CastEvent::Rejected,
                CastReason::TvBusy,
                "alice",
                "phone",
                "bedroom",
                "g1",
                "bedroom",
            );
        });

        let mut seats: Vec<(Vec<(String, String)>, DebugValue)> = snapshotter
            .snapshot()
            .into_vec()
            .into_iter()
            .filter(|(ck, _, _, _)| ck.key().name() == "pharos_cast_seat_total")
            .map(|(ck, _, _, value)| (labels_of(ck.key()), value))
            .collect();
        seats.sort_by(|a, b| a.0.cmp(&b.0));

        let expected = |event: &str, reason: &str| {
            vec![
                ("event".to_string(), event.to_string()),
                ("reason".to_string(), reason.to_string()),
            ]
        };
        assert_eq!(seats.len(), 2, "one series per (event, reason): {seats:?}");
        assert_eq!(seats[0].0, expected("ended", "user_stop"));
        assert!(matches!(seats[0].1, DebugValue::Counter(1)));
        assert_eq!(seats[1].0, expected("rejected", "tv_busy"));
        assert!(matches!(seats[1].1, DebugValue::Counter(1)));
    }

    #[test]
    fn drift_correction_and_connection_series_are_recorded_with_bounded_labels() {
        let recorder = DebuggingRecorder::new();
        let snapshotter = pharos_test_metrics::snapshotter(&recorder);
        metrics::with_local_recorder(&recorder, || {
            record_drift(0.25);
            record_correction(CorrectionKind::Seek);
            record_correction(CorrectionKind::Seek);
            record_correction(CorrectionKind::Pause);
            set_connection("living-room", ConnState::Following, 1.0);
            set_connection("living-room", ConnState::Lost, 0.0);
        });

        let snapshot = snapshotter.snapshot().into_vec();
        let by_name = |name: &str| -> Vec<(Vec<(String, String)>, &DebugValue)> {
            snapshot
                .iter()
                .filter(|(ck, _, _, _)| ck.key().name() == name)
                .map(|(ck, _, _, v)| (labels_of(ck.key()), v))
                .collect()
        };

        let drift = by_name("pharos_cast_drift_seconds");
        assert_eq!(drift.len(), 1, "{drift:?}");
        assert!(drift[0].0.is_empty());
        assert!(matches!(&drift[0].1, DebugValue::Histogram(v) if v.len() == 1));

        let mut corrections = by_name("pharos_cast_correction_total");
        corrections.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(corrections.len(), 2, "{corrections:?}");
        assert_eq!(
            corrections[0].0,
            vec![("kind".to_string(), "pause".to_string())]
        );
        assert!(matches!(corrections[0].1, DebugValue::Counter(1)));
        assert_eq!(
            corrections[1].0,
            vec![("kind".to_string(), "seek".to_string())]
        );
        assert!(matches!(corrections[1].1, DebugValue::Counter(2)));

        let mut connection = by_name("pharos_cast_connection");
        connection.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(connection.len(), 2, "{connection:?}");
        assert_eq!(
            connection[0].0,
            vec![
                ("tv".to_string(), "living-room".to_string()),
                ("state".to_string(), "following".to_string())
            ]
        );
        assert!(matches!(connection[0].1, DebugValue::Gauge(g) if g.into_inner() == 1.0));
        assert_eq!(
            connection[1].0,
            vec![
                ("tv".to_string(), "living-room".to_string()),
                ("state".to_string(), "lost".to_string())
            ]
        );
        assert!(matches!(connection[1].1, DebugValue::Gauge(g) if g.into_inner() == 0.0));
    }

    #[derive(Clone)]
    struct VecWriter(Arc<Mutex<Vec<u8>>>);

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for VecWriter {
        type Writer = SharedBuf;
        fn make_writer(&'a self) -> Self::Writer {
            SharedBuf(self.0.clone())
        }
    }

    struct SharedBuf(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for SharedBuf {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn cast_seat_log_line_names_user_device_tv_group_and_detail_on_success_and_failure() {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::registry().with(
            tracing_subscriber::fmt::layer()
                .with_writer(VecWriter(buf.clone()))
                .with_ansi(false),
        );
        let _guard = tracing::subscriber::set_default(subscriber);

        record_cast_event(
            CastEvent::Started,
            CastReason::UserStop,
            "alice",
            "phone-1",
            "living-room",
            "group-7",
            "",
        );
        record_cast_event(
            CastEvent::Rejected,
            CastReason::TvBusy,
            "bob",
            "tablet-2",
            "bedroom-tv",
            "group-7",
            "bedroom-tv answered 503",
        );

        let text = String::from_utf8(
            buf.lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone(),
        )
        .expect("log output is utf-8");
        let lines: Vec<&str> = text.lines().filter(|l| l.contains("cast seat")).collect();
        assert_eq!(lines.len(), 2, "one log line per call: {text}");
        for (line, user, device, tv, group) in [
            (lines[0], "alice", "phone-1", "living-room", "group-7"),
            (lines[1], "bob", "tablet-2", "bedroom-tv", "group-7"),
        ] {
            assert!(line.contains(user), "user missing: {line}");
            assert!(line.contains(device), "sender_device missing: {line}");
            assert!(line.contains(tv), "tv missing: {line}");
            assert!(line.contains(group), "group missing: {line}");
        }
        assert!(lines[1].contains("503"), "detail missing: {}", lines[1]);
        assert!(
            lines[1].contains("WARN"),
            "failure must log at WARN: {}",
            lines[1]
        );
        assert!(
            lines[0].contains("INFO"),
            "success must log at INFO: {}",
            lines[0]
        );
    }
}
