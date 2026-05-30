//! pharos-core: domain traits at IO boundary (V12).
//! No IO impls here. Servers/adapters live in pharos-server and friends.

pub mod auth;
pub mod secret;

pub use auth::{
    AuthBackend, AuthError, AuthResult, AuthToken, TokenRecord, TokenStore, User, UserId,
    UserPolicy, UserRecord, UserStore,
};
pub use secret::SecretString;

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub type MediaId = u64;

/// LIB-A6 — content fingerprint: a raw 8-byte digest of a file's bytes
/// (size + probed duration + head/tail content), computed by the scanner
/// (the hash itself is IO and lives in `pharos-scanner`, never here — V12).
/// Unlike the path-derived [`stable_id`](MediaItem::id), a fingerprint
/// survives a rename/move because it depends only on content, so the
/// scanner can recognise a moved file as the same item instead of
/// import-then-sweep churn. Persisted raw (no encoding) as a BLOB/BYTEA.
pub type Fingerprint = [u8; 8];

// Note: no `Eq` — `MediaMetadata` carries `f32` ratings (which are not
// `Eq`). `MediaItem` is only ever a HashMap *value* (keyed by MediaId),
// never a key, so `PartialEq` suffices for the round-trip assertions.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MediaItem {
    pub id: MediaId,
    pub path: PathBuf,
    pub title: String,
    pub kind: MediaKind,
    /// Probed file/stream metadata persisted alongside the item.
    /// All fields optional — a probe failure or pre-ffprobe scan still
    /// yields a row, just with `MediaProbe::default()`. Jellyfin DTOs
    /// omit fields whose value is `None` so clients negotiate against
    /// reality, not a stub.
    pub probe: MediaProbe,
    /// Show-hierarchy metadata when kind == Episode. None for
    /// Movie / Audio. Synthesised Series + Season DTOs derive their
    /// stable ids from `series_name` + `(series_name, season_number)`
    /// respectively (via `series_id_for` / `season_id_for`).
    pub series: Option<SeriesInfo>,
    /// Unix-seconds timestamp of the first time pharos saw this
    /// item. Set on initial INSERT; preserved by `ON CONFLICT` so
    /// rescans don't reset "added on" dates. `None` for rows
    /// imported before migration 0010.
    pub created_at: Option<i64>,
    /// LIB-C7/C8/C9 — descriptive (non-technical) metadata: overview,
    /// tagline, ratings, production year, premiere date, external
    /// provider ids. Distinct from [`MediaProbe`], which stays
    /// TECHNICAL-only (codecs/HDR/streams). EPIC D populates these from
    /// NFO / online providers; here we PLUMB them so they round-trip
    /// through the store and project down into the Jellyfin
    /// `BaseItemDto`. `Default` = all `None` / empty.
    pub metadata: MediaMetadata,
}

/// LIB-C7/C8/C9 — item-level descriptive metadata persisted alongside
/// the [`MediaProbe`]. All fields optional: a freshly-scanned file that
/// hasn't been enriched yet still yields a row, just with
/// `MediaMetadata::default()`. The Jellyfin DTO omits fields whose value
/// is `None` (and emits an empty `Taglines` array when `tagline` is
/// `None`) to preserve wire compatibility.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MediaMetadata {
    /// Jellyfin `CommunityRating` (0–10 audience score).
    pub community_rating: Option<f32>,
    /// Jellyfin `CriticRating` (0–100 critic score).
    pub critic_rating: Option<f32>,
    /// Parental rating string, e.g. `"PG-13"` → Jellyfin
    /// `OfficialRating`.
    pub official_rating: Option<String>,
    /// Release / production year → Jellyfin `ProductionYear`.
    pub production_year: Option<u32>,
    /// Original premiere/air date as unix-seconds (mirrors
    /// `created_at`'s encoding). The DTO converts to Jellyfin's ISO-8601
    /// `PremiereDate`.
    pub premiere_date: Option<i64>,
    /// Long-form synopsis → Jellyfin `Overview`.
    pub overview: Option<String>,
    /// Short tagline → Jellyfin `Taglines` (an array carrying the single
    /// value, or empty when `None`).
    pub tagline: Option<String>,
    /// External provider ids → Jellyfin `ProviderIds` map.
    pub provider_ids: ProviderIds,
}

/// LIB-C9 — external metadata-provider identifiers. Persisted as a JSON
/// object string in the `provider_ids` column; projected into the
/// Jellyfin `BaseItemDto.ProviderIds` map under the canonical provider
/// keys (`Tmdb` / `Tvdb` / `Imdb` / `MusicBrainzTrack`). All optional —
/// `Default` = no known ids.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderIds {
    /// TheMovieDB id (→ `Tmdb`).
    pub tmdb: Option<String>,
    /// TheTVDB id (→ `Tvdb`).
    pub tvdb: Option<String>,
    /// IMDb id, e.g. `tt0111161` (→ `Imdb`).
    pub imdb: Option<String>,
    /// MusicBrainz track id (→ `MusicBrainzTrack`).
    pub mbid: Option<String>,
}

impl ProviderIds {
    /// True when no provider id is set (so the DTO can emit an empty
    /// `ProviderIds` map rather than fabricating keys).
    pub fn is_empty(&self) -> bool {
        self.tmdb.is_none() && self.tvdb.is_none() && self.imdb.is_none() && self.mbid.is_none()
    }
}

