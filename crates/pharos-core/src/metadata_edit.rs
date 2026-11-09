//! Admin metadata edits (spec 015 US3, ADR-0097).
//!
//! An edit writes the new value into the item's own column and sets that
//! field's bit in the row's `locked_fields` mask. Every write path that
//! refreshes metadata (a scan's put, the series enricher's upsert) keeps the
//! stored value of a locked column instead of its own (V1030), so an edit
//! survives a rescan, an NFO change and an online refresh. Unlocking a field
//! hands it back: the next scan or refresh writes it again.

use crate::{DomainResult, MediaId};

/// A field an admin can edit and lock. The discriminant is the field's bit
/// position in the stored mask, so a variant's number never changes; a new
/// field takes the next number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum EditableField {
    Title = 0,
    SortName = 1,
    Overview = 2,
    Tagline = 3,
    ProductionYear = 4,
    PremiereDate = 5,
    OfficialRating = 6,
    CommunityRating = 7,
    CriticRating = 8,
    Genres = 9,
}

impl EditableField {
    pub const ALL: [Self; 10] = [
        Self::Title,
        Self::SortName,
        Self::Overview,
        Self::Tagline,
        Self::ProductionYear,
        Self::PremiereDate,
        Self::OfficialRating,
        Self::CommunityRating,
        Self::CriticRating,
        Self::Genres,
    ];

    /// The field's bit in [`LockedFields`].
    pub const fn bit(self) -> u32 {
        1 << (self as u8)
    }

    /// Stable metric label and log value.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Title => "title",
            Self::SortName => "sort_name",
            Self::Overview => "overview",
            Self::Tagline => "tagline",
            Self::ProductionYear => "production_year",
            Self::PremiereDate => "premiere_date",
            Self::OfficialRating => "official_rating",
            Self::CommunityRating => "community_rating",
            Self::CriticRating => "critic_rating",
            Self::Genres => "genres",
        }
    }

    /// The field's name on the admin REST API.
    pub const fn wire(self) -> &'static str {
        match self {
            Self::Title => "Title",
            Self::SortName => "SortName",
            Self::Overview => "Overview",
            Self::Tagline => "Tagline",
            Self::ProductionYear => "ProductionYear",
            Self::PremiereDate => "PremiereDate",
            Self::OfficialRating => "OfficialRating",
            Self::CommunityRating => "CommunityRating",
            Self::CriticRating => "CriticRating",
            Self::Genres => "Genres",
        }
    }

    /// Parses a [`wire`](Self::wire) name, ignoring ASCII case.
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|f| f.wire().eq_ignore_ascii_case(s.trim()))
    }

    /// Whether a film, episode, song or book row can carry this field.
    /// Genres there are a join the scanner relinks, not a column, so they
    /// wait for a later slice.
    pub const fn on_item(self) -> bool {
        !matches!(self, Self::Genres)
    }

    /// Whether a show's `series_metadata` row can carry this field. A show's
    /// name is its identity (spec 033), so renaming one is not an edit.
    pub const fn on_series(self) -> bool {
        matches!(
            self,
            Self::Overview
                | Self::PremiereDate
                | Self::OfficialRating
                | Self::CommunityRating
                | Self::Genres
        )
    }
}

/// The set of locked fields, stored as an integer mask. Bits naming no
/// field (written by a newer server, say) are dropped on read.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct LockedFields(u32);

impl LockedFields {
    const KNOWN: u32 = {
        let mut m = 0;
        let mut i = 0;
        while i < EditableField::ALL.len() {
            m |= EditableField::ALL[i].bit();
            i += 1;
        }
        m
    };

    pub const NONE: Self = Self(0);

    /// The mask as stored. Never negative.
    pub const fn bits(self) -> i64 {
        self.0 as i64
    }

    /// A stored mask. Unknown and out-of-range bits are dropped.
    pub fn from_bits(raw: i64) -> Self {
        Self(u32::try_from(raw & i64::from(Self::KNOWN)).unwrap_or(0))
    }

    pub const fn contains(self, f: EditableField) -> bool {
        self.0 & f.bit() != 0
    }

    pub fn insert(&mut self, f: EditableField) {
        self.0 |= f.bit();
    }

