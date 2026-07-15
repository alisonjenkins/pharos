//! Runtime-dispatching store enum: lets one binary pick SQLite or
//! Postgres at runtime by URL scheme instead of at compile time via
//! Cargo feature. Every method delegates to the active variant.

use crate::postgres::PostgresStore;
use crate::sqlite::SqliteStore;
use crate::{RuntimeConfig, ServerConfigStore, StoreError};
use pharos_core::{
    AuthResult, AuthToken, Collection, CollectionCount, CollectionStore, DomainResult, Fingerprint,
    GenreCount, GenreStore, ItemPerson, Library, LibraryKind, LibraryStore, MediaFacets, MediaId,
    MediaItem, MediaQuery, MediaStore, PersistedSyncGroup, PersistedTranscodeSession, Person,
    PersonCount, PersonRef, PersonStore, Playlist, PlaylistEntry, PlaylistStore, PreferenceStore,
    ScanState, SearchQuery, SecretString, Studio, StudioCount, StudioStore, SyncGroupStore, Tag,
    TagCount, TagStore, TokenRecord, TokenStore, TranscodeSessionStore, UserDataStore, UserId,
    UserItemData, UserPolicy, UserRecord, UserStore,
};

#[derive(Clone)]
pub enum AnyStore {
    Sqlite(SqliteStore),
    Postgres(PostgresStore),
}

impl AnyStore {
    /// Dispatch by URL scheme: `postgres://` / `postgresql://` -> Postgres, else SQLite.
    pub async fn connect(url: &str) -> Result<Self, StoreError> {
        if url.starts_with("postgres://") || url.starts_with("postgresql://") {
            Ok(Self::Postgres(PostgresStore::connect(url).await?))
        } else {
            Ok(Self::Sqlite(SqliteStore::connect(url).await?))
        }
    }
}

// ---------------------------------------------------------------------
// ServerConfigStore
// ---------------------------------------------------------------------
impl ServerConfigStore for AnyStore {
    async fn load_or_create_server_id(&self) -> Result<String, StoreError> {
        match self {
            AnyStore::Sqlite(s) => s.load_or_create_server_id().await,
            AnyStore::Postgres(p) => p.load_or_create_server_id().await,
        }
    }

    async fn load_runtime_config(&self) -> Result<RuntimeConfig, StoreError> {
        match self {
            AnyStore::Sqlite(s) => s.load_runtime_config().await,
            AnyStore::Postgres(p) => p.load_runtime_config().await,
        }
    }

    async fn set_runtime_config(&self, cfg: &RuntimeConfig) -> Result<(), StoreError> {
        match self {
            AnyStore::Sqlite(s) => s.set_runtime_config(cfg).await,
            AnyStore::Postgres(p) => p.set_runtime_config(cfg).await,
        }
    }

    async fn rename_library(&self, wire_id: &str, new_name: &str) -> Result<u64, StoreError> {
        match self {
            AnyStore::Sqlite(s) => s.rename_library(wire_id, new_name).await,
            AnyStore::Postgres(p) => p.rename_library(wire_id, new_name).await,
        }
    }

    async fn load_named_config(&self, key: &str) -> Result<Option<String>, StoreError> {
        match self {
            AnyStore::Sqlite(s) => s.load_named_config(key).await,
            AnyStore::Postgres(p) => p.load_named_config(key).await,
        }
    }

    async fn set_named_config(&self, key: &str, value: &str) -> Result<(), StoreError> {
        match self {
            AnyStore::Sqlite(s) => s.set_named_config(key, value).await,
            AnyStore::Postgres(p) => p.set_named_config(key, value).await,
        }
    }

    async fn distinct_series_keys(&self) -> Result<Vec<(Option<String>, String)>, StoreError> {
        match self {
            AnyStore::Sqlite(s) => s.distinct_series_keys().await,
            AnyStore::Postgres(p) => p.distinct_series_keys().await,
        }
    }

    async fn distinct_season_keys(&self) -> Result<Vec<(Option<String>, String, i64)>, StoreError> {
        match self {
            AnyStore::Sqlite(s) => s.distinct_season_keys().await,
            AnyStore::Postgres(p) => p.distinct_season_keys().await,
        }
    }

    async fn distinct_artist_names(&self) -> Result<Vec<String>, StoreError> {
        match self {
            AnyStore::Sqlite(s) => s.distinct_artist_names().await,
            AnyStore::Postgres(p) => p.distinct_artist_names().await,
        }
    }

    async fn distinct_album_names(&self) -> Result<Vec<String>, StoreError> {
        match self {
            AnyStore::Sqlite(s) => s.distinct_album_names().await,
            AnyStore::Postgres(p) => p.distinct_album_names().await,
        }
    }

