//! Where the screens read the library from (spec 049, ADR-0149): the
//! server while it answers, else what this device holds.

use std::rc::Rc;

use pharos_client::api::{
    Capabilities, Decision, Detail, Home, ItemRef, ListIndex, Listing, Lyrics, NativeApi,
    NativeGrant, Page, PersonPage, PharosApi, StartTracks, TrackPicks,
};
use pharos_client::{CallInfo, ClientError};

use super::offline_library::{offline_error, OfflineLibrary};

/// One library to read: the server's, or the device's.
#[derive(Clone)]
pub enum Library {
    Server(NativeApi),
    Device(Rc<OfflineLibrary>),
}

impl Library {
    /// The server, when this reads from it.
    pub fn server(&self) -> Option<&NativeApi> {
        match self {
            Self::Server(api) => Some(api),
            Self::Device(_) => None,
        }
    }

    pub fn is_offline(&self) -> bool {
        matches!(self, Self::Device(_))
    }
}

impl PharosApi for Library {
    type Playing = NativeGrant;

    async fn home(&self, call: &CallInfo) -> Result<Home, ClientError> {
        match self {
            Self::Server(api) => api.home(call).await,
            Self::Device(lib) => lib.home(call).await,
        }
    }

    async fn views(&self, call: &CallInfo) -> Result<Page, ClientError> {
        match self {
            Self::Server(api) => api.views(call).await,
            Self::Device(lib) => lib.views(call).await,
        }
    }

    async fn query(
        &self,
        call: &CallInfo,
        listing: &Listing,
        start: u32,
        limit: u32,
    ) -> Result<Page, ClientError> {
        match self {
            Self::Server(api) => api.query(call, listing, start, limit).await,
            Self::Device(lib) => lib.query(call, listing, start, limit).await,
        }
    }

    async fn index(&self, call: &CallInfo, listing: &Listing) -> Result<ListIndex, ClientError> {
        match self {
            Self::Server(api) => api.index(call, listing).await,
            Self::Device(lib) => lib.index(call, listing).await,
        }
    }

    async fn search(&self, call: &CallInfo, text: &str, limit: u32) -> Result<Page, ClientError> {
        match self {
            Self::Server(api) => api.search(call, text, limit).await,
            Self::Device(lib) => lib.search(call, text, limit).await,
        }
    }

    async fn detail(&self, call: &CallInfo, id: ItemRef) -> Result<Detail, ClientError> {
        match self {
            Self::Server(api) => api.detail(call, id).await,
            Self::Device(lib) => lib.detail(call, id).await,
        }
    }

    async fn person(&self, call: &CallInfo, id: ItemRef) -> Result<PersonPage, ClientError> {
        match self {
            Self::Server(api) => api.person(call, id).await,
            Self::Device(lib) => lib.person(call, id).await,
        }
    }

    async fn detail_version(
        &self,
        call: &CallInfo,
        id: ItemRef,
        version: &str,
    ) -> Result<Detail, ClientError> {
        match self {
            Self::Server(api) => api.detail_version(call, id, version).await,
            Self::Device(lib) => lib.detail_version(call, id, version).await,
        }
    }

    async fn lyrics(
        &self,
        call: &CallInfo,
        id: ItemRef,
        want_words: bool,
    ) -> Result<Lyrics, ClientError> {
        match self {
            Self::Server(api) => api.lyrics(call, id, want_words).await,
            Self::Device(lib) => lib.lyrics(call, id, want_words).await,
        }
    }

    async fn mix(
        &self,
        call: &CallInfo,
        spec: &pharos_client::mix::MixSpec,
        recent: &[ItemRef],
        count: u32,
    ) -> Result<pharos_client::mix::MixBatch, ClientError> {
        match self {
            Self::Server(api) => api.mix(call, spec, recent, count).await,
            Self::Device(lib) => lib.mix(call, spec, recent, count).await,
        }
    }

    async fn play(
        &self,
        call: &CallInfo,
        id: ItemRef,
        version: &str,
        caps: &Capabilities,
        picks: TrackPicks,
    ) -> Result<(Decision, NativeGrant), ClientError> {
        match self {
            Self::Server(api) => api.play(call, id, version, caps, picks).await,
            Self::Device(_) => Err(offline_error("streaming")),
        }
    }

    async fn start_tracks(
        &self,
        call: &CallInfo,
        id: ItemRef,
        caps: &Capabilities,
    ) -> Result<StartTracks, ClientError> {
        match self {
            Self::Server(api) => api.start_tracks(call, id, caps).await,
            Self::Device(lib) => lib.start_tracks(call, id, caps).await,
        }
    }
}
