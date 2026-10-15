//! In-memory model of the stock jellyfin-chromecast receiver, for tests.

use std::collections::VecDeque;
use std::io;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use tokio::sync::Notify;
use tokio::time::Instant;

use crate::messages::{
    CommandBody, PlayState, PlaybackReport, ReceiverCommand, ReceiverReport, ReportData,
};
use crate::transport::{AppSession, CastError, CastTransport, TransportEvent};

const TICKS_PER_MS: i64 = 10_000;
const DEFAULT_PROGRESS_INTERVAL: Duration = Duration::from_millis(1_500);

#[derive(Debug, Clone, Default)]
pub struct FakeReceiverConfig {
    /// Delay between `send` and the receiver acting on the command.
    pub command_latency_ms: u64,
    /// Constant offset added to every position the receiver reports.
    pub drift_ms: i64,
    /// Error added to the landing position of every `Seek` command.
    pub seek_error_ms: i64,
    pub refuse_connect: bool,
    /// Gap between progress reports; `None` is the stock receiver's 1.5 s.
    pub progress_interval_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FakeState {
    pub item_id: Option<String>,
    /// True position, without `drift_ms`.
    pub position_ms: i64,
    pub paused: bool,
    pub playing: bool,
}

#[derive(Debug)]
struct Inner {
    config: FakeReceiverConfig,
    commands: Vec<ReceiverCommand>,
    pending: Vec<(Instant, CommandBody)>,
    out: VecDeque<ReceiverReport>,
    item_id: Option<String>,
    play_session_id: Option<String>,
    base_ms: i64,
    anchor: Instant,
    paused: bool,
    playing: bool,
    next_progress: Option<Instant>,
    connected: bool,
    dropped: bool,
    sessions: u32,
}

impl Inner {
    fn progress_interval(&self) -> Duration {
        self.config
            .progress_interval_ms
            .map_or(DEFAULT_PROGRESS_INTERVAL, Duration::from_millis)
    }

    fn position_at(&self, at: Instant) -> i64 {
        if self.playing && !self.paused {
            let elapsed = at.saturating_duration_since(self.anchor).as_millis();
            self.base_ms
                .saturating_add(i64::try_from(elapsed).unwrap_or(i64::MAX))
        } else {
            self.base_ms
        }
    }

    fn rebase(&mut self, at: Instant, position_ms: i64) {
        self.base_ms = position_ms.max(0);
        self.anchor = at;
    }

    fn play_state_at(&self, at: Instant) -> PlayState {
        let reported = self.position_at(at).saturating_add(self.config.drift_ms);
        PlayState {
            position_ticks: Some(reported.saturating_mul(TICKS_PER_MS)),
            is_paused: Some(self.paused),
            item_id: self.item_id.clone(),
            play_session_id: self.play_session_id.clone(),
            play_method: Some("DirectPlay".to_owned()),
        }
    }

    fn report_at(&self, at: Instant) -> PlaybackReport {
        PlaybackReport {
            data: ReportData {
                play_state: self.play_state_at(at),
            },
        }
    }

    fn emit_change(&mut self, at: Instant) {
        let r = self.report_at(at);
        self.out.push_back(ReceiverReport::PlayStateChange(r));
    }

    fn start_item(&mut self, at: Instant, id: String, start_ms: i64) {
        self.sessions = self.sessions.saturating_add(1);
        self.item_id = Some(id);
        self.play_session_id = Some(format!("fake-session-{}", self.sessions));
        self.playing = true;
        self.paused = false;
        self.rebase(at, start_ms);
        self.next_progress = Some(at + self.progress_interval());
        let r = self.report_at(at);
        self.out.push_back(ReceiverReport::PlaybackStart(r));
        self.emit_change(at);
    }

    fn stop_item(&mut self, at: Instant) {
        let r = self.report_at(at);
        self.out.push_back(ReceiverReport::PlaybackStop(r));
        self.playing = false;
        self.paused = false;
        self.item_id = None;
        self.play_session_id = None;
        self.base_ms = 0;
        self.next_progress = None;
    }