    async fn distinct_genre_fields(&self) -> Result<Vec<String>, StoreError> {
        match self {
            AnyStore::Sqlite(s) => s.distinct_genre_fields().await,
            AnyStore::Postgres(p) => p.distinct_genre_fields().await,
        }
    }
}

// ---------------------------------------------------------------------
// MediaStore
// ---------------------------------------------------------------------
impl MediaStore for AnyStore {
    async fn get(&self, id: MediaId) -> DomainResult<MediaItem> {
        match self {
            AnyStore::Sqlite(s) => MediaStore::get(s, id).await,
            AnyStore::Postgres(p) => MediaStore::get(p, id).await,
        }
    }

    async fn put(&self, item: MediaItem) -> DomainResult<()> {
        match self {
            AnyStore::Sqlite(s) => s.put(item).await,
            AnyStore::Postgres(p) => p.put(item).await,
        }
    }

    async fn list(&self) -> DomainResult<Vec<MediaItem>> {
        match self {
            AnyStore::Sqlite(s) => MediaStore::list(s).await,
            AnyStore::Postgres(p) => MediaStore::list(p).await,
        }
    }

    async fn query(&self, q: &MediaQuery) -> DomainResult<(Vec<MediaItem>, u64)> {
        match self {
            AnyStore::Sqlite(s) => s.query(q).await,
            AnyStore::Postgres(p) => p.query(q).await,
        }
    }

    async fn search(&self, q: &SearchQuery) -> DomainResult<(Vec<MediaItem>, u64)> {
        match self {
            AnyStore::Sqlite(s) => s.search(q).await,
            AnyStore::Postgres(p) => p.search(q).await,
        }
    }

    async fn facets(
        &self,
        base: &MediaQuery,
        req: &pharos_core::FacetRequest,
    ) -> DomainResult<MediaFacets> {
        match self {
            AnyStore::Sqlite(s) => s.facets(base, req).await,
            AnyStore::Postgres(p) => p.facets(base, req).await,
        }
    }

    async fn scan_state(&self, id: MediaId) -> DomainResult<Option<ScanState>> {
        match self {
            AnyStore::Sqlite(s) => s.scan_state(id).await,
            AnyStore::Postgres(p) => p.scan_state(id).await,
        }
    }

    async fn begin_scan(&self, root: &std::path::Path) -> DomainResult<i64> {
        match self {
            AnyStore::Sqlite(s) => s.begin_scan(root).await,
            AnyStore::Postgres(p) => p.begin_scan(root).await,
        }
    }

    async fn mark_seen(
        &self,
        id: MediaId,
        scan_id: i64,
        mtime: i64,
        size: u64,
    ) -> DomainResult<()> {
        match self {
            AnyStore::Sqlite(s) => s.mark_seen(id, scan_id, mtime, size).await,
            AnyStore::Postgres(p) => p.mark_seen(id, scan_id, mtime, size).await,
        }
    }

    async fn mark_seen_batch(
        &self,
        items: &[(MediaId, i64, u64)],
        scan_id: i64,
    ) -> DomainResult<()> {
        match self {
            AnyStore::Sqlite(s) => s.mark_seen_batch(items, scan_id).await,
            AnyStore::Postgres(p) => p.mark_seen_batch(items, scan_id).await,
        }
    }

    async fn sweep_unseen(&self, scan_id: i64, root_prefix: &str) -> DomainResult<Vec<MediaId>> {
        match self {
            AnyStore::Sqlite(s) => s.sweep_unseen(scan_id, root_prefix).await,
            AnyStore::Postgres(p) => p.sweep_unseen(scan_id, root_prefix).await,
        }
    }

    async fn finish_scan(
        &self,
        scan_id: i64,
        items_seen: i64,
        items_swept: i64,
    ) -> DomainResult<()> {
        match self {
            AnyStore::Sqlite(s) => s.finish_scan(scan_id, items_seen, items_swept).await,
            AnyStore::Postgres(p) => p.finish_scan(scan_id, items_seen, items_swept).await,
        }
    }

    async fn find_by_fp(&self, fp: Fingerprint) -> DomainResult<Option<MediaItem>> {
        match self {
            AnyStore::Sqlite(s) => s.find_by_fp(fp).await,
            AnyStore::Postgres(p) => p.find_by_fp(fp).await,
        }
    }

    async fn set_fingerprint(&self, id: MediaId, fp: Fingerprint) -> DomainResult<()> {
        match self {
            AnyStore::Sqlite(s) => s.set_fingerprint(id, fp).await,
            AnyStore::Postgres(p) => p.set_fingerprint(id, fp).await,
        }
    }

