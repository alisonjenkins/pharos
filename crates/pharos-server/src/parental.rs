//! T68 — parental-rating score table.
//!
//! Jellyfin maps an official-rating string (`"PG-13"`, `"TV-MA"`, …) to a
//! numeric score, then a user's `MaxParentalRating` gates which items they may
//! see. pharos resolves the same mapping here: a built-in US default table
//! (used when config carries no `[parental]` override) plus a config-supplied
//! map for non-US libraries.
//!
//! The table feeds [`ParentalRatingMap::allowed_ratings_lc`], which the item
//! query uses to build a rating allow-set — so the filter runs in SQL and page
//! totals stay honest.

use std::collections::HashMap;

/// An ordered rating→score table. Order is preserved from construction so a
/// future `/Localization/ParentalRatings` projection can render it stably.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParentalRatingMap {
    entries: Vec<(String, i32)>,
}

impl ParentalRatingMap {
    /// The built-in US rating scores, mirroring Jellyfin's default
    /// `ratings.csv` for the `us` region. Scores are ordinal — only their
    /// ordering vs `MaxParentalRating` matters.
    pub fn us_default() -> Self {
        let entries = [
            ("Approved", 0),
            ("G", 0),
            ("E", 0),
            ("EC", 0),
            ("TV-G", 0),
            ("TV-Y", 0),
            ("TV-Y7", 7),
            ("TV-Y7-FV", 7),
            ("PG", 10),
            ("TV-PG", 10),
            ("PG-13", 13),
            ("TV-14", 14),
            ("R", 17),
            ("TV-MA", 17),
            ("NC-17", 18),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
        Self { entries }
    }

    /// Build from a config-supplied `rating → score` map. An empty map yields
    /// the [`Self::us_default`] table so an absent `[parental]` section keeps
    /// the sensible built-in behaviour.
    pub fn from_config(map: &HashMap<String, i32>) -> Self {
        if map.is_empty() {
            return Self::us_default();
        }
        let mut entries: Vec<(String, i32)> = map.iter().map(|(k, v)| (k.clone(), *v)).collect();
        // Deterministic order (config maps are unordered): by score then name.
        entries.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
        Self { entries }
    }

    /// Lowercased rating strings whose score is `<= max` — the allow-set for a
    /// user with `MaxParentalRating == max`. A rating absent from the table is
    /// omitted (treated as above-max / unknown → blocked).
    pub fn allowed_ratings_lc(&self, max: i32) -> Vec<String> {
        self.entries
            .iter()
            .filter(|(_, score)| *score <= max)
            .map(|(name, _)| name.to_ascii_lowercase())
            .collect()
    }

    /// The table entries in order (`rating`, `score`). For projection / tests.
    pub fn entries(&self) -> &[(String, i32)] {
        &self.entries
    }
}

impl Default for ParentalRatingMap {
    fn default() -> Self {
        Self::us_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn max_zero_allows_only_unscored_ratings_and_blocks_pg13() {
        let map = ParentalRatingMap::us_default();
        let allowed = map.allowed_ratings_lc(0);
        assert!(allowed.contains(&"g".to_string()));
        assert!(allowed.contains(&"tv-g".to_string()));
        assert!(!allowed.contains(&"pg-13".to_string()));
        assert!(!allowed.contains(&"r".to_string()));
    }

    #[test]
    fn higher_max_widens_the_allow_set() {
        let map = ParentalRatingMap::us_default();
        let allowed = map.allowed_ratings_lc(13);
        assert!(allowed.contains(&"pg-13".to_string()));
        assert!(allowed.contains(&"pg".to_string()));
        assert!(!allowed.contains(&"r".to_string()));
    }

    #[test]
    fn empty_config_falls_back_to_us_default() {
        let map = ParentalRatingMap::from_config(&HashMap::new());
        assert_eq!(map, ParentalRatingMap::us_default());
    }

    #[test]
    fn config_override_replaces_the_table() {
        let mut cfg = HashMap::new();
        cfg.insert("18".to_string(), 18);
        cfg.insert("0".to_string(), 0);
        let map = ParentalRatingMap::from_config(&cfg);
        assert_eq!(map.allowed_ratings_lc(0), vec!["0".to_string()]);
    }
}
