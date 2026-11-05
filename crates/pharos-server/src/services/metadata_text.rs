//! Show and episode descriptions in the language each user picked
//! (ADR-0062), resolved once here for the Jellyfin and native APIs (V172).
//!
//! The stored `overview` columns already hold the default text: English,
//! else the provider's original. A user on the default (unset or English)
//! costs one configuration read per page with shows or episodes on it, and
//! no translation lookup. Anyone else gets, per show or episode, the stored
//! text in their language, else that default; "original" reads the
//! provider's base-record text.

use std::collections::HashMap;

use pharos_core::{
    LocalizedText, MediaId, MediaItem, MediaKind, MetadataTranslationStore, PreferenceStore,
    SeriesMetadata, TranslationOwner, UserId,
};
use pharos_jellyfin_api::dto::UserConfigurationDto;

use crate::metadata_language::DescriptionLanguage;
use crate::state::AppState;

/// The description language `user` picked. A missing or unreadable
/// configuration is the default: a corrupt blob must not hide a page.
pub async fn description_language<S: PreferenceStore>(
    store: &S,
    user: UserId,
) -> DescriptionLanguage {
    match store.get_user_configuration(user).await {
        Ok(Some(raw)) => match serde_json::from_str::<UserConfigurationDto>(&raw) {
            Ok(c) => DescriptionLanguage::parse(&c.description_language_preference),
            Err(e) => {
                tracing::warn!(user.id = %user, error = %e, "user configuration does not parse; descriptions use the default language");
                DescriptionLanguage::Default
            }
        },
        Ok(None) => DescriptionLanguage::Default,
        Err(e) => {
            tracing::warn!(user.id = %user, error = %e, "could not read user configuration; descriptions use the default language");
            DescriptionLanguage::Default
        }
    }
}

/// Why a description-language setting was not stored.
#[derive(Debug)]
pub enum SetLanguageError {
    /// Not a language code, `OriginalLanguage` or empty.
    Invalid,
    /// The stored configuration is not a JSON object, so setting one field
    /// in it would throw the rest away.
    Unreadable,
    /// The configuration could not be read or written.
    Store(String),
}

/// The stored description-language setting in the form the settings pages
/// offer, empty when unset. The Jellyfin configuration save stores the field
/// unchecked, so an alias written there (`fr`, `fre`) is returned as the
/// selector's value (`fra`); a value naming no language is returned as is.
pub async fn language_setting<S: PreferenceStore>(
    store: &S,
    user: UserId,
) -> Result<String, String> {
    let raw = store
        .get_user_configuration(user)
        .await
        .map_err(|e| e.to_string())?;
    Ok(raw
        .and_then(|r| serde_json::from_str::<UserConfigurationDto>(&r).ok())
        .map(|c| {
            canonical_setting(&c.description_language_preference)
                .unwrap_or(c.description_language_preference)
        })
        .unwrap_or_default())
}

/// Store `value` as `user`'s description language, in their Jellyfin
/// configuration (`DescriptionLanguagePreference`) so jellyfin-web and the
/// native UI read and write the same setting. Every other field of the
/// stored configuration, known or not, is kept as it was; a stored value
/// that is not a JSON object is refused rather than overwritten. The value
/// is stored in the form the settings pages offer: `fr` becomes `fra`,
/// English becomes empty.
pub async fn set_language_setting<S: PreferenceStore>(
    store: &S,
    user: UserId,
    value: &str,
) -> Result<(), SetLanguageError> {
    let value = canonical_setting(value).ok_or(SetLanguageError::Invalid)?;
    let raw = store
        .get_user_configuration(user)
        .await
        .map_err(|e| SetLanguageError::Store(e.to_string()))?;
    let mut cfg = match raw {
        None => serde_json::json!({}),
        Some(r) => match serde_json::from_str::<serde_json::Value>(&r) {
            Ok(v) if v.is_object() => v,
            _ => {
                tracing::warn!(user.id = %user, size = r.len(), "description language not set: stored configuration is not a JSON object");
                return Err(SetLanguageError::Unreadable);
            }
        },
    };
    cfg["DescriptionLanguagePreference"] = value.as_str().into();
    store
        .set_user_configuration(user, &cfg.to_string())
        .await
        .map_err(|e| SetLanguageError::Store(e.to_string()))?;
    tracing::info!(user.id = %user, description_language = value, "description language set");
    Ok(())
}