/// LIB-D1 — artwork image role. Mirrors `pharos_cache::ImageRole` but
/// lives in core (V12: core must not depend on `pharos-cache`). The cache
/// crate maps its `ImageRole` from this enum, the same pattern by which
/// [`MediaKind`] is shared across crates. The Jellyfin image API serves
/// each role under its canonical token (`Primary` / `Backdrop` / ...).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ArtworkRole {
    /// Poster / cover. `poster.jpg` / `folder.jpg` / `cover.jpg`.
    #[default]
    Primary,
    /// Background art. `fanart.jpg` / `backdrop.jpg`.
    Backdrop,
    /// Wide thumbnail. `<name>-thumb.jpg`.
    Thumb,
    /// Transparent logo. `logo.png` / `clearlogo.png`.
    Logo,
    /// Banner strip. `banner.jpg`.
    Banner,
    /// Disc / CD art. `disc.png`.
    Disc,
    /// Clear-art. `clearart.png`.
    Art,
}

impl ArtworkRole {
    /// Canonical Jellyfin `ImageType` token for this role.
    pub fn as_str(self) -> &'static str {
        match self {
            ArtworkRole::Primary => "Primary",
            ArtworkRole::Backdrop => "Backdrop",
            ArtworkRole::Thumb => "Thumb",
            ArtworkRole::Logo => "Logo",
            ArtworkRole::Banner => "Banner",
            ArtworkRole::Disc => "Disc",
            ArtworkRole::Art => "Art",
        }
    }
}

/// LIB-D1 — where the bytes for an [`ArtworkRef`] come from. A local-first
/// scan yields [`ArtworkSource::LocalFile`] (a sibling sidecar discovered
/// on disk); a future online provider yields [`ArtworkSource::Url`] (to be
/// fetched + cached lazily). The D5 image-serving branch reads
/// `LocalFile` paths directly; `Url` is carried now and persisted later.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArtworkSource {
    /// A sidecar image file already on the user's disk.
    LocalFile(PathBuf),
    /// A remote image URL (online providers, fetched + cached lazily).
    Url(String),
}

/// LIB-D1 — one discovered/resolved artwork image for an item: its [role]
/// plus where the bytes live ([source]). Produced by a
/// [`MetadataProvider`] and merged (union + dedupe) by the resolver; D4
/// persists `LocalFile` refs into the `artwork` table keyed by item id +
/// role.
///
/// [role]: ArtworkRef::role
/// [source]: ArtworkRef::source
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtworkRef {
    pub role: ArtworkRole,
    pub source: ArtworkSource,
}

/// LIB-D1 — the kind of credit a [`PersonRef`] carries. `Other` is the
/// fallback for NFO `<type>` strings outside the common vocabulary so a
/// malformed/unknown role never drops the person.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PersonKind {
    #[default]
    Actor,
    Director,
    Writer,
    Producer,
    Composer,
    GuestStar,
    Other,
}

impl PersonKind {
    /// Canonical Jellyfin `PersonType` token.
    pub fn as_str(self) -> &'static str {
        match self {
            PersonKind::Actor => "Actor",
            PersonKind::Director => "Director",
            PersonKind::Writer => "Writer",
            PersonKind::Producer => "Producer",
            PersonKind::Composer => "Composer",
            PersonKind::GuestStar => "GuestStar",
            PersonKind::Other => "Other",
        }
    }
}

/// LIB-D1 — one person credit (cast / crew) carried by a
/// [`MetadataResult`]. People have no store table yet (a later slice adds
/// `people` + `item_people`); D7 carries these through the merge and logs
/// them as not-yet-persisted. `role` is the free-form NFO `<role>` string
/// (e.g. department) distinct from the structured [`kind`]; `character` is
/// the played character for cast; `sort_order` preserves NFO ordering.
///
/// [kind]: PersonRef::kind
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PersonRef {
    pub name: String,
    pub role: Option<String>,
    pub kind: PersonKind,
    pub character: Option<String>,
    pub sort_order: Option<u32>,
}

/// LIB-D1 — inputs a [`MetadataProvider`] resolves metadata from. Borrows
/// everything (no owned allocation per request): the media `path` (for
/// sidecar / NFO lookup), its [`MediaKind`] (so a provider can early-out
/// via [`supports`](MetadataProvider::supports)), the already-computed
/// [`MediaProbe`] (embedded tags a provider may fold in), and the
/// `series` hierarchy when the item is an episode (for show-level NFO /
/// season-level art). Lifetime `'a` ties the borrows to the scan closure.
#[derive(Debug, Clone, Copy)]
pub struct MetadataRequest<'a> {
    pub path: &'a std::path::Path,
    pub kind: MediaKind,
    pub probe: &'a MediaProbe,
    pub series: Option<&'a SeriesInfo>,
}

/// LIB-D1 — the merge-friendly result of one [`MetadataProvider::fetch`].
/// Every scalar is `Option` so the [`MetadataResolver`] can priority-merge
/// ("first `Some` by provider priority wins"); the `Vec` fields union +
/// dedupe across providers. `Default` = wholly empty (a provider that
/// found nothing returns this rather than erroring — V6 spirit).
///
/// Only a subset has a persistence home today: `overview` / `tagline` /
/// ratings / years / `provider_ids` land on [`MediaMetadata`], `genres`
/// on the genre join, and `artwork` (LocalFile refs) on the D4 artwork
/// table. `studios` / `people` / `tags` / `collections` are CARRIED now
/// even though their tables don't exist yet — D7 logs them as
/// not-yet-persisted and a later slice adds the tables.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MetadataResult {
    /// Canonical title override (NFO `<title>`); `None` keeps the
    /// scanner's filename-derived title.
    pub title: Option<String>,
    pub overview: Option<String>,
    pub tagline: Option<String>,
    pub production_year: Option<u32>,
    pub premiere_date: Option<i64>,
    pub community_rating: Option<f32>,
    pub critic_rating: Option<f32>,
    pub official_rating: Option<String>,
    pub genres: Vec<String>,
    pub studios: Vec<String>,
    pub people: Vec<PersonRef>,
    pub tags: Vec<String>,
    pub collections: Vec<String>,
    pub provider_ids: ProviderIds,
    pub artwork: Vec<ArtworkRef>,
}

