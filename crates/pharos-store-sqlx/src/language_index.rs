//! Backend-neutral half of the `media_item_languages` index (migration 0056):
//! how a request's language list maps onto its rows. The rows themselves are
//! written by database triggers; the SQL that reads them lives with each
//! backend.

use pharos_core::language::canonical_language;
use std::collections::BTreeSet;

pub(crate) const AUDIO: &str = "audio";
pub(crate) const SUBTITLE: &str = "subtitle";

/// The canonical tags a request list names, de-duplicated; blank entries
/// drop out (a list of only blanks filters nothing, as before).
pub(crate) fn requested(langs: &[String]) -> Vec<String> {
    langs
        .iter()
        .map(|l| canonical_language(l))
        .filter(|l| !l.is_empty())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requested_is_canonical_distinct_and_skips_blanks() {
        let asked = ["fra".to_string(), "fre".to_string(), " ".to_string()];
        assert_eq!(requested(&asked), ["fre"]);
    }
}
