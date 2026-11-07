//! Typed `IncludeItemTypes` / `ExcludeItemTypes` / `SortBy` vocabulary.
//!
//! Jellyfin clients send these as comma lists of PascalCase tokens (some
//! send camelCase or lowercase). Matching them as raw strings in several
//! places let the tables drift and made a typo silently match nothing; this
//! module is the one place the wire tokens live.
//!
//! Unknown-token policy (V583): a token outside the enum is **ignored**, never
//! an error, so a newer client naming a type or sort key pharos has never
//! heard of still gets a normal response. The list parsers drop unknown
//! tokens; the single-token parsers return `None` and the caller chooses the
//! fallback (`SortBy` falls back to `SortName`, as before).

use pharos_core::MediaKind;

macro_rules! wire_enum {
    (
        $(#[$meta:meta])*
        $name:ident { $( $(#[$vmeta:meta])* $variant:ident ),+ $(,)? }
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum $name { $( $(#[$vmeta])* $variant ),+ }

        impl $name {
            /// Every variant, in declaration order.
            pub const ALL: &'static [$name] = &[ $( $name::$variant ),+ ];

            /// The exact Jellyfin wire token.
            pub const fn as_wire(self) -> &'static str {
                match self { $( $name::$variant => stringify!($variant) ),+ }
            }

            /// Case-insensitive, whitespace-trimmed parse. `None` for a
            /// token pharos does not know (callers ignore it).
            pub fn from_wire(s: &str) -> Option<Self> {
                let s = s.trim();
                $( if s.eq_ignore_ascii_case(stringify!($variant)) {
                    return Some($name::$variant);
                } )+
                None
            }
        }
    };
}

wire_enum! {
    /// Jellyfin `BaseItemKind` (jellyfin-sdk-kotlin `BaseItemKind.kt`).
    ItemType {
        AggregateFolder, Audio, AudioBook, BasePluginFolder, Book, BoxSet, Channel,
        ChannelFolderItem, CollectionFolder, Episode, Folder, Genre,
        ManualPlaylistsFolder, Movie, LiveTvChannel, LiveTvProgram, MusicAlbum,
        MusicArtist, MusicGenre, MusicVideo, Person, Photo, PhotoAlbum, Playlist,
        PlaylistsFolder, Program, Recording, Season, Series, Studio, Trailer,
        TvChannel, TvProgram, UserRootFolder, UserView, Video, Year,
    }
}

impl ItemType {
    /// The stored [`MediaKind`] this type names, if any. Exhaustive on
    /// purpose: a new variant must decide here.
    pub const fn media_kind(self) -> Option<MediaKind> {
        match self {
            ItemType::Movie => Some(MediaKind::Movie),
            ItemType::Episode => Some(MediaKind::Episode),
            ItemType::Audio => Some(MediaKind::Audio),
            ItemType::Book => Some(MediaKind::Book),
            ItemType::AggregateFolder
            | ItemType::AudioBook
            | ItemType::BasePluginFolder
            | ItemType::BoxSet
            | ItemType::Channel
            | ItemType::ChannelFolderItem
            | ItemType::CollectionFolder
            | ItemType::Folder
            | ItemType::Genre
            | ItemType::ManualPlaylistsFolder
            | ItemType::LiveTvChannel
            | ItemType::LiveTvProgram
            | ItemType::MusicAlbum
            | ItemType::MusicArtist
            | ItemType::MusicGenre
            | ItemType::MusicVideo
            | ItemType::Person
            | ItemType::Photo
            | ItemType::PhotoAlbum
            | ItemType::Playlist
            | ItemType::PlaylistsFolder
            | ItemType::Program
            | ItemType::Recording
            | ItemType::Season
            | ItemType::Series
            | ItemType::Studio
            | ItemType::Trailer
            | ItemType::TvChannel
            | ItemType::TvProgram
            | ItemType::UserRootFolder
            | ItemType::UserView
            | ItemType::Video
            | ItemType::Year => None,
        }
    }

    /// Types pharos SYNTHESISES rather than stores: not a [`MediaKind`], but a
    /// request naming one is still answerable (a short-circuit, or folding
    /// stored rows). Every other non-stored type is one pharos holds none of,
    /// so naming only those matches nothing. Audiobooks are out of scope
    /// (specs/004-books), hence `AudioBook` is not listed.
    pub const fn is_synthesised(self) -> bool {
        match self {
            ItemType::Series
            | ItemType::Season
            | ItemType::MusicAlbum
            | ItemType::MusicArtist
            | ItemType::Playlist
            | ItemType::BoxSet
            | ItemType::Folder
            | ItemType::CollectionFolder
            | ItemType::UserView
            | ItemType::Genre
            | ItemType::MusicGenre
            | ItemType::Studio
            | ItemType::Person
            | ItemType::TvChannel
            | ItemType::LiveTvChannel
            | ItemType::Program
            | ItemType::TvProgram
            | ItemType::LiveTvProgram
            | ItemType::Recording => true,
            ItemType::Movie
            | ItemType::Episode
            | ItemType::Audio
            | ItemType::Book
            | ItemType::AggregateFolder
            | ItemType::AudioBook
            | ItemType::BasePluginFolder
            | ItemType::Channel
            | ItemType::ChannelFolderItem
            | ItemType::ManualPlaylistsFolder
            | ItemType::MusicVideo
            | ItemType::Photo
            | ItemType::PhotoAlbum
            | ItemType::PlaylistsFolder
            | ItemType::Trailer
            | ItemType::UserRootFolder
            | ItemType::Video
            | ItemType::Year => false,
        }
    }

    /// Parse a comma list (`IncludeItemTypes=Movie,Series`). Blank and unknown
    /// tokens are dropped.
    pub fn parse_list(csv: &str) -> Vec<ItemType> {
        csv.split(',').filter_map(ItemType::from_wire).collect()
    }

    /// The stored [`MediaKind`]s a comma list names, in list order. Unknown
    /// and non-stored tokens are dropped.
    pub fn stored_kinds(csv: &str) -> Vec<MediaKind> {
        ItemType::parse_list(csv)
            .into_iter()
            .filter_map(ItemType::media_kind)
            .collect()
    }

    /// Whether a comma list names at least one token and every token parses to
    /// a type in `allowed`. Unknown tokens count as "not allowed", so
    /// `Series,Bogus` is not a pure show-folder request.
    pub fn list_is_only(csv: &str, allowed: &[ItemType]) -> bool {
        let mut any = false;
        for t in csv.split(',').map(str::trim).filter(|t| !t.is_empty()) {
            match ItemType::from_wire(t) {
                Some(ty) if allowed.contains(&ty) => any = true,
                _ => return false,
            }
        }
        any
    }
}

wire_enum! {
    /// Jellyfin `ItemSortBy` (jellyfin-sdk-kotlin `ItemSortBy.kt`) plus the
    /// legacy tokens pharos has always accepted: `None`, `RuntimeTicks`,
    /// `DateAdded`, `ParentalRating`.
    SortField {
        Default, AiredEpisodeOrder, Album, AlbumArtist, Artist, DateCreated,
        OfficialRating, DatePlayed, PremiereDate, StartDate, SortName, Name, Random,
        Runtime, CommunityRating, ProductionYear, PlayCount, CriticRating, IsFolder,
        IsUnplayed, IsPlayed, SeriesSortName, VideoBitRate, AirTime, Studio,
        IsFavoriteOrLiked, DateLastContentAdded, SeriesDatePlayed,
        ParentIndexNumber, IndexNumber,
        // Legacy, not in the kotlin enum.
        None, RuntimeTicks, DateAdded, ParentalRating,
    }
}

impl SortField {
    /// Parse a `SortBy` comma list. Blank and unknown tokens are dropped, so
    /// `SortBy=Bogus` is an empty list (callers treat that as `SortName`).
    pub fn parse_list(csv: &str) -> Vec<SortField> {
        csv.split(',').filter_map(SortField::from_wire).collect()
    }

    /// The primary key of a `SortBy` comma list: the FIRST non-empty token,
    /// parsed. An unknown first token is `None` even when a later token is
    /// known, so `Bogus,PremiereDate` has no primary.
    pub fn first_of(csv: &str) -> Option<SortField> {
        csv.split(',')
            .map(str::trim)
            .find(|t| !t.is_empty())
            .and_then(SortField::from_wire)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    const KOTLIN_BASE_ITEM_KIND: &str = "AggregateFolder Audio AudioBook BasePluginFolder Book BoxSet Channel ChannelFolderItem CollectionFolder Episode Folder Genre ManualPlaylistsFolder Movie LiveTvChannel LiveTvProgram MusicAlbum MusicArtist MusicGenre MusicVideo Person Photo PhotoAlbum Playlist PlaylistsFolder Program Recording Season Series Studio Trailer TvChannel TvProgram UserRootFolder UserView Video Year";
    const KOTLIN_ITEM_SORT_BY: &str = "Default AiredEpisodeOrder Album AlbumArtist Artist DateCreated OfficialRating DatePlayed PremiereDate StartDate SortName Name Random Runtime CommunityRating ProductionYear PlayCount CriticRating IsFolder IsUnplayed IsPlayed SeriesSortName VideoBitRate AirTime Studio IsFavoriteOrLiked DateLastContentAdded SeriesDatePlayed ParentIndexNumber IndexNumber";

    #[test]
    fn item_type_round_trips_the_kotlin_list() {
        let wire: Vec<&str> = KOTLIN_BASE_ITEM_KIND.split(' ').collect();
        assert_eq!(wire.len(), ItemType::ALL.len());
        for w in wire {
            let t = ItemType::from_wire(w).unwrap_or_else(|| panic!("{w} must parse"));
            assert_eq!(t.as_wire(), w);
        }
    }

    #[test]
    fn sort_field_round_trips_the_kotlin_list_plus_legacy() {
        for w in KOTLIN_ITEM_SORT_BY.split(' ') {
            let f = SortField::from_wire(w).unwrap_or_else(|| panic!("{w} must parse"));
            assert_eq!(f.as_wire(), w);
        }
        assert_eq!(SortField::ALL.len(), 30 + 4);
        for w in ["None", "RuntimeTicks", "DateAdded", "ParentalRating"] {
            assert_eq!(SortField::from_wire(w).unwrap().as_wire(), w);
        }
    }

    #[test]
    fn parsing_is_case_insensitive_and_trims() {
        assert_eq!(ItemType::from_wire("movie"), Some(ItemType::Movie));
        assert_eq!(
            ItemType::from_wire(" MUSICALBUM "),
            Some(ItemType::MusicAlbum)
        );
        assert_eq!(ItemType::from_wire("boxSet"), Some(ItemType::BoxSet));
        assert_eq!(SortField::from_wire("sortname"), Some(SortField::SortName));
        assert_eq!(
            SortField::from_wire("dateCreated"),
            Some(SortField::DateCreated)
        );
    }

    #[test]
    fn unknown_values_are_ignored_not_errors() {
        assert_eq!(ItemType::from_wire("Bogus"), None);
        assert_eq!(ItemType::from_wire(""), None);
        assert_eq!(
            ItemType::parse_list("Movie,Bogus,,episode"),
            vec![ItemType::Movie, ItemType::Episode]
        );
        assert!(SortField::parse_list("Bogus").is_empty());
        assert_eq!(SortField::parse_list("Bogus, Name"), vec![SortField::Name]);
    }

    #[test]
    fn media_kind_is_read_through_item_type() {
        // Case-insensitive and trimmed, as the retired MediaKind::from_wire was.
        assert_eq!(
            ItemType::from_wire("Book").and_then(ItemType::media_kind),
            Some(MediaKind::Book)
        );
        assert_eq!(
            ItemType::from_wire(" audio ").and_then(ItemType::media_kind),
            Some(MediaKind::Audio)
        );
        assert_eq!(
            ItemType::from_wire("EPISODE").and_then(ItemType::media_kind),
            Some(MediaKind::Episode)
        );
        for t in ["Series", "Season", "MusicAlbum", "BoxSet", "Person", ""] {
            assert_eq!(
                ItemType::from_wire(t).and_then(ItemType::media_kind),
                None,
                "{t} must not map to a stored kind"
            );
        }
    }

    #[test]
    fn first_of_takes_the_first_non_empty_token_only() {
        assert_eq!(
            SortField::first_of(" ,DateCreated"),
            Some(SortField::DateCreated)
        );
        assert_eq!(
            SortField::first_of("Name,DateCreated"),
            Some(SortField::Name)
        );
        assert_eq!(SortField::first_of(""), None);
        assert_eq!(SortField::first_of(","), None);
        assert_eq!(SortField::first_of("Bogus,PremiereDate"), None);
    }

    #[test]
    fn stored_kinds_keeps_only_storable_tokens_in_order() {
        assert_eq!(
            ItemType::stored_kinds("Movie,Series,bogus,episode,MusicAlbum,Audio"),
            vec![MediaKind::Movie, MediaKind::Episode, MediaKind::Audio]
        );
        assert!(ItemType::stored_kinds("Series,Bogus").is_empty());
        assert!(ItemType::stored_kinds(" , ").is_empty());
    }

    #[test]
    fn media_kind_and_synthesised_partition() {
        let stored = ItemType::ALL
            .iter()
            .filter(|t| t.media_kind().is_some())
            .count();
        assert_eq!(stored, 4);
        for t in ItemType::ALL {
            assert!(
                !(t.media_kind().is_some() && t.is_synthesised()),
                "{t:?} is both stored and synthesised"
            );
        }
        assert!(!ItemType::AudioBook.is_synthesised());
        assert!(ItemType::Recording.is_synthesised());
    }

    #[test]
    fn list_is_only_requires_every_token_allowed() {
        let only = [ItemType::Series, ItemType::Season];
        assert!(ItemType::list_is_only("series, SEASON", &only));
        assert!(!ItemType::list_is_only("Series,Movie", &only));
        assert!(!ItemType::list_is_only("Series,Bogus", &only));
        assert!(!ItemType::list_is_only("", &only));
        assert!(!ItemType::list_is_only(" , ", &only));
    }
}