/// LIB-D1 — a source of descriptive metadata for a scanned item (local
/// NFO, sidecar art, filename convention, or a future online provider).
/// Declared here (V12: IO-free) so the resolver and store live in core;
/// the IO-bearing impls (NFO XML read, sidecar `stat`) live in
/// `pharos-scanner` exactly as [`Prober`] is declared here but
/// `FfmpegProber` is implemented there.
///
/// Providers are ordered by [`priority`](Self::priority) (highest first)
/// when merged: a local NFO edit (high priority) wins a scalar field over
/// an online provider (lower priority). `fetch` returns owned
/// [`MetadataResult`] data — no IO type leaks into core. A provider that
/// finds nothing returns an empty result; one that hits an IO/parse error
/// returns `Err`, which the resolver logs + skips (V6) rather than
/// aborting the whole merge.
pub trait MetadataProvider: Send + Sync {
    /// Stable identifier for logs/metrics (e.g. `"nfo"`, `"sidecar"`).
    fn name(&self) -> &'static str;

    /// Merge priority — higher wins a scalar field. Local sources
    /// (NFO/sidecar) sit above online providers so user-curated local
    /// edits take precedence.
    fn priority(&self) -> i32;

    /// Whether this provider can resolve metadata for `kind`. The
    /// resolver skips providers that don't support the item's kind
    /// before calling [`fetch`](Self::fetch).
    fn supports(&self, kind: MediaKind) -> bool;

    /// Resolve metadata for `req`. Owned-data return keeps core IO-free.
    /// On a missing source return `Ok(MetadataResult::default())`; on an
    /// IO/parse error return `Err` (the resolver logs + skips it).
    fn fetch(
        &self,
        req: &MetadataRequest<'_>,
    ) -> impl std::future::Future<Output = DomainResult<MetadataResult>> + Send;
}

/// LIB-C4 — stable 32-hex wire id for an aggregate entity (genre /
/// artist / album / studio), keyed on a `kind` namespace + `name`. Pure
/// arithmetic over the UTF-8 bytes — not IO, so it lives in core (V12
/// only forbids IO impls, not deterministic hashing). The Jellyfin DTO's
/// `genre_id_for` / `artist_id_for` / … delegate here so the wire id a
/// `genres.wire_id` column stores at upsert is byte-identical to the id
/// clients send back as `?ParentId=`.
///
/// Layout: `xxh3_64("{kind}:{name}") & 0x7FFF…` rendered as the 16-hex
/// digest repeated twice (32 chars) — a GUID-shaped string jellyfin-web
/// accepts as an item id.
pub fn name_aggregate_wire_id(kind: &str, name: &str) -> String {
    use xxhash_rust::xxh3::xxh3_64;
    let h = xxh3_64(format!("{kind}:{name}").as_bytes()) & 0x7FFF_FFFF_FFFF_FFFF;
    format!("{h:016x}{h:016x}")
}

/// LIB-C4 — the `genres.wire_id` value for a genre `name`. Thin wrapper
/// over [`name_aggregate_wire_id`] with the `"genre"` namespace; the
/// store stamps this at upsert so `/Items?ParentId=<genre id>` resolves
/// by an indexed `wire_id` lookup instead of an in-memory DISTINCT scan.
pub fn genre_wire_id(name: &str) -> String {
    name_aggregate_wire_id("genre", name)
}

/// LIB-C4 — a genre entity row. `wire_id` is the stable
/// [`genre_wire_id`] the Jellyfin DTO emits as the Genre's `Id`; the
/// integer `id` is the internal PK used by the `item_genres` join.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Genre {
    pub id: i64,
    pub name: String,
    pub wire_id: String,
}

/// LIB-C4 — one genre plus how many items carry it, for the `/Genres`
/// list (jellyfin-web shows the tile; the count drives library stats).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GenreCount {
    pub genre: Genre,
    pub item_count: u32,
}

/// LIB-C4 — genres as first-class entities. Split from [`MediaStore`] so
/// the in-memory test stores that only need item round-tripping don't
/// have to implement the join, while the scanner (which links items to
/// genres on write) and the API (which lists genres + resolves the
/// ParentId pivot) require both bounds.
///
/// The wire id a genre row stores is [`genre_wire_id`] of its name,
/// computed at [`upsert_genre`](Self::upsert_genre) time — so the
/// `/Items?ParentId=<genre id>` pivot is an indexed `wire_id` lookup
/// (see [`item_ids_for_genre`](Self::item_ids_for_genre)) rather than the
/// legacy in-memory DISTINCT scan over every item's `genre` string.
pub trait GenreStore: Send + Sync {
    /// Upsert a genre by `name`, returning its internal PK. Idempotent:
    /// re-upserting an existing name returns the same id without
    /// duplicating the row. Empty/whitespace names are rejected by the
    /// caller (the scanner trims + drops blanks before linking).
    fn upsert_genre(
        &self,
        name: &str,
    ) -> impl std::future::Future<Output = DomainResult<i64>> + Send;

    /// Replace `item`'s genre links with exactly `names` (trimmed,
    /// de-duplicated, blanks dropped by the impl). Upserts any missing
    /// genre rows first. Idempotent — a rescan that yields the same
    /// genres leaves the join unchanged.
    fn link_item_genres(
        &self,
        item: MediaId,
        names: &[String],
    ) -> impl std::future::Future<Output = DomainResult<()>> + Send;

    /// Every genre with its item count, ordered by name, for `/Genres`.
    fn genres_with_counts(
        &self,
    ) -> impl std::future::Future<Output = DomainResult<Vec<GenreCount>>> + Send;

