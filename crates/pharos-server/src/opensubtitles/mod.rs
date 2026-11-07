//! OpenSubtitles.com subtitle search and download (spec 030, ADR-0069).
//!
//! The provider sits behind [`SubtitleProvider`] so handlers and the
//! background job are tested with a fake; nothing in a test touches the
//! network.

pub mod client;
pub mod hash;
pub mod quota;
pub mod store;
pub mod telemetry;

use std::future::Future;
use std::pin::Pin;

pub use quota::QuotaGate;

/// Boxed future so [`SubtitleProvider`] is object-safe (`Arc<dyn ...>`).
pub type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// What to look a subtitle up by. Ids are the numeric part only (`tt0111161`
/// is `111161`), as the API wants them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SearchQuery {
    pub moviehash: Option<String>,
    pub imdb_id: Option<u64>,
    pub tmdb_id: Option<u64>,
    /// For an episode: the series' ids, with `season` and `episode`.
    pub parent_imdb_id: Option<u64>,
    pub parent_tmdb_id: Option<u64>,
    pub season: Option<u32>,
    pub episode: Option<u32>,
    /// OpenSubtitles language code: `en`, `pt-BR`, `zh-CN`.
    pub language: String,
}

/// One downloadable subtitle file from a search.
#[derive(Debug, Clone, PartialEq)]
pub struct SubtitleHit {
    pub file_id: u64,
    pub subtitle_id: String,
    pub language: String,
    pub release: String,
    pub download_count: u32,
    pub ratings: f32,
    pub from_trusted: bool,
    pub hearing_impaired: bool,
    pub ai_translated: bool,
    pub machine_translated: bool,
    pub foreign_parts_only: bool,
    pub fps: Option<f32>,
    pub uploader: Option<String>,
    pub upload_date: Option<String>,
    pub hash_match: bool,
}

/// A downloaded subtitle file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Downloaded {
    pub file_name: String,
    pub bytes: Vec<u8>,
    /// Downloads left in the current 24 h window, when the API said.
    pub remaining: Option<u32>,
}

/// Why a provider call failed. Quota variants carry the unix-seconds instant
/// the provider said the limit lifts, so callers can park until then.
#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("opensubtitles download quota used up until unix {reset_unix}")]
    QuotaExceeded { reset_unix: i64 },
    #[error("opensubtitles rate limit hit; retry after {retry_after_secs}s")]
    RateLimited { retry_after_secs: u64 },
    #[error("opensubtitles refused the credentials (HTTP {status}): {message}")]
    Auth { status: u16, message: String },
    #[error("opensubtitles answered HTTP {status} to {op}")]
    Http { op: &'static str, status: u16 },
    #[error("opensubtitles request {op} failed: {source}")]
    Transport {
        op: &'static str,
        #[source]
        source: reqwest::Error,
    },
    #[error("opensubtitles response to {op} was not understood: {detail}")]
    Parse { op: &'static str, detail: String },
}

pub trait SubtitleProvider: Send + Sync {
    fn search<'a>(
        &'a self,
        query: &'a SearchQuery,
    ) -> BoxFut<'a, Result<Vec<SubtitleHit>, ProviderError>>;
    fn download(&self, file_id: u64) -> BoxFut<'_, Result<Downloaded, ProviderError>>;
}

/// The configured feature: a provider, the directory downloads are stored
/// under, and the shared quota gate. Present in `AppState` only when an API
/// key is set and a subtitle cache directory exists (V563, V564).
#[derive(Clone)]
pub struct OpenSubtitles {
    pub provider: std::sync::Arc<dyn SubtitleProvider>,
    /// `<subtitle cache dir>/opensubtitles`.
    pub root: std::path::PathBuf,
    pub gate: std::sync::Arc<QuotaGate>,
}

impl std::fmt::Debug for OpenSubtitles {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenSubtitles")
            .field("root", &self.root)
            .finish_non_exhaustive()
    }
}