    pub fn remove(&mut self, f: EditableField) {
        self.0 &= !f.bit();
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    pub const fn without(self, other: Self) -> Self {
        Self(self.0 & !other.0)
    }

    pub fn iter(self) -> impl Iterator<Item = EditableField> {
        EditableField::ALL
            .into_iter()
            .filter(move |f| self.contains(*f))
    }
}

impl FromIterator<EditableField> for LockedFields {
    fn from_iter<I: IntoIterator<Item = EditableField>>(iter: I) -> Self {
        let mut out = Self::NONE;
        for f in iter {
            out.insert(f);
        }
        out
    }
}

/// One admin edit. A field left `None` is untouched; `Some(None)` clears it
/// (an admin may want no tagline at all, and that must survive the next
/// scan too). Every field set is locked; every field in `unlock` is
/// unlocked, keeping its current value until the next refresh.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MetadataEdit {
    pub title: Option<String>,
    pub sort_name: Option<Option<String>>,
    pub overview: Option<Option<String>>,
    pub tagline: Option<Option<String>>,
    pub production_year: Option<Option<u32>>,
    pub premiere_date: Option<Option<i64>>,
    pub official_rating: Option<Option<String>>,
    pub community_rating: Option<Option<f32>>,
    pub critic_rating: Option<Option<f32>>,
    pub genres: Option<Vec<String>>,
    pub unlock: LockedFields,
}

/// Longest title or sort title an edit accepts, in characters.
pub const MAX_TITLE_CHARS: usize = 500;
/// Longest overview, in characters: well past any provider's synopsis.
pub const MAX_OVERVIEW_CHARS: usize = 20_000;
/// Longest tagline or parental rating string, in characters.
pub const MAX_SHORT_TEXT_CHARS: usize = 200;
/// Most genres one show can carry.
pub const MAX_GENRES: usize = 50;
/// Earliest and latest year an edit accepts: the first films are from the
/// 1880s, and a year past 2200 is a typo.
pub const YEAR_RANGE: std::ops::RangeInclusive<u32> = 1850..=2200;

/// Why an edit was refused, naming the field and the value it carried.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum EditRejected {
    #[error("{field} cannot be edited on a {target}")]
    NotEditable {
        field: &'static str,
        target: &'static str,
    },
    #[error("{field} is {chars} characters, longer than the {max} allowed")]
    TooLong {
        field: &'static str,
        chars: usize,
        max: usize,
    },
    #[error("Title cannot be empty")]
    EmptyTitle,
    #[error("ProductionYear {0} is outside 1850..=2200")]
    Year(u32),
    #[error("{field} {value} is outside 0..={max}")]
    Rating {
        field: &'static str,
        value: f32,
        max: f32,
    },
    #[error("Genres has {0} entries, more than the 50 allowed")]
    TooManyGenres(usize),
}

impl EditRejected {
    /// The field the refusal is about, as the REST API names it, so a form
    /// can show the server's reason against that field (FR-003).
    pub fn field(&self) -> &'static str {
        match self {
            Self::NotEditable { field, .. }
            | Self::TooLong { field, .. }
            | Self::Rating { field, .. } => field,
            Self::EmptyTitle => EditableField::Title.wire(),
            Self::Year(_) => EditableField::ProductionYear.wire(),
            Self::TooManyGenres(_) => EditableField::Genres.wire(),
        }
    }

    /// Stable metric label for the refusal's class.
    pub const fn label(&self) -> &'static str {
        match self {
            Self::NotEditable { .. } => "not_editable",
            Self::TooLong { .. } => "too_long",
            Self::EmptyTitle => "empty_title",
            Self::Year(_) => "year",
            Self::Rating { .. } => "rating",
            Self::TooManyGenres(_) => "too_many_genres",
        }
    }
}

/// What an edit is applied to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditTarget {
    /// A `media_items` row.
    Item(MediaId),
    /// A show's `series_metadata` row, created if the show has none yet.
    Series { key: String, name: String },
}

impl EditTarget {
    const fn noun(&self) -> &'static str {
        match self {
            Self::Item(_) => "item",
            Self::Series { .. } => "show",
        }
    }

    fn allows(&self, f: EditableField) -> bool {
        match self {
            Self::Item(_) => f.on_item(),
            Self::Series { .. } => f.on_series(),
        }
    }
}

fn trimmed(v: &Option<Option<String>>) -> Option<Option<String>> {
    v.as_ref().map(|inner| {
        inner
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    })
}

fn check_len(field: EditableField, v: Option<&str>, max: usize) -> Result<(), EditRejected> {
    let chars = v.map_or(0, |s| s.chars().count());
    if chars > max {
        return Err(EditRejected::TooLong {
            field: field.wire(),
            chars,
            max,
        });
    }
    Ok(())
}

fn check_rating(field: EditableField, v: Option<f32>, max: f32) -> Result<(), EditRejected> {
    match v {
        Some(r) if !(0.0..=max).contains(&r) => Err(EditRejected::Rating {
            field: field.wire(),
            value: r,
            max,
        }),
        _ => Ok(()),
    }
}