    /// Item ids tagged with the genre whose `wire_id` matches — the exact
    /// `/Items?ParentId=<genre id>` pivot. Empty Vec when no genre row
    /// carries that wire id (so the caller renders an empty library).
    fn item_ids_for_genre(
        &self,
        wire_id: &str,
    ) -> impl std::future::Future<Output = DomainResult<Vec<MediaId>>> + Send;

    /// One-time backfill: read every `media_items.genre` string, split on
    /// comma/pipe, and populate `genres` + `item_genres` for rows scanned
    /// before C4. Idempotent (upsert + INSERT-OR-IGNORE join), so it is
    /// safe to run repeatedly. Returns the number of `item_genres` links
    /// present after the pass.
    fn backfill_genres(&self) -> impl std::future::Future<Output = DomainResult<u64>> + Send;
}

/// LIB-C4 — split a raw `media_items.genre` string into individual genre
/// names. Jellyfin's wire convention separates genres with `|`; NFO /
/// ffprobe tags often use `,`. We split on either, trim, and drop blanks.
/// Shared by the scanner (link on write) and the backfill so both derive
/// the same genre set from one source column.
pub fn split_genre_field(raw: &str) -> Vec<String> {
    raw.split(['|', ','])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// LIB-C1 — the typed kind of a top-level library, driving the Jellyfin
/// `CollectionType` a `/Library/VirtualFolders` / `/Library/MediaFolders`
/// entry advertises. `Mixed` is the back-compat default for a plain
/// `[media].roots` entry that didn't declare a kind (matches the legacy
/// single "All Media / mixed" stub).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum LibraryKind {
    Movies,
    TvShows,
    Music,
    #[default]
    Mixed,
}

impl LibraryKind {
    /// The Jellyfin `CollectionType` wire token. `Mixed` serialises as
    /// `"mixed"` — the same value the legacy stub emitted, so existing
    /// clients keep resolving the view.
    pub fn collection_type(self) -> &'static str {
        match self {
            LibraryKind::Movies => "movies",
            LibraryKind::TvShows => "tvshows",
            LibraryKind::Music => "music",
            LibraryKind::Mixed => "mixed",
        }
    }

    /// Parse a config / wire token (case-insensitive) into a kind.
    /// Unknown / empty tokens fall back to [`LibraryKind::Mixed`] so a
    /// typo never crashes startup — the operator just gets a mixed view.
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "movies" | "movie" => LibraryKind::Movies,
            "tvshows" | "tvshow" | "tv" | "shows" | "series" => LibraryKind::TvShows,
            "music" | "audio" => LibraryKind::Music,
            _ => LibraryKind::Mixed,
        }
    }
}

/// LIB-C1 — a typed top-level library: one per configured media root.
/// `wire_id` is the stable 32-hex `library_id_for_root(root_path)` the
/// Jellyfin views/virtual-folder DTOs already emit as a library `Id`, so
/// existing client URLs survive promoting the single "All Media" stub to
/// real per-root typed libraries. The integer `id` is the internal PK;
/// `media_items.library_id` references it after the path-prefix backfill.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Library {
    pub id: i64,
    pub name: String,
    pub root_path: String,
    pub kind: LibraryKind,
    pub wire_id: String,
}

/// LIB-C1 — typed libraries as first-class rows. Split from
/// [`MediaStore`] so in-memory test stores that only round-trip items
/// don't have to implement library reconciliation. Reconciled from
/// `[media]` config at boot (one row per root), then
/// [`backfill_library_ids`](Self::backfill_library_ids) stamps each
/// existing `media_items.library_id` by path-prefix.
pub trait LibraryStore: Send + Sync {
    /// Upsert a library by its unique `root_path`, returning its internal
    /// PK. Idempotent: re-upserting the same root updates the name/kind
    /// (config may have changed) and returns the existing id without
    /// duplicating the row. `wire_id` is supplied by the caller (computed
    /// from the root via the DTO's `library_id_for_root` so the hash
    /// lives at the API boundary, not in the store).
    fn upsert_library(
        &self,
        name: &str,
        root_path: &str,
        kind: LibraryKind,
        wire_id: &str,
    ) -> impl std::future::Future<Output = DomainResult<i64>> + Send;

    /// Every configured library, ordered by name, for
    /// `/Library/VirtualFolders` + `/Library/MediaFolders` + the view list.
    fn libraries(&self) -> impl std::future::Future<Output = DomainResult<Vec<Library>>> + Send;

    /// Path-boundary-safe backfill: assign `media_items.library_id` for
    /// every item whose path is strictly under the library's `root_path`
    /// (so `/media/movies` never claims `/media/movies-4k`). Idempotent —
    /// re-running re-points each item at the library covering its path.
    /// Returns the number of items assigned to some library.
    fn backfill_library_ids(&self) -> impl std::future::Future<Output = DomainResult<u64>> + Send;

    /// Item ids belonging to the library whose `wire_id` matches — the
    /// exact `/Items?ParentId=<library id>` pivot. Empty Vec when no
    /// library row carries that wire id.
    fn item_ids_for_library(
        &self,
        wire_id: &str,
    ) -> impl std::future::Future<Output = DomainResult<Vec<MediaId>>> + Send;
}