    async fn rebind_path(&self, id: MediaId, new_path: &std::path::Path) -> DomainResult<()> {
        match self {
            AnyStore::Sqlite(s) => s.rebind_path(id, new_path).await,
            AnyStore::Postgres(p) => p.rebind_path(id, new_path).await,
        }
    }

    async fn set_artwork(
        &self,
        item_id: MediaId,
        role: &str,
        source: &str,
        locator: &str,
    ) -> DomainResult<()> {
        match self {
            AnyStore::Sqlite(s) => s.set_artwork(item_id, role, source, locator).await,
            AnyStore::Postgres(p) => p.set_artwork(item_id, role, source, locator).await,
        }
    }

    async fn artwork_for(&self, item_id: MediaId) -> DomainResult<Vec<(String, String, String)>> {
        match self {
            AnyStore::Sqlite(s) => s.artwork_for(item_id).await,
            AnyStore::Postgres(p) => p.artwork_for(item_id).await,
        }
    }
}

// ---------------------------------------------------------------------
// MediaSegmentStore (T86 / ADR-0018)
// ---------------------------------------------------------------------
impl pharos_core::MediaSegmentStore for AnyStore {
    async fn set_media_segments(
        &self,
        item_id: MediaId,
        segments: &[pharos_core::DetectedSegment],
        schema_version: i64,
    ) -> DomainResult<()> {
        match self {
            AnyStore::Sqlite(s) => {
                s.set_media_segments(item_id, segments, schema_version)
                    .await
            }
            AnyStore::Postgres(p) => {
                p.set_media_segments(item_id, segments, schema_version)
                    .await
            }
        }
    }
    async fn media_segments_for(
        &self,
        item_id: MediaId,
    ) -> DomainResult<Vec<pharos_core::DetectedSegment>> {
        match self {
            AnyStore::Sqlite(s) => s.media_segments_for(item_id).await,
            AnyStore::Postgres(p) => p.media_segments_for(item_id).await,
        }
    }
    async fn set_episode_fingerprint(
        &self,
        item_id: MediaId,
        kind: pharos_core::FingerprintKind,
        points: &[u32],
        schema_version: i64,
    ) -> DomainResult<()> {
        match self {
            AnyStore::Sqlite(s) => {
                s.set_episode_fingerprint(item_id, kind, points, schema_version)
                    .await
            }
            AnyStore::Postgres(p) => {
                p.set_episode_fingerprint(item_id, kind, points, schema_version)
                    .await
            }
        }
    }
    async fn episode_fingerprint_for(
        &self,
        item_id: MediaId,
        kind: pharos_core::FingerprintKind,
        schema_version: i64,
    ) -> DomainResult<Option<Vec<u32>>> {
        match self {
            AnyStore::Sqlite(s) => {
                s.episode_fingerprint_for(item_id, kind, schema_version)
                    .await
            }
            AnyStore::Postgres(p) => {
                p.episode_fingerprint_for(item_id, kind, schema_version)
                    .await
            }
        }
    }
}

// ---------------------------------------------------------------------
// GenreStore
// ---------------------------------------------------------------------
impl GenreStore for AnyStore {
    async fn upsert_genre(&self, name: &str) -> DomainResult<i64> {
        match self {
            AnyStore::Sqlite(s) => s.upsert_genre(name).await,
            AnyStore::Postgres(p) => p.upsert_genre(name).await,
        }
    }

    async fn link_item_genres(&self, item: MediaId, names: &[String]) -> DomainResult<()> {
        match self {
            AnyStore::Sqlite(s) => s.link_item_genres(item, names).await,
            AnyStore::Postgres(p) => p.link_item_genres(item, names).await,
        }
    }

    async fn genres_with_counts(&self) -> DomainResult<Vec<GenreCount>> {
        match self {
            AnyStore::Sqlite(s) => s.genres_with_counts().await,
            AnyStore::Postgres(p) => p.genres_with_counts().await,
        }
    }

    async fn item_ids_for_genre(&self, wire_id: &str) -> DomainResult<Vec<MediaId>> {
        match self {
            AnyStore::Sqlite(s) => s.item_ids_for_genre(wire_id).await,
            AnyStore::Postgres(p) => p.item_ids_for_genre(wire_id).await,
        }
    }

    async fn backfill_genres(&self) -> DomainResult<u64> {
        match self {
            AnyStore::Sqlite(s) => s.backfill_genres().await,
            AnyStore::Postgres(p) => p.backfill_genres().await,
        }
    }
}

