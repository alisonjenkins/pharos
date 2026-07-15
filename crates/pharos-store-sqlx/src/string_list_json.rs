//! JSON wire shape for the `Vec<String>` metadata columns
//! (`production_locations`, `trailers` — T67).
//!
//! Mirrors `provider_ids_json`: the column stores a JSON array string
//! (`["US","GB"]`); an EMPTY list encodes to `None` (NULL column) so the
//! common no-value case round-trips cleanly and existing NULL rows decode
//! back to an empty `Vec`.

/// Encode to a JSON array string, or `None` when the list is empty (stored
/// as a NULL column).
pub fn encode(items: &[String]) -> Option<String> {
    if items.is_empty() {
        return None;
    }
    serde_json::to_string(items).ok()
}

/// Decode a stored JSON array string back into a `Vec<String>`. `None` or
/// malformed JSON yields an empty `Vec`.
pub fn decode(s: Option<&str>) -> Vec<String> {
    let Some(s) = s else {
        return Vec::new();
    };
    serde_json::from_str::<Vec<String>>(s).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn round_trips_through_json() {
        let v = vec!["US".to_string(), "GB".to_string()];
        let s = encode(&v).unwrap();
        assert_eq!(decode(Some(s.as_str())), v);
    }

    #[test]
    fn empty_encodes_to_none_and_decodes_to_empty() {
        assert!(encode(&[]).is_none());
        assert!(decode(None).is_empty());
    }

    #[test]
    fn malformed_json_decodes_to_empty() {
        assert!(decode(Some("not json")).is_empty());
    }
}
