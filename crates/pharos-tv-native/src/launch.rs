//! `dioxus_native::launch`, trimmed, with the spike's own fetcher and the
//! server as the document's base URL, so the shared components' same-origin
//! art paths (`/Items/..`) resolve to the server. dioxus-native 0.8 has no
//! hook to pass either in.

use blitz_shell::{BlitzShellProxy, WindowConfig, create_default_event_loop};
use blitz_traits::net::NetProvider;
use dioxus::prelude::*;
use dioxus_native::{
    DioxusDocument, DioxusNativeApplication, DioxusNativeWindowRenderer, DocumentConfig,
    RendererOptions,
};
use std::sync::Arc;

pub fn launch(app: fn() -> Element, server: url::Url) {
    let event_loop = create_default_event_loop();
    let (proxy, event_queue) = BlitzShellProxy::new(event_loop.create_proxy());

    // Fetches and their decodes run here, off the UI thread.
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            log::error!("launch: net runtime: {e}");
            return;
        }
    };
    let _guard = rt.enter();

    let vdom = VirtualDom::new(app);
    let net: Arc<dyn NetProvider> = Arc::new(crate::net::PharosNet::new(
        &server,
        Arc::new(proxy.clone()),
        rt.handle().clone(),
    ));
    vdom.provide_root_context(Arc::clone(&net));

    let doc = DioxusDocument::new(
        vdom,
        DocumentConfig {
            net_provider: Some(net),
            base_url: Some(server.to_string()),
            ..Default::default()
        },
    );
    let renderer = DioxusNativeWindowRenderer::with_options(RendererOptions::default());
    let config = WindowConfig::new(Box::new(doc) as _, renderer);
    let application = DioxusNativeApplication::new(proxy, event_queue, config);
    if let Err(e) = event_loop.run_app(application) {
        log::error!("launch: event loop: {e}");
    }
}