// ---------------------------------------------------------------------
// PersonStore
// ---------------------------------------------------------------------
impl PersonStore for AnyStore {
    async fn upsert_person(
        &self,
        name: &str,
        sort_name: Option<&str>,
        provider_ids: Option<&str>,
        thumb_url: Option<&str>,
    ) -> DomainResult<i64> {
        match self {
            AnyStore::Sqlite(s) => {
                s.upsert_person(name, sort_name, provider_ids, thumb_url)
                    .await
            }
            AnyStore::Postgres(p) => {
                p.upsert_person(name, sort_name, provider_ids, thumb_url)
                    .await
            }
        }
    }

    async fn link_item_people(&self, item: MediaId, people: &[PersonRef]) -> DomainResult<()> {
        match self {
            AnyStore::Sqlite(s) => s.link_item_people(item, people).await,
            AnyStore::Postgres(p) => p.link_item_people(item, people).await,
        }
    }

    async fn people_with_counts(&self) -> DomainResult<Vec<PersonCount>> {
        match self {
            AnyStore::Sqlite(s) => s.people_with_counts().await,
            AnyStore::Postgres(p) => p.people_with_counts().await,
        }
    }

    async fn person_by_wire_id(&self, wire_id: &str) -> DomainResult<Option<Person>> {
        match self {
            AnyStore::Sqlite(s) => s.person_by_wire_id(wire_id).await,
            AnyStore::Postgres(p) => p.person_by_wire_id(wire_id).await,
        }
    }

    async fn people_needing_images(&self, limit: i64) -> DomainResult<Vec<Person>> {
        match self {
            AnyStore::Sqlite(s) => s.people_needing_images(limit).await,
            AnyStore::Postgres(p) => p.people_needing_images(limit).await,
        }
    }

    async fn item_ids_for_person(&self, wire_id: &str) -> DomainResult<Vec<MediaId>> {
        match self {
            AnyStore::Sqlite(s) => s.item_ids_for_person(wire_id).await,
            AnyStore::Postgres(p) => p.item_ids_for_person(wire_id).await,
        }
    }

    async fn people_for_item(&self, item: MediaId) -> DomainResult<Vec<ItemPerson>> {
        match self {
            AnyStore::Sqlite(s) => s.people_for_item(item).await,
            AnyStore::Postgres(p) => p.people_for_item(item).await,
        }
    }
}

// ---------------------------------------------------------------------
// StudioStore
// ---------------------------------------------------------------------
impl StudioStore for AnyStore {
    async fn upsert_studio(&self, name: &str) -> DomainResult<i64> {
        match self {
            AnyStore::Sqlite(s) => s.upsert_studio(name).await,
            AnyStore::Postgres(p) => p.upsert_studio(name).await,
        }
    }

    async fn link_item_studios(&self, item: MediaId, names: &[String]) -> DomainResult<()> {
        match self {
            AnyStore::Sqlite(s) => s.link_item_studios(item, names).await,
            AnyStore::Postgres(p) => p.link_item_studios(item, names).await,
        }
    }

    async fn studios_with_counts(&self) -> DomainResult<Vec<StudioCount>> {
        match self {
            AnyStore::Sqlite(s) => s.studios_with_counts().await,
            AnyStore::Postgres(p) => p.studios_with_counts().await,
        }
    }

    async fn item_ids_for_studio(&self, wire_id: &str) -> DomainResult<Vec<MediaId>> {
        match self {
            AnyStore::Sqlite(s) => s.item_ids_for_studio(wire_id).await,
            AnyStore::Postgres(p) => p.item_ids_for_studio(wire_id).await,
        }
    }

    async fn studios_for_item(&self, item: MediaId) -> DomainResult<Vec<Studio>> {
        match self {
            AnyStore::Sqlite(s) => s.studios_for_item(item).await,
            AnyStore::Postgres(p) => p.studios_for_item(item).await,
        }
    }
}

// ---------------------------------------------------------------------
// TagStore
// ---------------------------------------------------------------------
impl TagStore for AnyStore {
    async fn upsert_tag(&self, name: &str) -> DomainResult<i64> {
        match self {
            AnyStore::Sqlite(s) => s.upsert_tag(name).await,
            AnyStore::Postgres(p) => p.upsert_tag(name).await,
        }
    }

    async fn link_item_tags(&self, item: MediaId, names: &[String]) -> DomainResult<()> {
        match self {
            AnyStore::Sqlite(s) => s.link_item_tags(item, names).await,
            AnyStore::Postgres(p) => p.link_item_tags(item, names).await,
        }
    }

