//! Keyed image tags (ADR-0073, V712).
//!
//! Every image URL a Jellyfin client builds echoes the `ImageTag` the server
//! handed it in a listing: `/Items/{id}/Images/{role}?tag={tag}`. The native
//! apps send no credential with those requests, so pharos serves them
//! anonymously (ADR-0040). Before this module the tags were unkeyed hashes of
//! the item id and art version, so anyone could build a valid image URL for
//! any id without signing in.
//!
//! A signed tag is the unsigned ("inner") tag followed by a fixed-width MAC:
//!
//! ```text
//! tag = inner ‖ hex(HMAC-SHA256(key, "pharos-image-tag/v1" 0 id 0 role 0 inner)[..8])
//! ```
//!
//! The MAC binds the item id the URL names and the image role, so a tag
//! minted for one image authorises no other. The inner tag still carries the
//! art version, so a replaced image still changes the URL and busts client
//! caches (B124). Verification needs nothing but the key and the URL: the
//! inner tag rides inside the tag, so a stale tag from a cached listing still
//! verifies after the art changes.
//!
//! The DTO builders keep minting inner tags. [`sign_image_tags`] signs a
//! whole response body on its way out, pairing each tag field with the id
//! field a client puts in the URL. One table here instead of a signer at
//! each of the ~70 places a tag is built.

use hmac::{Hmac, KeyInit, Mac};
use serde_json::Value;
use sha2::Sha256;
use subtle::ConstantTimeEq;

/// Hex digits of MAC appended to the inner tag (8 bytes, 64 bits).
pub const MAC_HEX_LEN: usize = 16;

/// Domain separator, versioned so a change of construction can never
/// accept a tag minted under the old one.
const DOMAIN: &[u8] = b"pharos-image-tag/v1";

/// The server's image-tag key: 32 random bytes, persisted so tags stay
/// valid across restarts.
#[derive(Clone)]
pub struct ImageTagKey([u8; 32]);

impl std::fmt::Debug for ImageTagKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ImageTagKey(REDACTED)")
    }
}

impl ImageTagKey {
    /// Length of a key in bytes.
    pub const LEN: usize = 32;

    /// A key from exactly [`Self::LEN`] bytes; `None` for any other length.
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        <[u8; 32]>::try_from(bytes).ok().map(Self)
    }

    /// A key from an array of exactly the right length.
    pub const fn from_array(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// The raw bytes, for persisting. Never log them.
    pub fn to_bytes(&self) -> [u8; 32] {
        self.0
    }

    fn mac_hex(&self, wire_id: &str, role: &str, inner: &str) -> String {
        // HMAC accepts a key of any length, so this cannot fail; the
        // fallback keeps clippy's expect/unwrap ban without a panic path.
        let Ok(mut mac) = <Hmac<Sha256> as KeyInit>::new_from_slice(&self.0) else {
            return String::new();
        };
        mac.update(DOMAIN);
        mac.update(&[0]);
        mac.update(canonical_id(wire_id).as_bytes());
        mac.update(&[0]);
        mac.update(role.to_ascii_lowercase().as_bytes());
        mac.update(&[0]);
        mac.update(inner.as_bytes());
        let out = mac.finalize().into_bytes();
        let mut hex = String::with_capacity(MAC_HEX_LEN);
        for b in &out[..MAC_HEX_LEN / 2] {
            hex.push_str(&format!("{b:02x}"));
        }
        hex
    }

    /// Sign `inner` for the image `role` of the item whose URL id is
    /// `wire_id`.
    pub fn sign(&self, wire_id: &str, role: &str, inner: &str) -> String {
        format!("{inner}{}", self.mac_hex(wire_id, role, inner))
    }

    /// Check the `tag=` a request carried against the id and role in its
    /// path.
    pub fn verify(&self, wire_id: &str, role: &str, tag: Option<&str>) -> TagVerdict {
        let Some(tag) = tag.filter(|t| !t.is_empty()) else {
            return TagVerdict::Absent;
        };
        if tag.len() <= MAC_HEX_LEN || !tag.is_char_boundary(tag.len() - MAC_HEX_LEN) {
            return TagVerdict::Invalid;
        }
        let (inner, mac) = tag.split_at(tag.len() - MAC_HEX_LEN);
        let want = self.mac_hex(wire_id, role, inner);
        if bool::from(want.as_bytes().ct_eq(mac.to_ascii_lowercase().as_bytes())) {
            TagVerdict::Valid
        } else {
            TagVerdict::Invalid
        }
    }
}