/// Parent-show / season / episode metadata for items the scanner
/// promoted to `MediaKind::Episode`. `season_number` + `episode_number`
/// fall back to None when the path didn't yield them but the
/// containing dir still flagged as a season layout.
///
/// LIB-C11 — series identity is keyed on the **show folder path**
/// (`series_folder`), not the bare `series_name`, so two distinct shows
/// that happen to share a name (`Cosmos (1980)` vs `Cosmos (2014)`) get
/// distinct synthesised Series/Season wire ids and don't interleave
/// their episodes. `series_folder` is the canonical filesystem path of
/// the directory that holds the season dirs / episode files (captured by
/// the scanner). `series_year` is parsed from a `Show Name (YYYY)` folder
/// convention so clients can disambiguate the two shows visually. Both
/// are `Option` and additive: rows scanned before C11 (or items whose
/// path didn't yield a folder) decode with `None` and fall back to the
/// legacy name-keyed identity.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SeriesInfo {
    pub series_name: String,
    pub season_number: Option<u32>,
    pub episode_number: Option<u32>,
    /// LIB-C11 — canonical filesystem path of the show's root folder
    /// (the closest non-"Season NN" ancestor of the episode). `None` for
    /// legacy rows; the wire-id helpers fall back to `series_name` then.
    pub series_folder: Option<String>,
    /// LIB-C11 — release year parsed from a `Show Name (YYYY)` folder
    /// name, surfaced as `ProductionYear` so same-name shows are
    /// distinguishable in clients. `None` when the folder carries no year.
    pub series_year: Option<u32>,
}

impl SeriesInfo {
    /// LIB-C11 — the identity key the synthesised Series/Season wire ids
    /// hash on. Prefers the stable, per-show-on-disk `series_folder`;
    /// falls back to `series_name` for legacy rows lacking a folder so
    /// pre-backfill client URLs keep resolving.
    pub fn series_key(&self) -> &str {
        self.series_folder.as_deref().unwrap_or(&self.series_name)
    }
}

/// Stream/format metadata pulled by `Prober::probe` (today: ffprobe).
/// Persisted on `MediaItem` so the API surface (PlaybackInfo, BaseItemDto)
/// reports real codec / container / size / runtime per file.
///
/// `frame_rate_mille` stores frames-per-second × 1000 to keep MediaProbe
/// `Eq` without leaking floats into the domain layer. Conversion helpers
/// (`frame_rate_f32`) live in the DTO boundary.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaProbe {
    pub size_bytes: Option<u64>,
    pub duration_ms: Option<u64>,
    pub container: Option<String>,
    pub bitrate_bps: Option<u64>,
    pub video_codec: Option<String>,
    /// Canonical H.264/HEVC/VP9 profile name as ffprobe reports
    /// (`"High"`, `"Main"`, `"Main 10"`, `"Profile 0"`). Used to
    /// build RFC 6381 CODECS strings for HLS playlists.
    pub video_profile: Option<String>,
    /// Codec level × 10 (e.g. 40 = level 4.0, 51 = level 5.1). Wire
    /// format for the trailing two hex digits of `avc1.…` /
    /// `hvc1.…L<level>` codec tokens.
    pub video_level: Option<u32>,
    /// P13 — ffprobe `pix_fmt` token (e.g. `"yuv420p"`,
    /// `"yuv420p10le"`). Distinguishes 8-bit vs 10-bit pipelines so
    /// HDR-capable clients pick the right decoder path.
    pub pixel_format: Option<String>,
    /// ffprobe `color_primaries` (`"bt709"`, `"bt2020"`).
    pub color_primaries: Option<String>,
    /// ffprobe `color_transfer` (`"bt709"`, `"smpte2084"` = HDR10,
    /// `"arib-std-b67"` = HLG). Primary HDR discriminator.
    pub color_transfer: Option<String>,
    /// ffprobe `color_space` (`"bt709"`, `"bt2020nc"`).
    pub color_space: Option<String>,
    pub audio_codec: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub frame_rate_mille: Option<u32>,
    pub audio_channels: Option<u32>,
    pub sample_rate: Option<u32>,
    /// Embedded subtitle tracks discovered by the prober. Stored
    /// JSON-serialised in the `subtitle_tracks` column.
    pub subtitle_tracks: Vec<SubtitleTrack>,
    /// P16 — every audio stream the source carries. The scalar
    /// `audio_codec` / `audio_channels` / `sample_rate` above stay
    /// populated from the first stream for back-compat with rows that
    /// pre-date the multi-track migration. Empty Vec = no audio
    /// streams in source.
    pub audio_tracks: Vec<AudioTrack>,
    /// Common audio-file format tags (`title` / `artist` / `album` /
    /// `album_artist` / `genre`). Populated by FfmpegProber from
    /// ffprobe's `format.tags`. None when the file lacks the tag.
    pub artist: Option<String>,
    pub album: Option<String>,
    pub album_artist: Option<String>,
    pub genre: Option<String>,
    /// Embedded chapter markers extracted by ffprobe `-show_chapters`.
    /// Each entry's `start_ms` lands on Jellyfin's `Chapters[].StartPositionTicks`.
    pub chapters: Vec<MediaChapter>,
    /// P34 — alternate playable versions of the same logical item
    /// (theatrical / director's cut / extended / alternate dubs).
    /// PlaybackInfo emits one MediaSource per entry in addition to
    /// the primary version this struct describes. Empty Vec leaves
    /// PlaybackInfo single-source. A future scanner enrichment pass
    /// populates this from sibling-file convention or NFO metadata.
    pub alternate_sources: Vec<AlternateMediaSource>,
}

/// P34 — minimal MediaSource shape carried alongside the primary
/// probe so PlaybackInfo can advertise multiple editions of the same
/// item. Path is stored so the segment + direct-play handlers know
/// which file to mux. Fields not listed here fall back to the primary
/// probe at PlaybackInfo build time (saves duplicating the entire
/// codec stack for every edition).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AlternateMediaSource {
    /// Stable id suffix appended to the parent item id when forming
    /// the wire `MediaSourceInfo.Id`. Real Jellyfin uses a free-form
    /// string here so existing client URLs survive a re-scan.
    pub id: String,
    /// Filesystem path to the alternate-edition source file. Same
    /// shape as `MediaItem.path`; the request-path handlers honour
    /// it instead of the primary path when the wire MediaSourceId
    /// selects this entry.
    pub path: std::path::PathBuf,
    pub container: Option<String>,
    pub video_codec: Option<String>,
    pub audio_codec: Option<String>,
    pub bitrate_bps: Option<u64>,
    pub size_bytes: Option<u64>,
    pub duration_ms: Option<u64>,
    /// Human-readable edition tag (`"Director's Cut"`, `"Extended"`,
    /// `"Theatrical"`). Surfaces as `MediaSourceInfo.Name` so the
    /// jellyfin-web edition picker labels rows correctly.
    pub name: Option<String>,
}