    async fn add_item_tags(&self, item: MediaId, names: &[String]) -> DomainResult<u64> {
        match self {
            AnyStore::Sqlite(s) => s.add_item_tags(item, names).await,
            AnyStore::Postgres(p) => p.add_item_tags(item, names).await,
        }
    }

    async fn remove_item_tags(&self, item: MediaId, names: &[String]) -> DomainResult<u64> {
        match self {
            AnyStore::Sqlite(s) => s.remove_item_tags(item, names).await,
            AnyStore::Postgres(p) => p.remove_item_tags(item, names).await,
        }
    }

    async fn tags_with_counts(&self) -> DomainResult<Vec<TagCount>> {
        match self {
            AnyStore::Sqlite(s) => s.tags_with_counts().await,
            AnyStore::Postgres(p) => p.tags_with_counts().await,
        }
    }

    async fn item_ids_for_tag(&self, wire_id: &str) -> DomainResult<Vec<MediaId>> {
        match self {
            AnyStore::Sqlite(s) => s.item_ids_for_tag(wire_id).await,
            AnyStore::Postgres(p) => p.item_ids_for_tag(wire_id).await,
        }
    }

    async fn tags_for_item(&self, item: MediaId) -> DomainResult<Vec<Tag>> {
        match self {
            AnyStore::Sqlite(s) => s.tags_for_item(item).await,
            AnyStore::Postgres(p) => p.tags_for_item(item).await,
        }
    }
}

// ---------------------------------------------------------------------
// CollectionStore
// ---------------------------------------------------------------------
impl CollectionStore for AnyStore {
    async fn upsert_collection(
        &self,
        name: &str,
        kind: Option<&str>,
        overview: Option<&str>,
    ) -> DomainResult<i64> {
        match self {
            AnyStore::Sqlite(s) => s.upsert_collection(name, kind, overview).await,
            AnyStore::Postgres(p) => p.upsert_collection(name, kind, overview).await,
        }
    }

    async fn link_item_collections(&self, item: MediaId, names: &[String]) -> DomainResult<()> {
        match self {
            AnyStore::Sqlite(s) => s.link_item_collections(item, names).await,
            AnyStore::Postgres(p) => p.link_item_collections(item, names).await,
        }
    }

    async fn collections_with_counts(&self) -> DomainResult<Vec<CollectionCount>> {
        match self {
            AnyStore::Sqlite(s) => s.collections_with_counts().await,
            AnyStore::Postgres(p) => p.collections_with_counts().await,
        }
    }

    async fn collection_by_wire_id(&self, wire_id: &str) -> DomainResult<Option<Collection>> {
        match self {
            AnyStore::Sqlite(s) => s.collection_by_wire_id(wire_id).await,
            AnyStore::Postgres(p) => p.collection_by_wire_id(wire_id).await,
        }
    }

    async fn collection_items(&self, wire_id: &str) -> DomainResult<Vec<MediaId>> {
        match self {
            AnyStore::Sqlite(s) => s.collection_items(wire_id).await,
            AnyStore::Postgres(p) => p.collection_items(wire_id).await,
        }
    }

    async fn create_collection(
        &self,
        name: &str,
        item_ids: &[MediaId],
    ) -> DomainResult<Collection> {
        match self {
            AnyStore::Sqlite(s) => s.create_collection(name, item_ids).await,
            AnyStore::Postgres(p) => p.create_collection(name, item_ids).await,
        }
    }

    async fn add_collection_items(
        &self,
        wire_id: &str,
        item_ids: &[MediaId],
    ) -> DomainResult<Option<u64>> {
        match self {
            AnyStore::Sqlite(s) => s.add_collection_items(wire_id, item_ids).await,
            AnyStore::Postgres(p) => p.add_collection_items(wire_id, item_ids).await,
        }
    }

    async fn remove_collection_items(
        &self,
        wire_id: &str,
        item_ids: &[MediaId],
    ) -> DomainResult<Option<u64>> {
        match self {
            AnyStore::Sqlite(s) => s.remove_collection_items(wire_id, item_ids).await,
            AnyStore::Postgres(p) => p.remove_collection_items(wire_id, item_ids).await,
        }
    }
}

// ---------------------------------------------------------------------
// PlaylistStore
// ---------------------------------------------------------------------
impl PlaylistStore for AnyStore {
    async fn create_playlist(
        &self,
        name: &str,
        owner_user_id: Option<&str>,
        media_type: &str,
        item_ids: &[MediaId],
    ) -> DomainResult<Playlist> {
        match self {
            AnyStore::Sqlite(s) => {
                s.create_playlist(name, owner_user_id, media_type, item_ids)
                    .await
            }
            AnyStore::Postgres(p) => {
                p.create_playlist(name, owner_user_id, media_type, item_ids)
                    .await
            }
        }
    }