/// `raw` as the settings pages offer it, or `None` when it names no
/// language.
fn canonical_setting(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Some(String::new());
    }
    match DescriptionLanguage::parse(raw) {
        DescriptionLanguage::Original => Some(crate::metadata_language::ORIGINAL.to_string()),
        DescriptionLanguage::Code(code) => Some(code),
        DescriptionLanguage::Default => crate::metadata_language::normalize(raw)
            .filter(|c| c == crate::metadata_language::DEFAULT_LANGUAGE)
            .map(|_| String::new()),
    }
}

/// The text to show for one owner from its stored rows, `None` to keep the
/// default column.
fn pick<'a>(lang: &DescriptionLanguage, rows: &'a [LocalizedText]) -> Option<&'a str> {
    let row = match lang {
        DescriptionLanguage::Default => None,
        DescriptionLanguage::Code(code) => rows.iter().find(|r| &r.language == code),
        DescriptionLanguage::Original => rows.iter().find(|r| r.is_original),
    };
    row.and_then(|r| r.overview.as_deref())
}

/// Rows for `owners` in `lang`, grouped by owner. Empty for the default.
async fn rows_for<S: MetadataTranslationStore>(
    store: &S,
    lang: &DescriptionLanguage,
    owners: Vec<TranslationOwner>,
) -> HashMap<TranslationOwner, Vec<LocalizedText>> {
    let (languages, with_original) = match lang {
        DescriptionLanguage::Default => return HashMap::new(),
        DescriptionLanguage::Code(code) => (vec![code.clone()], false),
        DescriptionLanguage::Original => (Vec::new(), true),
    };
    if owners.is_empty() {
        return HashMap::new();
    }
    match store
        .translations_for(&owners, &languages, with_original)
        .await
    {
        Ok(rows) => {
            let mut by_owner: HashMap<TranslationOwner, Vec<LocalizedText>> = HashMap::new();
            for (owner, text) in rows {
                by_owner.entry(owner).or_default().push(text);
            }
            by_owner
        }
        Err(e) => {
            // Symmetry with the success path: say which language fell back.
            tracing::warn!(error = %e, language = ?lang, "could not read translated descriptions; showing the default text");
            HashMap::new()
        }
    }
}

/// Each episode's stored description in `lang`, by item id; episodes with
/// none (and every other kind) are absent and keep their default text.
pub async fn episode_overviews_in<'a, S: MetadataTranslationStore>(
    store: &S,
    lang: &DescriptionLanguage,
    items: impl IntoIterator<Item = &'a MediaItem>,
) -> HashMap<MediaId, String> {
    if *lang == DescriptionLanguage::Default {
        return HashMap::new();
    }
    let owners = items
        .into_iter()
        .filter(|i| i.kind == MediaKind::Episode)
        .map(|i| TranslationOwner::Item(i.id))
        .collect();
    rows_for(store, lang, owners)
        .await
        .into_iter()
        .filter_map(|(owner, rows)| match owner {
            TranslationOwner::Item(id) => Some((id, pick(lang, &rows)?.to_string())),
            TranslationOwner::Series(_) => None,
        })
        .collect()
}

/// Put each episode's description in `lang` where one is stored.
pub async fn localize_items_in<'a, S: MetadataTranslationStore>(
    store: &S,
    lang: &DescriptionLanguage,
    items: impl IntoIterator<Item = &'a mut MediaItem>,
) {
    let mut items: Vec<&mut MediaItem> = items.into_iter().collect();
    let mut overviews = episode_overviews_in(store, lang, items.iter().map(|i| &**i)).await;
    for item in &mut items {
        if let Some(text) = overviews.remove(&item.id) {
            item.metadata.overview = Some(text);
        }
    }
}

/// Put each show's description in `lang` where one is stored.
pub async fn localize_series_in<S: MetadataTranslationStore>(
    store: &S,
    lang: &DescriptionLanguage,
    metas: &mut HashMap<String, SeriesMetadata>,
) {
    if *lang == DescriptionLanguage::Default {
        return;
    }
    let owners = metas
        .keys()
        .map(|k| TranslationOwner::Series(k.clone()))
        .collect();
    let rows = rows_for(store, lang, owners).await;
    for (key, meta) in metas.iter_mut() {
        if let Some(text) = rows
            .get(&TranslationOwner::Series(key.clone()))
            .and_then(|r| pick(lang, r))
        {
            meta.overview = Some(text.to_string());
        }
    }
}

