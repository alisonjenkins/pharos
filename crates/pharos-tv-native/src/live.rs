//! The live server for the spike: one thread owns the capnp connection (its
//! clients are `!Send` and `android.rs` drives them on a `LocalSet`) and
//! sends what the screens need to the UI over a channel.

use futures_channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use pharos_client::CallInfo;
use pharos_client::api::{Home, ItemRef, LibraryKind, NativeApi, Page, PharosApi};
use pharos_ui::native::{android, tv_page};
use std::sync::Mutex;

/// What the spike reports to its screens.
pub enum Event {
    /// A QuickConnect code waiting for approval in another signed-in client.
    Code(String),
    Ready { home: Home, grid: Page },
    Failed(String),
}

/// Posters the grid loads: spec 032's state (b) needs at least 100.
const GRID_LIMIT: u32 = 120;

/// The client name the server records and its session list shows, so this
/// build is told apart from the WebView app (spec 032, observability).
const DEVICE: &str = "pharos-android-tv-native";

static EVENTS: Mutex<Option<UnboundedReceiver<Event>>> = Mutex::new(None);

/// The receiver `start` made; `None` after the first call.
pub fn take_events() -> Option<UnboundedReceiver<Event>> {
    EVENTS.lock().ok().and_then(|mut r| r.take())
}

/// The server address the app uses, without a trailing slash.
pub fn server() -> String {
    tv_page::server_base(android::load_server().as_deref())
}

/// Start the connection thread. Call once, before launch.
pub fn start() {
    let (tx, rx) = unbounded();
    if let Ok(mut slot) = EVENTS.lock() {
        *slot = Some(rx);
    }
    let spawned = std::thread::Builder::new()
        .name("pharos-rpc".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(rt) => rt,
                Err(e) => {
                    let _ = tx.unbounded_send(Event::Failed(format!("rpc runtime: {e}")));
                    return;
                }
            };
            let local = tokio::task::LocalSet::new();
            local.block_on(&rt, async move {
                if let Err(e) = run(&tx).await {
                    log::warn!("live: {e}");
                    let _ = tx.unbounded_send(Event::Failed(e));
                }
                // Keep the connection for the item page and playback probes.
                std::future::pending::<()>().await;
            });
        });
    if let Err(e) = spawned {
        log::warn!("live: rpc thread: {e}");
    }
}

async fn run(tx: &UnboundedSender<Event>) -> Result<(), String> {
    let base = server();
    let info = CallInfo { device: DEVICE.into(), ..CallInfo::default() };
    let session = match android::load_token() {
        Some(token) => {
            let (client, system, closed) = android::connect(&base).await?;
            tokio::task::spawn_local(system);
            android::resume(&client, &closed, &info, &token)
                .await
                .map_err(|e| format!("resume server={base}: {e}"))?
        }
        None => {
            let signed = android::quick_connect_sign_in(&base, &info, |code| {
                let _ = tx.unbounded_send(Event::Code(code.to_owned()));
            })
            .await?;
            android::store_token(&signed.token)?;
            log::info!("live: signed in by quickconnect server={base}");
            signed.session
        }
    };
    let api = NativeApi::new(&session);
    let started = std::time::Instant::now();
    let home = api.home(&info).await.map_err(|e| format!("home: {e}"))?;
    let library = home
        .rails
        .iter()
        .map(|r| &r.library)
        .find(|l| l.library_kind == Some(LibraryKind::Movies))
        .or_else(|| home.rails.first().map(|r| &r.library))
        .map(|l| l.id.clone())
        .unwrap_or(ItemRef::Real(0));
    let grid = api
        .browse(&info, library, 0, GRID_LIMIT)
        .await
        .map_err(|e| format!("browse: {e}"))?;
    log::info!(
        "bench live_load_ms={} rails={} grid={} of {}",
        started.elapsed().as_millis(),
        home.rails.len(),
        grid.items.len(),
        grid.total
    );
    tx.unbounded_send(Event::Ready { home, grid })
        .map_err(|_| "ui gone".to_owned())
}