    async fn playlist_by_wire_id(&self, wire_id: &str) -> DomainResult<Option<Playlist>> {
        match self {
            AnyStore::Sqlite(s) => s.playlist_by_wire_id(wire_id).await,
            AnyStore::Postgres(p) => p.playlist_by_wire_id(wire_id).await,
        }
    }

    async fn playlists_for_owner(
        &self,
        owner_user_id: Option<&str>,
    ) -> DomainResult<Vec<Playlist>> {
        match self {
            AnyStore::Sqlite(s) => s.playlists_for_owner(owner_user_id).await,
            AnyStore::Postgres(p) => p.playlists_for_owner(owner_user_id).await,
        }
    }

    async fn playlist_entries(&self, wire_id: &str) -> DomainResult<Vec<PlaylistEntry>> {
        match self {
            AnyStore::Sqlite(s) => s.playlist_entries(wire_id).await,
            AnyStore::Postgres(p) => p.playlist_entries(wire_id).await,
        }
    }

    async fn add_playlist_items(
        &self,
        wire_id: &str,
        item_ids: &[MediaId],
    ) -> DomainResult<Option<u64>> {
        match self {
            AnyStore::Sqlite(s) => s.add_playlist_items(wire_id, item_ids).await,
            AnyStore::Postgres(p) => p.add_playlist_items(wire_id, item_ids).await,
        }
    }

    async fn remove_playlist_entries(
        &self,
        wire_id: &str,
        entry_ids: &[String],
    ) -> DomainResult<Option<u64>> {
        match self {
            AnyStore::Sqlite(s) => s.remove_playlist_entries(wire_id, entry_ids).await,
            AnyStore::Postgres(p) => p.remove_playlist_entries(wire_id, entry_ids).await,
        }
    }

    async fn move_playlist_entry(
        &self,
        wire_id: &str,
        entry_id: &str,
        new_index: usize,
    ) -> DomainResult<Option<bool>> {
        match self {
            AnyStore::Sqlite(s) => s.move_playlist_entry(wire_id, entry_id, new_index).await,
            AnyStore::Postgres(p) => p.move_playlist_entry(wire_id, entry_id, new_index).await,
        }
    }

    async fn delete_playlist(&self, wire_id: &str) -> DomainResult<Option<()>> {
        match self {
            AnyStore::Sqlite(s) => s.delete_playlist(wire_id).await,
            AnyStore::Postgres(p) => p.delete_playlist(wire_id).await,
        }
    }
}

// ---------------------------------------------------------------------
// LibraryStore
// ---------------------------------------------------------------------
impl LibraryStore for AnyStore {
    async fn upsert_library(
        &self,
        name: &str,
        root_path: &str,
        kind: LibraryKind,
        wire_id: &str,
    ) -> DomainResult<i64> {
        match self {
            AnyStore::Sqlite(s) => s.upsert_library(name, root_path, kind, wire_id).await,
            AnyStore::Postgres(p) => p.upsert_library(name, root_path, kind, wire_id).await,
        }
    }

    async fn libraries(&self) -> DomainResult<Vec<Library>> {
        match self {
            AnyStore::Sqlite(s) => s.libraries().await,
            AnyStore::Postgres(p) => p.libraries().await,
        }
    }

    async fn delete_library(&self, root_path: &str) -> DomainResult<()> {
        match self {
            AnyStore::Sqlite(s) => s.delete_library(root_path).await,
            AnyStore::Postgres(p) => p.delete_library(root_path).await,
        }
    }

    async fn backfill_library_ids(&self) -> DomainResult<u64> {
        match self {
            AnyStore::Sqlite(s) => s.backfill_library_ids().await,
            AnyStore::Postgres(p) => p.backfill_library_ids().await,
        }
    }

    async fn item_ids_for_library(&self, wire_id: &str) -> DomainResult<Vec<MediaId>> {
        match self {
            AnyStore::Sqlite(s) => s.item_ids_for_library(wire_id).await,
            AnyStore::Postgres(p) => p.item_ids_for_library(wire_id).await,
        }
    }
}

// ---------------------------------------------------------------------
// PreferenceStore
// ---------------------------------------------------------------------
impl PreferenceStore for AnyStore {
    async fn get_user_configuration(&self, user: UserId) -> DomainResult<Option<String>> {
        match self {
            AnyStore::Sqlite(s) => s.get_user_configuration(user).await,
            AnyStore::Postgres(p) => p.get_user_configuration(user).await,
        }
    }