/// One chapter marker. `title` defaults to `Chapter {N}` when ffprobe
/// reports no name (most BluRay rips).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaChapter {
    pub start_ms: u64,
    pub end_ms: u64,
    pub title: String,
}

/// P16 — one embedded audio stream from the source file. Multi-track
/// containers (TV episodes with eng + jpn dubs, movies with director
/// commentary) emit one entry per stream so the PlaybackInfo wire
/// shape surfaces a track picker.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioTrack {
    pub stream_index: u32,
    pub codec: Option<String>,
    pub channels: Option<u32>,
    pub sample_rate: Option<u32>,
    pub language: Option<String>,
    pub title: Option<String>,
    pub is_default: bool,
    /// P37 — track-level ReplayGain in centidecibels (× 100). ffprobe
    /// reports `tags.replaygain_track_gain` as `"-7.34 dB"`; the
    /// scanner parses the leading float and rounds to centidecibels.
    /// `Option<i16>` keeps the Eq derive (Option<f32> would break it)
    /// and the range easily fits all realistic gain values.
    pub replaygain_track_centidb: Option<i16>,
    /// P37 — album-level ReplayGain, same encoding as the track field.
    pub replaygain_album_centidb: Option<i16>,
}

/// One embedded subtitle stream from the source file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubtitleTrack {
    /// ffprobe stream index — what we pass `ffmpeg -map 0:s:<n>`.
    pub stream_index: u32,
    /// ISO-639 language tag when ffprobe emitted one.
    pub language: Option<String>,
    /// Codec name (`subrip`, `webvtt`, `ass`, ...) used to pick the
    /// right extraction pipeline.
    pub codec: Option<String>,
    /// Optional human-readable title.
    pub title: Option<String>,
    /// `true` when the stream's `disposition.default` flag is set.
    pub is_default: bool,
    /// `true` when the stream's `disposition.forced` flag is set.
    pub is_forced: bool,
    /// P35 — `true` when ffprobe reports `disposition.hearing_impaired`
    /// (the SDH / CC flag). Surfaces in MediaStream as
    /// `IsHearingImpaired` so jellyfin-web's subtitle picker can
    /// label the track and accessibility filtering works.
    pub is_hearing_impaired: bool,
}

impl MediaProbe {
    /// Convenience accessor — fps as f32, rounded back from the
    /// `× 1000` integer storage. Returns `None` if absent.
    pub fn frame_rate_f32(&self) -> Option<f32> {
        self.frame_rate_mille.map(|m| m as f32 / 1000.0)
    }

    /// Convert duration_ms → Jellyfin's 100-ns ticks (10_000 ticks / ms).
    pub fn run_time_ticks(&self) -> Option<u64> {
        self.duration_ms.map(|ms| ms.saturating_mul(10_000))
    }

    /// P13 — derive the Jellyfin `VideoRange` discriminator (`"HDR"`
    /// vs `"SDR"`) from probe color metadata. HDR10 uses
    /// `smpte2084`; HLG broadcast uses `arib-std-b67`; Dolby Vision
    /// ffprobe also reports `smpte2084` for the base layer.
    pub fn video_range(&self) -> &'static str {
        match self.color_transfer.as_deref() {
            Some("smpte2084") | Some("arib-std-b67") => "HDR",
            _ => "SDR",
        }
    }

    /// True when the probe carries HDR transfer characteristics.
    pub fn is_hdr(&self) -> bool {
        matches!(self.video_range(), "HDR")
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MediaKind {
    Movie,
    Episode,
    #[default]
    Audio,
}

impl MediaKind {
    pub fn as_str(self) -> &'static str {
        match self {
            MediaKind::Movie => "movie",
            MediaKind::Episode => "episode",
            MediaKind::Audio => "audio",
        }
    }
}

impl std::str::FromStr for MediaKind {
    type Err = DomainError;
    fn from_str(s: &str) -> DomainResult<Self> {
        match s {
            "movie" => Ok(MediaKind::Movie),
            "episode" => Ok(MediaKind::Episode),
            "audio" => Ok(MediaKind::Audio),
            other => Err(DomainError::Backend(format!("unknown media kind: {other}"))),
        }
    }
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

/// LIB-A1 — per-row scan-state signature used for incremental rescans.
/// `file_mtime` / `file_size` are the filesystem stat values seen on the
/// last scan (distinct from `MediaProbe::size_bytes`, which is the
/// ffprobe-reported format size). The A2 skip-unchanged path compares a
/// fresh stat against this signature to decide whether re-probing is
/// needed; `last_seen_scan_id` ties the row to the most recent scan run
/// that observed it (the A3 mark-and-sweep token).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScanState {
    /// Unix-seconds timestamp of the scan that last touched this row.
    pub last_scanned: i64,
    /// Filesystem mtime (unix seconds) recorded at last scan.
    pub file_mtime: i64,
    /// Filesystem size in bytes recorded at last scan.
    pub file_size: u64,
    /// Id of the most recent `scan_runs` entry that saw this row.
    pub last_seen_scan_id: i64,
}