/// [`localize_items_in`] for `user`'s language.
pub async fn localize_items<'a>(
    state: &AppState,
    user: UserId,
    items: impl IntoIterator<Item = &'a mut MediaItem>,
) {
    let episodes: Vec<&mut MediaItem> = items
        .into_iter()
        .filter(|i| i.kind == MediaKind::Episode)
        .collect();
    if episodes.is_empty() {
        return;
    }
    let lang = description_language(&state.stores, user).await;
    localize_items_in(&state.stores, &lang, episodes).await;
}

/// [`episode_overviews_in`] for `user`'s language, for a page that renders
/// from borrowed items.
pub async fn episode_overviews<'a>(
    state: &AppState,
    user: UserId,
    items: impl IntoIterator<Item = &'a MediaItem>,
) -> HashMap<MediaId, String> {
    let episodes: Vec<&MediaItem> = items
        .into_iter()
        .filter(|i| i.kind == MediaKind::Episode)
        .collect();
    if episodes.is_empty() {
        return HashMap::new();
    }
    let lang = description_language(&state.stores, user).await;
    episode_overviews_in(&state.stores, &lang, episodes).await
}

/// [`localize_series_in`] for `user`'s language.
pub async fn localize_series(
    state: &AppState,
    user: UserId,
    metas: &mut HashMap<String, SeriesMetadata>,
) {
    if metas.is_empty() {
        return;
    }
    let lang = description_language(&state.stores, user).await;
    localize_series_in(&state.stores, &lang, metas).await;
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use pharos_store_sqlx::sqlite::SqliteStore;

    async fn store() -> SqliteStore {
        SqliteStore::connect("sqlite::memory:").await.unwrap()
    }

    fn row(lang: &str, overview: &str, original: bool) -> LocalizedText {
        LocalizedText {
            language: lang.into(),
            title: None,
            overview: Some(overview.into()),
            is_original: original,
        }
    }

    fn episode(id: u64, overview: &str) -> MediaItem {
        MediaItem {
            id,
            kind: MediaKind::Episode,
            metadata: pharos_core::MediaMetadata {
                overview: Some(overview.into()),
                ..Default::default()
            },
            ..MediaItem::default()
        }
    }

    async fn seeded() -> SqliteStore {
        let s = store().await;
        s.upsert_translations(
            &TranslationOwner::Item(1),
            &[
                row("jpn", "ブルマが…", true),
                row("eng", "Bulma meets Goku.", false),
                LocalizedText {
                    language: "fra".into(),
                    ..Default::default()
                },
            ],
            1,
        )
        .await
        .unwrap();
        s.upsert_translations(
            &TranslationOwner::Series("/tv/Dragon Ball".into()),
            &[
                row("jpn", "悟空の冒険。", true),
                row("deu", "Goku zieht los.", false),
            ],
            1,
        )
        .await
        .unwrap();
        s
    }

    async fn shown(s: &SqliteStore, setting: &str) -> (String, String) {
        let lang = DescriptionLanguage::parse(setting);
        let mut items = [episode(1, "Bulma meets Goku.")];
        localize_items_in(s, &lang, items.iter_mut()).await;
        let mut metas = HashMap::from([(
            "/tv/Dragon Ball".to_string(),
            SeriesMetadata {
                series_key: "/tv/Dragon Ball".into(),
                overview: Some("Goku sets out.".into()),
                ..Default::default()
            },
        )]);
        localize_series_in(s, &lang, &mut metas).await;
        (
            items[0].metadata.overview.clone().unwrap(),
            metas["/tv/Dragon Ball"].overview.clone().unwrap(),
        )
    }

    #[tokio::test]
    async fn each_language_resolves_with_the_default_as_fallback() {
        let s = seeded().await;
        assert_eq!(
            shown(&s, "").await,
            ("Bulma meets Goku.".into(), "Goku sets out.".into())
        );
        assert_eq!(
            shown(&s, "jpn").await,
            ("ブルマが…".into(), "悟空の冒険。".into())
        );
        assert_eq!(
            shown(&s, "deu").await,
            ("Bulma meets Goku.".into(), "Goku zieht los.".into()),
            "no German episode text: the default"
        );
        assert_eq!(
            shown(&s, "fra").await,
            ("Bulma meets Goku.".into(), "Goku sets out.".into()),
            "a recorded miss is not text"
        );
        assert_eq!(
            shown(&s, "OriginalLanguage").await,
            ("ブルマが…".into(), "悟空の冒険。".into())
        );
    }

    #[tokio::test]
    async fn setting_the_language_keeps_the_rest_of_the_configuration() {
        use pharos_core::{SecretString, UserPolicy, UserRecord, UserStore};
        let s = store().await;
        let uid = UserId::new();
        UserStore::create(
            &s,
            UserRecord {
                id: uid,
                name: "mika".into(),
                password_hash: SecretString::new("$argon2id$fake"),
                policy: UserPolicy::default(),
            },
        )
        .await
        .unwrap();
        assert_eq!(language_setting(&s, uid).await.unwrap(), "");
        s.set_user_configuration(uid, r#"{"AudioLanguagePreference":"jpn","SomethingNew":1}"#)
            .await
            .unwrap();
        set_language_setting(&s, uid, "fra").await.unwrap();
        let raw: serde_json::Value =
            serde_json::from_str(&s.get_user_configuration(uid).await.unwrap().unwrap()).unwrap();
        assert_eq!(raw["AudioLanguagePreference"], "jpn");
        assert_eq!(raw["SomethingNew"], 1);
        assert_eq!(language_setting(&s, uid).await.unwrap(), "fra");
        set_language_setting(&s, uid, "ja").await.unwrap();
        assert_eq!(language_setting(&s, uid).await.unwrap(), "jpn");
        set_language_setting(&s, uid, "en").await.unwrap();
        assert_eq!(language_setting(&s, uid).await.unwrap(), "");
        // A Jellyfin client stores the field unchecked; it reads back as
        // the selector's value.
        s.set_user_configuration(uid, r#"{"DescriptionLanguagePreference":"fr"}"#)
            .await
            .unwrap();
        assert_eq!(language_setting(&s, uid).await.unwrap(), "fra");
        set_language_setting(&s, uid, "OriginalLanguage")
            .await
            .unwrap();
        assert_eq!(
            description_language(&s, uid).await,
            DescriptionLanguage::Original
        );
        set_language_setting(&s, uid, "").await.unwrap();
        assert_eq!(
            description_language(&s, uid).await,
            DescriptionLanguage::Default
        );
        assert!(matches!(
            set_language_setting(&s, uid, "not a language").await,
            Err(SetLanguageError::Invalid)
        ));
        s.set_user_configuration(uid, "[1,2]").await.unwrap();
        assert!(matches!(
            set_language_setting(&s, uid, "fra").await,
            Err(SetLanguageError::Unreadable)
        ));
        assert_eq!(
            s.get_user_configuration(uid).await.unwrap().unwrap(),
            "[1,2]"
        );
    }

    #[tokio::test]
    async fn movies_are_left_alone() {
        let s = seeded().await;
        let mut movie = episode(1, "A film.");
        movie.kind = MediaKind::Movie;
        let mut items = [movie];
        localize_items_in(
            &s,
            &DescriptionLanguage::Code("jpn".into()),
            items.iter_mut(),
        )
        .await;
        assert_eq!(items[0].metadata.overview.as_deref(), Some("A film."));
    }

    #[tokio::test]
    async fn the_setting_is_read_from_the_user_configuration() {
        use pharos_core::{SecretString, UserPolicy, UserRecord, UserStore};
        let s = store().await;
        let uid = UserId::new();
        UserStore::create(
            &s,
            UserRecord {
                id: uid,
                name: "kenji".into(),
                password_hash: SecretString::new("$argon2id$fake"),
                policy: UserPolicy::default(),
            },
        )
        .await
        .unwrap();
        assert_eq!(
            description_language(&s, uid).await,
            DescriptionLanguage::Default
        );
        s.set_user_configuration(uid, r#"{"DescriptionLanguagePreference":"ja"}"#)
            .await
            .unwrap();
        assert_eq!(
            description_language(&s, uid).await,
            DescriptionLanguage::Code("jpn".into())
        );
        s.set_user_configuration(uid, "not json").await.unwrap();
        assert_eq!(
            description_language(&s, uid).await,
            DescriptionLanguage::Default
        );
    }
}