    async fn set_user_configuration(&self, user: UserId, json: &str) -> DomainResult<()> {
        match self {
            AnyStore::Sqlite(s) => s.set_user_configuration(user, json).await,
            AnyStore::Postgres(p) => p.set_user_configuration(user, json).await,
        }
    }

    async fn get_display_preferences(
        &self,
        user: UserId,
        dp_id: &str,
        client: &str,
    ) -> DomainResult<Option<String>> {
        match self {
            AnyStore::Sqlite(s) => s.get_display_preferences(user, dp_id, client).await,
            AnyStore::Postgres(p) => p.get_display_preferences(user, dp_id, client).await,
        }
    }

    async fn set_display_preferences(
        &self,
        user: UserId,
        dp_id: &str,
        client: &str,
        json: &str,
    ) -> DomainResult<()> {
        match self {
            AnyStore::Sqlite(s) => s.set_display_preferences(user, dp_id, client, json).await,
            AnyStore::Postgres(p) => p.set_display_preferences(user, dp_id, client, json).await,
        }
    }
}

// ---------------------------------------------------------------------
// TranscodeSessionStore
// ---------------------------------------------------------------------
impl TranscodeSessionStore for AnyStore {
    async fn upsert_transcode_session(
        &self,
        play_session_id: &str,
        session: &PersistedTranscodeSession,
        now_unix_secs: i64,
    ) -> DomainResult<()> {
        match self {
            AnyStore::Sqlite(s) => {
                s.upsert_transcode_session(play_session_id, session, now_unix_secs)
                    .await
            }
            AnyStore::Postgres(p) => {
                p.upsert_transcode_session(play_session_id, session, now_unix_secs)
                    .await
            }
        }
    }

    async fn get_transcode_session(
        &self,
        play_session_id: &str,
    ) -> DomainResult<Option<PersistedTranscodeSession>> {
        match self {
            AnyStore::Sqlite(s) => s.get_transcode_session(play_session_id).await,
            AnyStore::Postgres(p) => p.get_transcode_session(play_session_id).await,
        }
    }

    async fn remove_transcode_session(&self, play_session_id: &str) -> DomainResult<()> {
        match self {
            AnyStore::Sqlite(s) => s.remove_transcode_session(play_session_id).await,
            AnyStore::Postgres(p) => p.remove_transcode_session(play_session_id).await,
        }
    }

    async fn prune_transcode_sessions(&self, cutoff_unix_secs: i64) -> DomainResult<u64> {
        match self {
            AnyStore::Sqlite(s) => s.prune_transcode_sessions(cutoff_unix_secs).await,
            AnyStore::Postgres(p) => p.prune_transcode_sessions(cutoff_unix_secs).await,
        }
    }
}

// ---------------------------------------------------------------------
// SyncGroupStore
// ---------------------------------------------------------------------
impl SyncGroupStore for AnyStore {
    async fn upsert_sync_group(
        &self,
        group: &PersistedSyncGroup,
        now_unix_secs: i64,
    ) -> DomainResult<()> {
        match self {
            AnyStore::Sqlite(s) => s.upsert_sync_group(group, now_unix_secs).await,
            AnyStore::Postgres(p) => p.upsert_sync_group(group, now_unix_secs).await,
        }
    }

    async fn get_sync_group(&self, group_id: &str) -> DomainResult<Option<PersistedSyncGroup>> {
        match self {
            AnyStore::Sqlite(s) => s.get_sync_group(group_id).await,
            AnyStore::Postgres(p) => p.get_sync_group(group_id).await,
        }
    }

    async fn list_sync_groups(&self) -> DomainResult<Vec<PersistedSyncGroup>> {
        match self {
            AnyStore::Sqlite(s) => s.list_sync_groups().await,
            AnyStore::Postgres(p) => p.list_sync_groups().await,
        }
    }

    async fn remove_sync_group(&self, group_id: &str) -> DomainResult<()> {
        match self {
            AnyStore::Sqlite(s) => s.remove_sync_group(group_id).await,
            AnyStore::Postgres(p) => p.remove_sync_group(group_id).await,
        }
    }

    async fn prune_sync_groups(&self, cutoff_unix_secs: i64) -> DomainResult<u64> {
        match self {
            AnyStore::Sqlite(s) => s.prune_sync_groups(cutoff_unix_secs).await,
            AnyStore::Postgres(p) => p.prune_sync_groups(cutoff_unix_secs).await,
        }
    }
}