/// LIB-A4 — structured result of an incremental scan. Replaces the bare
/// `usize` probed-count `scan_into` used to return so callers can broadcast
/// content deltas to connected clients and print richer CLI summaries.
///
/// `added` / `updated` / `removed` carry the affected [`MediaId`]s; `skipped`
/// is the count of unchanged files whose probe was elided (no ids retained —
/// they're noise for a delta broadcast). Invariants:
/// - `added`   — files inserted for the first time this run.
/// - `updated` — existing rows re-probed because their fs signature changed.
/// - `removed` — rows swept because the backing file vanished from disk.
/// - `skipped` — unchanged files (`mark_seen` only, probe skipped).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScanOutcome {
    pub added: Vec<MediaId>,
    pub updated: Vec<MediaId>,
    pub removed: Vec<MediaId>,
    pub skipped: usize,
}

impl ScanOutcome {
    /// Total rows touched (probed+stored) this run — the legacy `usize`
    /// `scan_into` returned, i.e. `added + updated`. Skipped/removed excluded.
    pub fn probed(&self) -> usize {
        self.added.len() + self.updated.len()
    }
}

pub trait MediaStore: Send + Sync {
    fn get(&self, id: MediaId)
        -> impl std::future::Future<Output = DomainResult<MediaItem>> + Send;
    fn put(&self, item: MediaItem) -> impl std::future::Future<Output = DomainResult<()>> + Send;
    fn list(&self) -> impl std::future::Future<Output = DomainResult<Vec<MediaItem>>> + Send;

    /// LIB-A1 — read the stored fs-stat signature for one item, or
    /// `None` when the row is absent or predates migration 0016 (no
    /// signature recorded yet, so the caller must re-probe).
    fn scan_state(
        &self,
        id: MediaId,
    ) -> impl std::future::Future<Output = DomainResult<Option<ScanState>>> + Send;

    /// LIB-A1 — open a scan run against `root`, recording the start
    /// time. Returns the new `scan_runs.id` used as the mark-and-sweep
    /// token for `mark_seen` / `sweep_unseen` / `finish_scan`.
    fn begin_scan(
        &self,
        root: &std::path::Path,
    ) -> impl std::future::Future<Output = DomainResult<i64>> + Send;

    /// LIB-A1 — stamp `id` as seen by scan run `scan_id`, persisting the
    /// freshly-stat'd `mtime` / `size`. No-op (zero rows) when the id is
    /// absent — the caller `put`s before marking on a fresh insert.
    fn mark_seen(
        &self,
        id: MediaId,
        scan_id: i64,
        mtime: i64,
        size: u64,
    ) -> impl std::future::Future<Output = DomainResult<()>> + Send;

    /// LIB-A1 — root-scoped mark-and-sweep delete. Removes
    /// `media_items` rows under `root_prefix` whose `last_seen_scan_id`
    /// is NULL or != `scan_id` (i.e. not observed by the current run),
    /// returning the deleted ids. Root-scoped so sweeping one root never
    /// deletes another root's items (V10: a single atomic DELETE).
    fn sweep_unseen(
        &self,
        scan_id: i64,
        root_prefix: &str,
    ) -> impl std::future::Future<Output = DomainResult<Vec<MediaId>>> + Send;

    /// LIB-A1 — close the scan run, recording the finish time and the
    /// seen/swept counts for observability.
    fn finish_scan(
        &self,
        scan_id: i64,
        items_seen: i64,
        items_swept: i64,
    ) -> impl std::future::Future<Output = DomainResult<()>> + Send;

    /// LIB-A6 — find the first `media_items` row whose stored content
    /// [`Fingerprint`] equals `fp`, or `None` when no row carries that
    /// fingerprint (including rows predating migration 0017 whose column
    /// is NULL). "First" is by ascending id for determinism. Lets the
    /// scanner recognise a moved/renamed file (whose path-derived id
    /// changed) by its stable content digest.
    fn find_by_fp(
        &self,
        fp: Fingerprint,
    ) -> impl std::future::Future<Output = DomainResult<Option<MediaItem>>> + Send;

    /// LIB-A6 — persist the content fingerprint for `id`. Dedicated
    /// setter (rather than widening `put`) so the scanner can stamp the
    /// fingerprint independently of the probe-write path. No-op (zero
    /// rows) when the id is absent, mirroring [`mark_seen`](Self::mark_seen).
    fn set_fingerprint(
        &self,
        id: MediaId,
        fp: Fingerprint,
    ) -> impl std::future::Future<Output = DomainResult<()>> + Send;

    /// LIB-A7 — rebind an existing row to a new `path`, keeping its `id`
    /// (and therefore every `user_data` FK / watch-history row hung off
    /// it). Used by the scanner's move/rename detection: a file recognised
    /// by content [`Fingerprint`] under a new path has its row's `path`
    /// column repointed in place rather than being swept + re-inserted
    /// under a fresh path-derived id. No-op (zero rows) when the id is
    /// absent, mirroring [`mark_seen`](Self::mark_seen).
    fn rebind_path(
        &self,
        id: MediaId,
        new_path: &std::path::Path,
    ) -> impl std::future::Future<Output = DomainResult<()>> + Send;

    /// LIB-D4 — upsert one artwork row for `(item_id, role)`. `role` is an
    /// [`ArtworkRole::as_str`] token (`"Primary"` / `"Backdrop"` / …);
    /// `source` is `"local"` or `"url"`; `locator` is the absolute sidecar
    /// path (for `local`) or the remote URL (for `url`). One row per
    /// `(item, role)` — re-`set`ing the same role overwrites the locator so
    /// the highest-priority source wins (the resolver feeds rows in
    /// priority order; the scanner writes the winner). IO-free signature —
    /// the SQL lives in the store impls (V12).
    fn set_artwork(
        &self,
        item_id: MediaId,
        role: &str,
        source: &str,
        locator: &str,
    ) -> impl std::future::Future<Output = DomainResult<()>> + Send;