    fn apply(&mut self, at: Instant, body: CommandBody) {
        match body {
            CommandBody::PlayNow(opts) => {
                let Some(first) = opts.items.first() else {
                    return;
                };
                let id = first.id.clone();
                self.start_item(at, id, opts.start_position_ticks / TICKS_PER_MS);
            }
            CommandBody::Stop => {
                if self.playing {
                    self.stop_item(at);
                }
            }
            _ if !self.playing => {}
            CommandBody::Pause => {
                if !self.paused {
                    let now_ms = self.position_at(at);
                    self.paused = true;
                    self.rebase(at, now_ms);
                    self.next_progress = None;
                }
                self.emit_change(at);
            }
            CommandBody::Unpause => {
                if self.paused {
                    self.paused = false;
                    let base = self.base_ms;
                    self.rebase(at, base);
                    self.next_progress = Some(at + self.progress_interval());
                }
                self.emit_change(at);
            }
            CommandBody::Seek(opts) => {
                let target = (opts.position * 1000.0).round() as i64;
                let landed = target.saturating_add(self.config.seek_error_ms);
                self.rebase(at, landed);
                self.emit_change(at);
            }
            CommandBody::NextTrack
            | CommandBody::PreviousTrack
            | CommandBody::SetAudioStreamIndex(_) => self.emit_change(at),
        }
    }

    fn next_due(&self) -> Option<Instant> {
        let cmd = self.pending.iter().map(|(t, _)| *t).min();
        match (cmd, self.next_progress) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    fn advance(&mut self, now: Instant) {
        while let Some(due) = self.next_due() {
            if due > now {
                break;
            }
            let cmd_due = self.pending.iter().map(|(t, _)| *t).min();
            if cmd_due == Some(due) {
                if let Some(idx) = self.pending.iter().position(|(t, _)| *t == due) {
                    let (_, body) = self.pending.remove(idx);
                    self.apply(due, body);
                }
            } else {
                let r = self.report_at(due);
                self.out.push_back(ReceiverReport::PlaybackProgress(r));
                self.next_progress = Some(due + self.progress_interval());
            }
        }
    }

    fn state(&self, now: Instant) -> FakeState {
        FakeState {
            item_id: self.item_id.clone(),
            position_ms: self.position_at(now),
            paused: self.paused,
            playing: self.playing,
        }
    }
}

#[derive(Debug)]
struct Shared {
    inner: Mutex<Inner>,
    wake: Notify,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Test-side view of a [`FakeReceiver`]: inspect what it received and drive the "TV remote".
#[derive(Debug, Clone)]
pub struct FakeReceiverHandle {
    shared: Arc<Shared>,
}

impl FakeReceiverHandle {
    pub fn commands(&self) -> Vec<ReceiverCommand> {
        self.shared.lock().commands.clone()
    }

    pub fn state(&self) -> FakeState {
        let now = Instant::now();
        let mut g = self.shared.lock();
        g.advance(now);
        g.state(now)
    }

    pub fn configure(&self, f: impl FnOnce(&mut FakeReceiverConfig)) {
        f(&mut self.shared.lock().config);
    }

    pub fn drop_connection(&self) {
        self.shared.lock().dropped = true;
        self.shared.wake.notify_one();
    }

    pub fn user_pause(&self) {
        self.remote(|g, at| {
            if g.playing && !g.paused {
                let now_ms = g.position_at(at);
                g.paused = true;
                g.rebase(at, now_ms);
                g.next_progress = None;
            }
            g.emit_change(at);
        });
    }

    pub fn user_unpause(&self) {
        self.remote(|g, at| {
            if g.playing && g.paused {
                g.paused = false;
                let base = g.base_ms;
                g.rebase(at, base);
                g.next_progress = Some(at + g.progress_interval());
            }
            g.emit_change(at);
        });
    }

    pub fn user_seek(&self, position: Duration) {
        self.remote(|g, at| {
            let ms = i64::try_from(position.as_millis()).unwrap_or(i64::MAX);
            g.rebase(at, ms);
            g.emit_change(at);
        });
    }

    pub fn user_stop(&self) {
        self.remote(|g, at| {
            if g.playing {
                g.stop_item(at);
            }
        });
    }

