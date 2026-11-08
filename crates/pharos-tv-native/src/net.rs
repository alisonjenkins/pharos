//! Blitz's fetcher for the spike: images and other subresources from the
//! server, with the device token in a header and never in a URL (as the
//! WebView app's proxy). blitz-net is not used: it pulls in native-tls.

use blitz_traits::net::{NetHandler, NetProvider, NetWaker, Request};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::Semaphore;

/// Fetches in flight at once. Each poster is decoded as soon as it lands, so
/// this also bounds how many decodes queue up behind a grid's first paint.
const MAX_IN_FLIGHT: usize = 4;

pub struct PharosNet {
    client: reqwest::Client,
    origin: url::Origin,
    waker: Arc<dyn NetWaker>,
    slots: Arc<Semaphore>,
    rt: tokio::runtime::Handle,
}

impl PharosNet {
    pub fn new(
        server: &url::Url,
        waker: Arc<dyn NetWaker>,
        rt: tokio::runtime::Handle,
    ) -> Self {
        Self {
            client: reqwest::Client::new(),
            origin: server.origin(),
            waker,
            slots: Arc::new(Semaphore::new(MAX_IN_FLIGHT)),
            rt,
        }
    }
}

impl NetProvider for PharosNet {
    fn fetch(&self, doc_id: usize, request: Request, handler: Box<dyn NetHandler>) {
        let url = request.url;
        if url.origin() != self.origin {
            log::warn!("net: refused off-server fetch {}", url.as_str());
            return;
        }
        let client = self.client.clone();
        // Read per fetch: a QuickConnect sign-in stores it after launch.
        let token = pharos_ui::native::android::token();
        let waker = self.waker.clone();
        let slots = self.slots.clone();
        self.rt.spawn(async move {
            let Ok(_slot) = slots.acquire_owned().await else {
                return;
            };
            let started = Instant::now();
            let mut get = client.get(url.clone());
            if let Some(token) = &token {
                get = get.bearer_auth(token);
            }
            // The query is a public cache tag and a size, never a credential.
            let path = format!("{}?{}", url.path(), url.query().unwrap_or(""));
            match get.send().await {
                Ok(resp) => {
                    let status = resp.status().as_u16();
                    let kind = resp
                        .headers()
                        .get(reqwest::header::CONTENT_TYPE)
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("-")
                        .to_owned();
                    match resp.bytes().await {
                        Ok(bytes) => {
                            log::info!(
                                "bench fetch path={path} status={status} type={kind} bytes={} ms={}",
                                bytes.len(),
                                started.elapsed().as_millis()
                            );
                            if (200..300).contains(&status) {
                                handler.bytes(url.to_string(), bytes);
                            }
                        }
                        Err(e) => log::warn!("net: body path={path} status={status}: {e}"),
                    }
                }
                Err(e) => log::warn!("net: get path={path}: {e}"),
            }
            waker.wake(doc_id);
        });
    }
}