    /// LIB-D4 — every artwork row for `item_id` as `(role, source,
    /// locator)` triples, ordered by `role`. Empty Vec when the item has no
    /// recorded artwork. The D5 image-serving branch reads this to serve a
    /// recorded sidecar before falling back to ffmpeg frame-extraction.
    fn artwork_for(
        &self,
        item_id: MediaId,
    ) -> impl std::future::Future<Output = DomainResult<Vec<(String, String, String)>>> + Send;
}

/// Per-(user, item) state Jellyfin tracks: watched/unwatched, play
/// count, resume position, favourite flag. T33 — drives the watched
/// indicator + resume tiles in jellyfin-web.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UserItemData {
    pub played: bool,
    pub play_count: u32,
    /// Resume position in Jellyfin's 100ns ticks (10_000_000 per
    /// second). Stays 0 once the item is fully played.
    pub last_played_position_ticks: u64,
    pub is_favorite: bool,
    /// Unix-seconds timestamp of the last progress/playback event.
    /// `0` means "never played" — kept separate from `played` so a
    /// favourited-but-never-played item still reports last_played=0.
    pub last_played_at: i64,
}

pub trait UserDataStore: Send + Sync {
    fn get_user_data(
        &self,
        user: UserId,
        item: MediaId,
    ) -> impl std::future::Future<Output = DomainResult<UserItemData>> + Send;

    fn set_user_data(
        &self,
        user: UserId,
        item: MediaId,
        data: UserItemData,
    ) -> impl std::future::Future<Output = DomainResult<()>> + Send;

    /// Bulk fetch keyed by `(user, item)`. Items not in the store
    /// default to `UserItemData::default()` — callers do not need to
    /// distinguish "row missing" from "all zeros". O(1) round trip
    /// instead of N point-fetches when rendering a library list.
    fn user_data_bulk(
        &self,
        user: UserId,
        items: &[MediaId],
    ) -> impl std::future::Future<Output = DomainResult<Vec<UserItemData>>> + Send;

    /// Item ids that have a non-zero `last_played_position_ticks` and
    /// are not flagged as played — drives Jellyfin's Resume row.
    fn resumable_items(
        &self,
        user: UserId,
    ) -> impl std::future::Future<Output = DomainResult<Vec<MediaId>>> + Send;
}

/// Per-user free-form preferences (UserConfiguration + display
/// preferences). Stored as JSON strings — the schema lives in
/// jellyfin-web's UserConfigurationDto and varies by version, so
/// the storage layer treats them as opaque payloads.
pub trait PreferenceStore: Send + Sync {
    fn get_user_configuration(
        &self,
        user: UserId,
    ) -> impl std::future::Future<Output = DomainResult<Option<String>>> + Send;

    fn set_user_configuration(
        &self,
        user: UserId,
        json: &str,
    ) -> impl std::future::Future<Output = DomainResult<()>> + Send;

    fn get_display_preferences(
        &self,
        user: UserId,
        dp_id: &str,
        client: &str,
    ) -> impl std::future::Future<Output = DomainResult<Option<String>>> + Send;

    fn set_display_preferences(
        &self,
        user: UserId,
        dp_id: &str,
        client: &str,
        json: &str,
    ) -> impl std::future::Future<Output = DomainResult<()>> + Send;
}

pub trait Scanner: Send + Sync {
    fn scan(
        &self,
        root: &std::path::Path,
    ) -> impl std::future::Future<Output = DomainResult<Vec<MediaItem>>> + Send;
}

/// Result of a single probe call. `kind` informs MediaItem classification;
/// `probe` carries the full metadata block persisted on the item.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProbeInfo {
    pub kind: MediaKind,
    pub probe: MediaProbe,
}

impl ProbeInfo {
    /// Backwards-compat shortcut for old callers that only checked
    /// `duration_ms`. Reads through to the inner probe block.
    pub fn duration_ms(&self) -> Option<u64> {
        self.probe.duration_ms
    }

    pub fn container(&self) -> Option<&str> {
        self.probe.container.as_deref()
    }
}

pub trait Prober: Send + Sync {
    fn probe(
        &self,
        path: &std::path::Path,
    ) -> impl std::future::Future<Output = DomainResult<ProbeInfo>> + Send;
}

/// Future transcoding ops (T8, T9). Inherits `probe` from `Prober`.
pub trait Transcoder: Prober {}

pub trait Clock: Send + Sync {
    fn now_unix_ms(&self) -> u64;
}

/// Live-TV channel exposed to Jellyfin clients via the /LiveTv API
/// surface (T47). `stream_url` is what the channel's video pulls
/// from — pharos may either pass-through or transcode depending on
/// the client's DeviceProfile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveChannel {
    /// Stable id within the backend (e.g. `tvg-id` from M3U or
    /// HDHomeRun's `GuideNumber`).
    pub id: String,
    pub number: String,
    pub name: String,
    pub logo_url: Option<String>,
    pub stream_url: String,
    pub group_title: Option<String>,
}

/// EPG entry — one upcoming program on a channel. `start_unix_ms`
/// / `end_unix_ms` are absolute timestamps; consumers convert to
/// Jellyfin's ISO-8601 wire shape at the DTO boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EpgProgram {
    pub channel_id: String,
    pub title: String,
    pub description: Option<String>,
    pub start_unix_ms: u64,
    pub end_unix_ms: u64,
}

pub trait TunerBackend: Send + Sync {
    fn channels(&self) -> impl std::future::Future<Output = DomainResult<Vec<LiveChannel>>> + Send;

    /// EPG programmes in `[start_unix_ms, end_unix_ms)`. Backends
    /// without an EPG return an empty Vec.
    fn programs(
        &self,
        start_unix_ms: u64,
        end_unix_ms: u64,
    ) -> impl std::future::Future<Output = DomainResult<Vec<EpgProgram>>> + Send;
}

#[cfg(test)]
mod tests;