/// The role a tag is signed for, with the image's index folded in, so a tag
/// minted for one backdrop or chapter authorises no other. Index 0 is the bare
/// role: `/Images/Backdrop` and `/Images/Backdrop/0` name the same image.
pub fn indexed_role(role: &str, index: usize) -> String {
    if index == 0 {
        role.to_owned()
    } else {
        format!("{role}/{index}")
    }
}

/// The ids clients send are dashed or dashless and of either case (B85);
/// both sides of the MAC see the same canonical form.
fn canonical_id(wire_id: &str) -> String {
    wire_id
        .chars()
        .filter(|c| *c != '-')
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// What an anonymous image request's `tag=` proved. A metric label: the
/// strings are a dashboard contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TagVerdict {
    /// The tag was minted by this server for this id and role.
    Valid,
    /// A tag was sent but this server did not mint it for this image: an
    /// unkeyed tag from before ADR-0073, a different key, or a guess.
    Invalid,
    /// No `tag=` at all.
    Absent,
}

impl TagVerdict {
    pub fn label(self) -> &'static str {
        match self {
            Self::Valid => "valid",
            Self::Invalid => "invalid",
            Self::Absent => "absent",
        }
    }
}

/// Tag fields whose image belongs to another item, with the field naming
/// that item and the role the client requests. jellyfin-web and the Kotlin
/// SDK both build these URLs from exactly these pairs.
const PARENT_TAGS: &[(&str, &str, &str)] = &[
    ("SeriesPrimaryImageTag", "SeriesId", "primary"),
    ("SeriesThumbImageTag", "SeriesId", "thumb"),
    ("ParentThumbImageTag", "ParentThumbItemId", "thumb"),
    ("ParentLogoImageTag", "ParentLogoItemId", "logo"),
    ("ParentArtImageTag", "ParentArtItemId", "art"),
    (
        "ParentPrimaryImageTag",
        "ParentPrimaryImageItemId",
        "primary",
    ),
    ("AlbumPrimaryImageTag", "AlbumId", "primary"),
    ("ChannelPrimaryImageTag", "ChannelId", "primary"),
];

/// Sign every image tag in a response body in place. Returns how many tags
/// were signed. Person portraits (`People[].PrimaryImageTag`) and user
/// avatars are left alone: their routes are not tag-checked (ADR-0073).
pub fn sign_image_tags(key: &ImageTagKey, body: &mut Value) -> usize {
    match body {
        Value::Array(items) => items.iter_mut().map(|v| sign_image_tags(key, v)).sum(),
        Value::Object(obj) => {
            let mut n = 0;
            let own_id = obj.get("Id").and_then(Value::as_str).map(str::to_owned);
            if let Some(id) = own_id.as_deref() {
                if let Some(Value::Object(tags)) = obj.get_mut("ImageTags") {
                    for (role, tag) in tags.iter_mut() {
                        n += sign_in_place(key, id, role, tag);
                    }
                }
                for (field, role) in [
                    ("BackdropImageTags", "backdrop"),
                    ("ScreenshotImageTags", "screenshot"),
                ] {
                    if let Some(Value::Array(tags)) = obj.get_mut(field) {
                        for (i, tag) in tags.iter_mut().enumerate() {
                            n += sign_in_place(key, id, &indexed_role(role, i), tag);
                        }
                    }
                }
                if let Some(Value::Array(chapters)) = obj.get_mut("Chapters") {
                    for (i, ch) in chapters.iter_mut().enumerate() {
                        if let Some(tag) = ch.get_mut("ImageTag") {
                            n += sign_in_place(key, id, &indexed_role("chapter", i), tag);
                        }
                    }
                }
            }
            for (tag_field, id_field, role) in PARENT_TAGS {
                let Some(parent) = obj
                    .get(*id_field)
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                else {
                    continue;
                };
                if let Some(tag) = obj.get_mut(*tag_field) {
                    n += sign_in_place(key, &parent, role, tag);
                }
            }
            if let Some(Value::Array(tags)) = obj.get_mut("ParentBackdropImageTags") {
                // Borrow dance: the id lives in the same object.
                let tags = std::mem::take(tags);
                let parent = obj
                    .get("ParentBackdropItemId")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                let tags: Vec<Value> = match parent {
                    Some(p) => tags
                        .into_iter()
                        .enumerate()
                        .map(|(i, mut t)| {
                            n += sign_in_place(key, &p, &indexed_role("backdrop", i), &mut t);
                            t
                        })
                        .collect(),
                    None => tags,
                };
                obj.insert("ParentBackdropImageTags".into(), Value::Array(tags));
            }
            for (field, child) in obj.iter_mut() {
                // Already handled above; their values hold no nested DTOs.
                if matches!(field.as_str(), "ImageTags" | "Chapters") {
                    continue;
                }
                if child.is_object() || child.is_array() {
                    n += sign_image_tags(key, child);
                }
            }
            n
        }
        _ => 0,
    }
}