// ---------------------------------------------------------------------
// UserDataStore
// ---------------------------------------------------------------------
impl UserDataStore for AnyStore {
    async fn get_user_data(&self, user: UserId, item: MediaId) -> DomainResult<UserItemData> {
        match self {
            AnyStore::Sqlite(s) => s.get_user_data(user, item).await,
            AnyStore::Postgres(p) => p.get_user_data(user, item).await,
        }
    }

    async fn set_user_data(
        &self,
        user: UserId,
        item: MediaId,
        data: UserItemData,
    ) -> DomainResult<()> {
        match self {
            AnyStore::Sqlite(s) => s.set_user_data(user, item, data).await,
            AnyStore::Postgres(p) => p.set_user_data(user, item, data).await,
        }
    }

    async fn user_data_bulk(
        &self,
        user: UserId,
        items: &[MediaId],
    ) -> DomainResult<Vec<UserItemData>> {
        match self {
            AnyStore::Sqlite(s) => s.user_data_bulk(user, items).await,
            AnyStore::Postgres(p) => p.user_data_bulk(user, items).await,
        }
    }

    async fn resumable_items(&self, user: UserId) -> DomainResult<Vec<MediaId>> {
        match self {
            AnyStore::Sqlite(s) => s.resumable_items(user).await,
            AnyStore::Postgres(p) => p.resumable_items(user).await,
        }
    }
}

// ---------------------------------------------------------------------
// UserStore
// ---------------------------------------------------------------------
impl UserStore for AnyStore {
    async fn create(&self, record: UserRecord) -> AuthResult<()> {
        match self {
            AnyStore::Sqlite(s) => s.create(record).await,
            AnyStore::Postgres(p) => p.create(record).await,
        }
    }

    async fn lookup_by_name(&self, name: &str) -> AuthResult<UserRecord> {
        match self {
            AnyStore::Sqlite(s) => s.lookup_by_name(name).await,
            AnyStore::Postgres(p) => p.lookup_by_name(name).await,
        }
    }

    async fn get(&self, id: UserId) -> AuthResult<UserRecord> {
        match self {
            AnyStore::Sqlite(s) => UserStore::get(s, id).await,
            AnyStore::Postgres(p) => UserStore::get(p, id).await,
        }
    }

    async fn list(&self) -> AuthResult<Vec<UserRecord>> {
        match self {
            AnyStore::Sqlite(s) => UserStore::list(s).await,
            AnyStore::Postgres(p) => UserStore::list(p).await,
        }
    }

    async fn delete(&self, id: UserId) -> AuthResult<()> {
        match self {
            AnyStore::Sqlite(s) => s.delete(id).await,
            AnyStore::Postgres(p) => p.delete(id).await,
        }
    }

    async fn set_policy(&self, id: UserId, policy: UserPolicy) -> AuthResult<()> {
        match self {
            AnyStore::Sqlite(s) => s.set_policy(id, policy).await,
            AnyStore::Postgres(p) => p.set_policy(id, policy).await,
        }
    }

    async fn set_password(&self, id: UserId, password_hash: SecretString) -> AuthResult<()> {
        match self {
            AnyStore::Sqlite(s) => s.set_password(id, password_hash).await,
            AnyStore::Postgres(p) => p.set_password(id, password_hash).await,
        }
    }
}

// ---------------------------------------------------------------------
// TokenStore
// ---------------------------------------------------------------------
impl TokenStore for AnyStore {
    async fn issue(&self, user_id: UserId, device_id: &str) -> AuthResult<AuthToken> {
        match self {
            AnyStore::Sqlite(s) => s.issue(user_id, device_id).await,
            AnyStore::Postgres(p) => p.issue(user_id, device_id).await,
        }
    }

    async fn resolve(&self, token: &str) -> AuthResult<UserId> {
        match self {
            AnyStore::Sqlite(s) => s.resolve(token).await,
            AnyStore::Postgres(p) => p.resolve(token).await,
        }
    }

    async fn revoke(&self, token: &str) -> AuthResult<()> {
        match self {
            AnyStore::Sqlite(s) => s.revoke(token).await,
            AnyStore::Postgres(p) => p.revoke(token).await,
        }
    }

    async fn tokens_for(&self, user: UserId) -> AuthResult<Vec<TokenRecord>> {
        match self {
            AnyStore::Sqlite(s) => s.tokens_for(user).await,
            AnyStore::Postgres(p) => p.tokens_for(user).await,
        }
    }

    async fn revoke_tokens_by_device(&self, user: UserId, device_id: &str) -> AuthResult<u64> {
        match self {
            AnyStore::Sqlite(s) => s.revoke_tokens_by_device(user, device_id).await,
            AnyStore::Postgres(p) => p.revoke_tokens_by_device(user, device_id).await,
        }
    }
}