    fn remote(&self, f: impl FnOnce(&mut Inner, Instant)) {
        let now = Instant::now();
        let mut g = self.shared.lock();
        g.advance(now);
        f(&mut g, now);
        drop(g);
        self.shared.wake.notify_one();
    }
}

#[derive(Debug)]
pub struct FakeReceiver {
    shared: Arc<Shared>,
}

impl FakeReceiver {
    pub fn new(config: FakeReceiverConfig) -> (Self, FakeReceiverHandle) {
        let shared = Arc::new(Shared {
            inner: Mutex::new(Inner {
                config,
                commands: Vec::new(),
                pending: Vec::new(),
                out: VecDeque::new(),
                item_id: None,
                play_session_id: None,
                base_ms: 0,
                anchor: Instant::now(),
                paused: false,
                playing: false,
                next_progress: None,
                connected: false,
                dropped: false,
                sessions: 0,
            }),
            wake: Notify::new(),
        });
        (
            Self {
                shared: Arc::clone(&shared),
            },
            FakeReceiverHandle { shared },
        )
    }
}

impl CastTransport for FakeReceiver {
    async fn connect(&mut self) -> Result<(), CastError> {
        let mut g = self.shared.lock();
        if g.config.refuse_connect {
            return Err(CastError::Connect {
                addr: "fake-receiver".to_owned(),
                source: io::Error::new(io::ErrorKind::ConnectionRefused, "refuse_connect set"),
            });
        }
        g.connected = true;
        g.dropped = false;
        Ok(())
    }

    async fn launch(&mut self, app_id: &str) -> Result<AppSession, CastError> {
        let g = self.shared.lock();
        if !g.connected || g.dropped {
            return Err(CastError::Closed);
        }
        tracing::debug!(app_id, "fake receiver launched app");
        Ok(AppSession {
            transport_id: "fake-transport".to_owned(),
            session_id: "fake-app-session".to_owned(),
        })
    }

    async fn send(&mut self, command: &ReceiverCommand) -> Result<(), CastError> {
        let now = Instant::now();
        {
            let mut g = self.shared.lock();
            if !g.connected || g.dropped {
                return Err(CastError::Closed);
            }
            g.advance(now);
            g.commands.push(command.clone());
            let due = now + Duration::from_millis(g.config.command_latency_ms);
            g.pending.push((due, command.body.clone()));
            g.advance(now);
        }
        self.shared.wake.notify_one();
        Ok(())
    }

    async fn recv(&mut self) -> TransportEvent {
        loop {
            let next = {
                let now = Instant::now();
                let mut g = self.shared.lock();
                if !g.connected || g.dropped {
                    return TransportEvent::Closed {
                        reason: if g.dropped {
                            "connection dropped".to_owned()
                        } else {
                            "not connected".to_owned()
                        },
                    };
                }
                g.advance(now);
                if let Some(report) = g.out.pop_front() {
                    return TransportEvent::Report(report);
                }
                g.next_due()
            };
            match next {
                Some(at) => {
                    tokio::select! {
                        () = tokio::time::sleep_until(at) => {}
                        () = self.shared.wake.notified() => {}
                    }
                }
                None => self.shared.wake.notified().await,
            }
        }
    }

