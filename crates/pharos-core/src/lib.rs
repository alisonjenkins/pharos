//! pharos-core: domain traits at IO boundary (V12).
//! No IO impls here. Servers/adapters live in pharos-server and friends.

use async_trait::async_trait;
use std::path::PathBuf;

pub type MediaId = u64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaItem {
    pub id: MediaId,
    pub path: PathBuf,
    pub title: String,
    pub kind: MediaKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaKind {
    Movie,
    Episode,
    Audio,
}

#[derive(Debug, thiserror::Error)]
pub enum DomainError {
    #[error("not found: {0}")]
    NotFound(MediaId),
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("backend: {0}")]
    Backend(String),
}

pub type DomainResult<T> = Result<T, DomainError>;

#[async_trait]
pub trait MediaStore: Send + Sync {
    async fn get(&self, id: MediaId) -> DomainResult<MediaItem>;
    async fn put(&self, item: MediaItem) -> DomainResult<()>;
    async fn list(&self) -> DomainResult<Vec<MediaItem>>;
}

#[async_trait]
pub trait Scanner: Send + Sync {
    async fn scan(&self, root: &std::path::Path) -> DomainResult<Vec<MediaItem>>;
}

#[async_trait]
pub trait Transcoder: Send + Sync {
    async fn probe(&self, path: &std::path::Path) -> DomainResult<MediaKind>;
}

pub trait Clock: Send + Sync {
    fn now_unix_ms(&self) -> u64;
}

#[cfg(test)]
mod tests;