fn sign_in_place(key: &ImageTagKey, id: &str, role: &str, tag: &mut Value) -> usize {
    match tag {
        Value::String(inner) if !inner.is_empty() => {
            *inner = key.sign(id, role, inner);
            1
        }
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use serde_json::json;

    fn key(b: u8) -> ImageTagKey {
        ImageTagKey::from_bytes(&[b; 32]).unwrap()
    }

    #[test]
    fn a_signed_tag_verifies_only_for_its_own_id_role_and_key() {
        let k = key(1);
        let tag = k.sign("abc", "primary", "0123456789abcdef");
        assert_eq!(tag.len(), 32, "same width as a Jellyfin tag");
        assert_eq!(k.verify("abc", "primary", Some(&tag)), TagVerdict::Valid);
        assert_eq!(k.verify("abd", "primary", Some(&tag)), TagVerdict::Invalid);
        assert_eq!(k.verify("abc", "backdrop", Some(&tag)), TagVerdict::Invalid);
        assert_eq!(
            key(2).verify("abc", "primary", Some(&tag)),
            TagVerdict::Invalid
        );
    }

    #[test]
    fn id_and_role_are_compared_in_canonical_form() {
        // The kotlin SDK sends dashed ids (B85) and LowercasePath lowers
        // the role, so neither may change the verdict.
        let k = key(1);
        let id = "0123456789abcdef0123456789abcdef";
        let dashed = "01234567-89AB-CDEF-0123-456789ABCDEF";
        let tag = k.sign(id, "Primary", "x");
        assert_eq!(k.verify(dashed, "primary", Some(&tag)), TagVerdict::Valid);
        assert_eq!(
            k.verify(id, "primary", Some(&tag.to_ascii_uppercase())),
            TagVerdict::Invalid,
            "the inner tag is opaque, so only the MAC is case-folded"
        );
    }

    #[test]
    fn missing_short_and_unkeyed_tags_are_told_apart() {
        let k = key(1);
        assert_eq!(k.verify("a", "primary", None), TagVerdict::Absent);
        assert_eq!(k.verify("a", "primary", Some("")), TagVerdict::Absent);
        assert_eq!(k.verify("a", "primary", Some("abc")), TagVerdict::Invalid);
        // A pre-ADR-0073 tag: 16 hex of unkeyed hash.
        assert_eq!(
            k.verify("a", "primary", Some("7fa3c2b1d0e9f812")),
            TagVerdict::Invalid
        );
        assert_eq!(
            k.verify("a", "primary", Some("ab€cdefghijklmnopq")),
            TagVerdict::Invalid
        );
    }

    #[test]
    fn verdict_labels_are_distinct() {
        let labels =
            [TagVerdict::Valid, TagVerdict::Invalid, TagVerdict::Absent].map(TagVerdict::label);
        assert_eq!(labels, ["valid", "invalid", "absent"]);
    }

    #[test]
    fn the_key_never_prints() {
        assert_eq!(format!("{:?}", key(7)), "ImageTagKey(REDACTED)");
    }

    /// Every tag a client reads from a DTO must verify for the URL that
    /// client then builds from the paired id field. Walks a nested listing
    /// shaped like `/Items` with an episode carrying every parent tag.
    #[test]
    fn every_tag_in_a_listing_verifies_for_the_url_a_client_builds() {
        let k = key(3);
        let mut body = json!({
            "Items": [{
                "Id": "ep1",
                "ImageTags": {"Primary": "p", "Thumb": "t"},
                "BackdropImageTags": ["b0", "b1"],
                "SeriesId": "ser",
                "SeriesPrimaryImageTag": "sp",
                "SeriesThumbImageTag": "st",
                "ParentBackdropItemId": "pb",
                "ParentBackdropImageTags": ["pb0"],
                "ParentThumbItemId": "pt", "ParentThumbImageTag": "ptt",
                "ParentLogoItemId": "pl", "ParentLogoImageTag": "plt",
                "ParentArtItemId": "pa", "ParentArtImageTag": "pat",
                "ParentPrimaryImageItemId": "pp", "ParentPrimaryImageTag": "ppt",
                "AlbumId": "alb", "AlbumPrimaryImageTag": "at",
                "ChannelId": "ch", "ChannelPrimaryImageTag": "ct",
                "Chapters": [{"ImageTag": "c0"}, {"ImageTag": "c1"}],
                "People": [{"Id": "per", "PrimaryImageTag": "pt0"}],
            }],
            "TotalRecordCount": 1,
        });
        let n = sign_image_tags(&k, &mut body);
        let it = &body["Items"][0];
        let check = |id: &str, role: &str, v: &Value| {
            assert_eq!(
                k.verify(id, role, v.as_str()),
                TagVerdict::Valid,
                "{id}/{role}: {v}"
            );
        };
        check("ep1", "primary", &it["ImageTags"]["Primary"]);
        check("ep1", "thumb", &it["ImageTags"]["Thumb"]);
        check("ep1", "backdrop", &it["BackdropImageTags"][0]);
        check("ep1", "backdrop/1", &it["BackdropImageTags"][1]);
        check("ser", "primary", &it["SeriesPrimaryImageTag"]);
        check("ser", "thumb", &it["SeriesThumbImageTag"]);
        check("pb", "backdrop", &it["ParentBackdropImageTags"][0]);
        check("pt", "thumb", &it["ParentThumbImageTag"]);
        check("pl", "logo", &it["ParentLogoImageTag"]);
        check("pa", "art", &it["ParentArtImageTag"]);
        check("pp", "primary", &it["ParentPrimaryImageTag"]);
        check("alb", "primary", &it["AlbumPrimaryImageTag"]);
        check("ch", "primary", &it["ChannelPrimaryImageTag"]);
        check("ep1", "chapter", &it["Chapters"][0]["ImageTag"]);
        check("ep1", "chapter/1", &it["Chapters"][1]["ImageTag"]);
        assert_eq!(
            k.verify("ep1", "chapter/1", it["Chapters"][0]["ImageTag"].as_str()),
            TagVerdict::Invalid,
            "chapter 0's tag must not authorise chapter 1"
        );
        assert_eq!(
            it["People"][0]["PrimaryImageTag"], "pt0",
            "portraits stay unsigned"
        );
        assert_eq!(n, 15);
    }

    #[test]
    fn a_tag_without_its_id_field_is_left_alone() {
        let k = key(3);
        let mut body = json!({"SeriesPrimaryImageTag": "sp", "ImageTags": {"Primary": "p"}});
        assert_eq!(sign_image_tags(&k, &mut body), 0);
        assert_eq!(body["SeriesPrimaryImageTag"], "sp");
    }
}