impl MetadataEdit {
    /// The fields this edit sets, which it also locks.
    pub fn fields(&self) -> LockedFields {
        use EditableField as F;
        [
            (F::Title, self.title.is_some()),
            (F::SortName, self.sort_name.is_some()),
            (F::Overview, self.overview.is_some()),
            (F::Tagline, self.tagline.is_some()),
            (F::ProductionYear, self.production_year.is_some()),
            (F::PremiereDate, self.premiere_date.is_some()),
            (F::OfficialRating, self.official_rating.is_some()),
            (F::CommunityRating, self.community_rating.is_some()),
            (F::CriticRating, self.critic_rating.is_some()),
            (F::Genres, self.genres.is_some()),
        ]
        .into_iter()
        .filter_map(|(f, set)| set.then_some(f))
        .collect()
    }

    /// The edit with text trimmed, blank text cleared and genres de-duplicated,
    /// checked against the limits above and against what `target` can carry.
    pub fn normalised(&self, target: &EditTarget) -> Result<Self, EditRejected> {
        use EditableField as F;
        for f in self.fields().union(self.unlock).iter() {
            if !target.allows(f) {
                return Err(EditRejected::NotEditable {
                    field: f.wire(),
                    target: target.noun(),
                });
            }
        }
        let title = match self.title.as_deref().map(str::trim) {
            Some("") => return Err(EditRejected::EmptyTitle),
            t => t.map(str::to_owned),
        };
        check_len(F::Title, title.as_deref(), MAX_TITLE_CHARS)?;
        let sort_name = trimmed(&self.sort_name);
        check_len(
            F::SortName,
            sort_name.clone().flatten().as_deref(),
            MAX_TITLE_CHARS,
        )?;
        let overview = trimmed(&self.overview);
        check_len(
            F::Overview,
            overview.clone().flatten().as_deref(),
            MAX_OVERVIEW_CHARS,
        )?;
        let tagline = trimmed(&self.tagline);
        check_len(
            F::Tagline,
            tagline.clone().flatten().as_deref(),
            MAX_SHORT_TEXT_CHARS,
        )?;
        let official_rating = trimmed(&self.official_rating);
        check_len(
            F::OfficialRating,
            official_rating.clone().flatten().as_deref(),
            MAX_SHORT_TEXT_CHARS,
        )?;
        if let Some(Some(y)) = self.production_year {
            if !YEAR_RANGE.contains(&y) {
                return Err(EditRejected::Year(y));
            }
        }
        check_rating(F::CommunityRating, self.community_rating.flatten(), 10.0)?;
        check_rating(F::CriticRating, self.critic_rating.flatten(), 100.0)?;
        let genres = match &self.genres {
            None => None,
            Some(list) => {
                let mut out: Vec<String> = Vec::with_capacity(list.len());
                for g in list.iter().map(|g| g.trim()).filter(|g| !g.is_empty()) {
                    check_len(F::Genres, Some(g), MAX_SHORT_TEXT_CHARS)?;
                    if !out.iter().any(|e| e.eq_ignore_ascii_case(g)) {
                        out.push(g.to_owned());
                    }
                }
                if out.len() > MAX_GENRES {
                    return Err(EditRejected::TooManyGenres(out.len()));
                }
                Some(out)
            }
        };
        Ok(Self {
            title,
            sort_name,
            overview,
            tagline,
            production_year: self.production_year,
            premiere_date: self.premiere_date,
            official_rating,
            community_rating: self.community_rating,
            critic_rating: self.critic_rating,
            genres,
            unlock: self.unlock.without(self.fields()),
        })
    }
}

/// Writes admin edits. Split from [`crate::MediaStore`] like
/// [`crate::RatingsStore`] so in-memory test stores don't have to implement it.
pub trait MetadataEditStore: Send + Sync {
    /// Apply `edit` (already [`normalised`](MetadataEdit::normalised)) and
    /// return the target's locked fields afterwards. An item that does not
    /// exist is `DomainError::NotFound`; a show with no metadata row gets one.
    fn edit_metadata(
        &self,
        target: &EditTarget,
        edit: &MetadataEdit,
    ) -> impl std::future::Future<Output = DomainResult<LockedFields>> + Send;