    async fn close(&mut self) {
        self.shared.lock().connected = false;
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::messages::{AccessToken, PlayItem, PlayNowOptions, SeekOptions};

    fn cmd(body: CommandBody) -> ReceiverCommand {
        ReceiverCommand {
            body,
            server_address: "http://pharos:8096".into(),
            access_token: AccessToken::new("t"),
            receiver_name: None,
            user_id: None,
            device_id: None,
            server_id: None,
            server_version: None,
            max_bitrate: None,
        }
    }

    fn play_now(start_ms: i64) -> ReceiverCommand {
        cmd(CommandBody::PlayNow(PlayNowOptions {
            items: vec![PlayItem {
                id: "item-1".into(),
                kind: "Movie".into(),
            }],
            start_index: 0,
            start_position_ticks: start_ms * TICKS_PER_MS,
            media_source_id: None,
            audio_stream_index: None,
            subtitle_stream_index: None,
        }))
    }

    async fn connected(config: FakeReceiverConfig) -> (FakeReceiver, FakeReceiverHandle) {
        let (mut rx, h) = FakeReceiver::new(config);
        rx.connect().await.unwrap();
        rx.launch("F007D354").await.unwrap();
        (rx, h)
    }

    async fn next_report(rx: &mut FakeReceiver) -> ReceiverReport {
        match rx.recv().await {
            TransportEvent::Report(r) => r,
            other => panic!("expected report, got {other:?}"),
        }
    }

    async fn drain_start(rx: &mut FakeReceiver) {
        assert!(matches!(
            next_report(rx).await,
            ReceiverReport::PlaybackStart(_)
        ));
        assert!(matches!(
            next_report(rx).await,
            ReceiverReport::PlayStateChange(_)
        ));
    }

    fn ticks(r: &ReceiverReport) -> i64 {
        r.play_state().unwrap().position_ticks.unwrap()
    }

    #[tokio::test(start_paused = true)]
    async fn play_now_records_item_and_starts_at_requested_position() {
        let (mut rx, h) = connected(FakeReceiverConfig::default()).await;
        rx.send(&play_now(10_000)).await.unwrap();
        drain_start(&mut rx).await;
        let s = h.state();
        assert_eq!(s.item_id.as_deref(), Some("item-1"));
        assert_eq!(s.position_ms, 10_000);
        assert!(s.playing && !s.paused);
        assert_eq!(h.commands().len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn progress_arrives_every_1500ms() {
        let (mut rx, _h) = connected(FakeReceiverConfig::default()).await;
        rx.send(&play_now(0)).await.unwrap();
        drain_start(&mut rx).await;
        let t0 = Instant::now();
        for n in 1..=3u32 {
            let r = next_report(&mut rx).await;
            assert!(matches!(r, ReceiverReport::PlaybackProgress(_)));
            assert_eq!(t0.elapsed(), Duration::from_millis(1_500) * n);
            assert_eq!(ticks(&r), i64::from(n) * 1_500 * TICKS_PER_MS);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn progress_interval_is_configurable() {
        let cfg = FakeReceiverConfig {
            progress_interval_ms: Some(400),
            ..Default::default()
        };
        let (mut rx, _h) = connected(cfg).await;
        rx.send(&play_now(0)).await.unwrap();
        drain_start(&mut rx).await;
        let t0 = Instant::now();
        let r = next_report(&mut rx).await;
        assert!(matches!(r, ReceiverReport::PlaybackProgress(_)));
        assert_eq!(t0.elapsed(), Duration::from_millis(400));
    }

    #[tokio::test(start_paused = true)]
    async fn recv_is_cancel_safe() {
        let (mut rx, _h) = connected(FakeReceiverConfig::default()).await;
        rx.send(&play_now(0)).await.unwrap();
        drain_start(&mut rx).await;
        let t0 = Instant::now();
        let early = tokio::time::timeout(Duration::from_millis(100), rx.recv()).await;
        assert!(early.is_err());
        let r = next_report(&mut rx).await;
        assert!(matches!(r, ReceiverReport::PlaybackProgress(_)));
        assert_eq!(t0.elapsed(), Duration::from_millis(1_500));
    }

    #[tokio::test(start_paused = true)]
    async fn command_is_applied_only_after_latency() {
        let cfg = FakeReceiverConfig {
            command_latency_ms: 200,
            ..Default::default()
        };
        let (mut rx, h) = connected(cfg).await;
        rx.send(&play_now(0)).await.unwrap();
        assert!(!h.state().playing);
        tokio::time::sleep(Duration::from_millis(199)).await;
        assert!(!h.state().playing);
        tokio::time::sleep(Duration::from_millis(1)).await;
        assert!(h.state().playing);

        drain_start(&mut rx).await;
        rx.send(&cmd(CommandBody::Pause)).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!h.state().paused);
        let r = next_report(&mut rx).await;
        assert!(matches!(r, ReceiverReport::PlayStateChange(_)));
        assert_eq!(r.play_state().unwrap().is_paused, Some(true));
        assert!(h.state().paused);
    }

    #[tokio::test(start_paused = true)]
    async fn drift_is_visible_in_reports_but_not_in_true_position() {
        let cfg = FakeReceiverConfig {
            drift_ms: 500,
            ..Default::default()
        };
        let (mut rx, h) = connected(cfg).await;
        rx.send(&play_now(10_000)).await.unwrap();
        drain_start(&mut rx).await;
        let r = next_report(&mut rx).await;
        assert_eq!(ticks(&r), (10_000 + 1_500 + 500) * TICKS_PER_MS);
        assert_eq!(h.state().position_ms, 11_500);
    }

    #[tokio::test(start_paused = true)]
    async fn seek_lands_with_configured_error() {
        let cfg = FakeReceiverConfig {
            seek_error_ms: 300,
            ..Default::default()
        };
        let (mut rx, h) = connected(cfg).await;
        rx.send(&play_now(0)).await.unwrap();
        drain_start(&mut rx).await;
        rx.send(&cmd(CommandBody::Seek(SeekOptions { position: 60.0 })))
            .await
            .unwrap();
        let r = next_report(&mut rx).await;
        assert!(matches!(r, ReceiverReport::PlayStateChange(_)));
        assert_eq!(ticks(&r), 60_300 * TICKS_PER_MS);
        assert_eq!(h.state().position_ms, 60_300);
    }

    #[tokio::test(start_paused = true)]
    async fn pause_freezes_position_and_unpause_resumes() {
        let (mut rx, h) = connected(FakeReceiverConfig::default()).await;
        rx.send(&play_now(0)).await.unwrap();
        drain_start(&mut rx).await;
        tokio::time::sleep(Duration::from_millis(1_000)).await;
        rx.send(&cmd(CommandBody::Pause)).await.unwrap();
        tokio::time::sleep(Duration::from_secs(10)).await;
        assert_eq!(h.state().position_ms, 1_000);
        rx.send(&cmd(CommandBody::Unpause)).await.unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(h.state().position_ms, 1_500);
        assert!(!h.state().paused);
    }

    #[tokio::test(start_paused = true)]
    async fn stop_clears_item_and_reports_playbackstop() {
        let (mut rx, h) = connected(FakeReceiverConfig::default()).await;
        rx.send(&play_now(0)).await.unwrap();
        drain_start(&mut rx).await;
        rx.send(&cmd(CommandBody::Stop)).await.unwrap();
        assert!(matches!(
            next_report(&mut rx).await,
            ReceiverReport::PlaybackStop(_)
        ));
        assert!(!h.state().playing);
        assert_eq!(h.state().item_id, None);
    }

    #[tokio::test(start_paused = true)]
    async fn next_and_previous_track_are_noops() {
        let (mut rx, h) = connected(FakeReceiverConfig::default()).await;
        rx.send(&play_now(5_000)).await.unwrap();
        drain_start(&mut rx).await;
        rx.send(&cmd(CommandBody::NextTrack)).await.unwrap();
        rx.send(&cmd(CommandBody::PreviousTrack)).await.unwrap();
        assert_eq!(h.state().item_id.as_deref(), Some("item-1"));
        assert_eq!(h.state().position_ms, 5_000);
        assert_eq!(h.commands().len(), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn dropped_connection_makes_recv_closed_and_send_fail() {
        let (mut rx, h) = connected(FakeReceiverConfig::default()).await;
        h.drop_connection();
        assert!(matches!(rx.recv().await, TransportEvent::Closed { .. }));
        assert!(matches!(
            rx.send(&cmd(CommandBody::Pause)).await,
            Err(CastError::Closed)
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn drop_wakes_a_blocked_recv() {
        let (mut rx, h) = connected(FakeReceiverConfig::default()).await;
        let waiter = tokio::spawn(async move { rx.recv().await });
        tokio::task::yield_now().await;
        h.drop_connection();
        assert!(matches!(
            waiter.await.unwrap(),
            TransportEvent::Closed { .. }
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn refused_connect_returns_connect_error() {
        let (mut rx, _h) = FakeReceiver::new(FakeReceiverConfig {
            refuse_connect: true,
            ..Default::default()
        });
        assert!(matches!(rx.connect().await, Err(CastError::Connect { .. })));
    }

    #[tokio::test(start_paused = true)]
    async fn user_pause_emits_paused_report_without_a_command() {
        let (mut rx, h) = connected(FakeReceiverConfig::default()).await;
        rx.send(&play_now(0)).await.unwrap();
        drain_start(&mut rx).await;
        h.user_pause();
        let r = next_report(&mut rx).await;
        assert!(matches!(r, ReceiverReport::PlayStateChange(_)));
        assert_eq!(r.play_state().unwrap().is_paused, Some(true));
        assert!(h.state().paused);
        assert_eq!(h.commands().len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn user_seek_and_stop_emit_reports() {
        let (mut rx, h) = connected(FakeReceiverConfig::default()).await;
        rx.send(&play_now(0)).await.unwrap();
        drain_start(&mut rx).await;
        h.user_seek(Duration::from_secs(90));
        let r = next_report(&mut rx).await;
        assert_eq!(ticks(&r), 90_000 * TICKS_PER_MS);
        h.user_stop();
        assert!(matches!(
            next_report(&mut rx).await,
            ReceiverReport::PlaybackStop(_)
        ));
        assert!(!h.state().playing);
    }
}