    /// The item's stored sort title, which [`crate::MediaItem`] does not
    /// carry. `None` for an item with none stored or no row.
    fn stored_sort_name(
        &self,
        id: MediaId,
    ) -> impl std::future::Future<Output = DomainResult<Option<String>>> + Send;
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn bits_are_distinct_and_round_trip() {
        let all: LockedFields = EditableField::ALL.into_iter().collect();
        assert_eq!(all.iter().count(), EditableField::ALL.len());
        assert_eq!(LockedFields::from_bits(all.bits()), all);
        for f in EditableField::ALL {
            assert_eq!(EditableField::parse(f.wire()), Some(f));
            assert_eq!(EditableField::parse(&f.wire().to_lowercase()), Some(f));
        }
    }

    #[test]
    fn labels_are_distinct() {
        let mut seen = std::collections::HashSet::new();
        for f in EditableField::ALL {
            assert!(seen.insert(f.label()), "duplicate label {}", f.label());
        }
    }

    #[test]
    fn refusal_labels_are_distinct() {
        let all = [
            EditRejected::NotEditable {
                field: "Title",
                target: "show",
            },
            EditRejected::TooLong {
                field: "Overview",
                chars: 1,
                max: 0,
            },
            EditRejected::EmptyTitle,
            EditRejected::Year(1),
            EditRejected::Rating {
                field: "CriticRating",
                value: 1.0,
                max: 0.0,
            },
            EditRejected::TooManyGenres(51),
        ];
        let labels: std::collections::HashSet<_> = all.iter().map(EditRejected::label).collect();
        assert_eq!(labels.len(), all.len());
    }

    #[test]
    fn unknown_and_negative_bits_are_dropped() {
        assert_eq!(LockedFields::from_bits(1 << 40), LockedFields::NONE);
        assert_eq!(
            LockedFields::from_bits(-1).bits(),
            LockedFields::KNOWN as i64
        );
    }

    #[test]
    fn normalising_trims_clears_blanks_and_dedups_genres() {
        let edit = MetadataEdit {
            title: Some("  Alien  ".into()),
            tagline: Some(Some("   ".into())),
            ..Default::default()
        };
        let n = edit.normalised(&EditTarget::Item(1)).unwrap();
        assert_eq!(n.title.as_deref(), Some("Alien"));
        assert_eq!(n.tagline, Some(None));
        let show = EditTarget::Series {
            key: "k".into(),
            name: "Show".into(),
        };
        let edit = MetadataEdit {
            genres: Some(vec![
                "Drama".into(),
                " drama ".into(),
                "".into(),
                "Crime".into(),
            ]),
            ..Default::default()
        };
        assert_eq!(
            edit.normalised(&show).unwrap().genres,
            Some(vec!["Drama".to_string(), "Crime".to_string()])
        );
    }

    #[test]
    fn refusals_name_the_field_and_value() {
        let item = EditTarget::Item(1);
        let err = MetadataEdit {
            title: Some(" ".into()),
            ..Default::default()
        }
        .normalised(&item)
        .unwrap_err();
        assert_eq!(err, EditRejected::EmptyTitle);
        let err = MetadataEdit {
            production_year: Some(Some(19999)),
            ..Default::default()
        }
        .normalised(&item)
        .unwrap_err();
        assert_eq!(err.field(), "ProductionYear");
        assert!(err.to_string().contains("19999"));
        let err = MetadataEdit {
            community_rating: Some(Some(11.0)),
            ..Default::default()
        }
        .normalised(&item)
        .unwrap_err();
        assert_eq!(err.field(), "CommunityRating");
        assert!(err.to_string().contains("11"));
    }

    #[test]
    fn a_target_refuses_fields_it_cannot_carry() {
        let err = MetadataEdit {
            genres: Some(vec!["Drama".into()]),
            ..Default::default()
        }
        .normalised(&EditTarget::Item(1))
        .unwrap_err();
        assert_eq!(err.label(), "not_editable");
        let show = EditTarget::Series {
            key: "k".into(),
            name: "Show".into(),
        };
        let err = MetadataEdit {
            title: Some("Renamed".into()),
            ..Default::default()
        }
        .normalised(&show)
        .unwrap_err();
        assert_eq!(err.field(), "Title");
    }

    #[test]
    fn a_field_set_and_unlocked_stays_locked() {
        let mut unlock = LockedFields::NONE;
        unlock.insert(EditableField::Overview);
        unlock.insert(EditableField::Tagline);
        let n = MetadataEdit {
            overview: Some(Some("New".into())),
            unlock,
            ..Default::default()
        }
        .normalised(&EditTarget::Item(1))
        .unwrap();
        assert!(!n.unlock.contains(EditableField::Overview));
        assert!(n.unlock.contains(EditableField::Tagline));
    }
}
