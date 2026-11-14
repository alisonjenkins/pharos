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

use crate::metadata_language::DescriptionLanguage;
use crate::services::user_config::{
    get_string_field, set_string_field, string_field, SetFieldError,
};
use crate::state::AppState;
use pharos_core::{
    EditableField, LocalizedText, MediaId, MediaItem, MediaKind, MetadataTranslationStore,
    PreferenceStore, SeriesMetadata, TranslationOwner, UserId,
};

const FIELD: &str = "DescriptionLanguagePreference";

/// The description language `user` picked. A missing or unreadable
/// configuration is the default: a corrupt blob must not hide a page.
pub async fn description_language<S: PreferenceStore>(
    store: &S,
    user: UserId,
) -> DescriptionLanguage {
    match store.get_user_configuration(user).await {
        Ok(Some(raw)) => match string_field(&raw, FIELD) {
            Ok(p) => DescriptionLanguage::parse(&p),
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

/// The stored description-language setting in the form the settings pages
/// offer, empty when unset. The Jellyfin configuration save stores the field
/// unchecked, so an alias written there (`fr`, `fre`) is returned as the
/// selector's value (`fra`); a value naming no language is returned as is.
pub async fn language_setting<S: PreferenceStore>(
    store: &S,
    user: UserId,
) -> Result<String, String> {
    let p = get_string_field(store, user, FIELD).await?;
    Ok(canonical_setting(&p).unwrap_or(p))
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
) -> Result<(), SetFieldError> {
    let value = canonical_setting(value).ok_or(SetFieldError::Invalid)?;
    set_string_field(store, user, FIELD, &value).await
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
        .filter(|i| i.kind == MediaKind::Episode && !i.locked.contains(EditableField::Overview))
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
    // An admin's own text is shown in every language (ADR-0097).
    let owners = metas
        .iter()
        .filter(|(_, m)| !m.locked.contains(EditableField::Overview))
        .map(|(k, _)| TranslationOwner::Series(k.clone()))
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

/// Whether `item` can have a provider's description in another language: an
/// episode, but not a course lesson, whose rows a film match may have left
/// behind (ADR-0126).
fn has_provider_text(item: &MediaItem) -> bool {
    item.kind == MediaKind::Episode
        && item.match_source.as_deref() != Some(pharos_scanner::course::COURSE_MATCH_SOURCE)
}

/// [`localize_items_in`] for `user`'s language.
pub async fn localize_items<'a>(
    state: &AppState,
    user: UserId,
    items: impl IntoIterator<Item = &'a mut MediaItem>,
) {
    let episodes: Vec<&mut MediaItem> =
        items.into_iter().filter(|i| has_provider_text(i)).collect();
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
    let episodes: Vec<&MediaItem> = items.into_iter().filter(|i| has_provider_text(i)).collect();
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

    /// V1392 — a course lesson shows its own text, never a provider's.
    #[test]
    fn a_course_lesson_has_no_provider_text() {
        let episode = MediaItem {
            kind: MediaKind::Episode,
            ..Default::default()
        };
        assert!(super::has_provider_text(&episode));
        let lesson = MediaItem {
            match_source: Some("course".into()),
            ..episode
        };
        assert!(!super::has_provider_text(&lesson));
    }
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
            &TranslationOwner::Series("/tv/Silver Rock".into()),
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
            "/tv/Silver Rock".to_string(),
            SeriesMetadata {
                series_key: "/tv/Silver Rock".into(),
                overview: Some("Goku sets out.".into()),
                ..Default::default()
            },
        )]);
        localize_series_in(s, &lang, &mut metas).await;
        (
            items[0].metadata.overview.clone().unwrap(),
            metas["/tv/Silver Rock"].overview.clone().unwrap(),
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
    async fn an_admin_edited_description_is_shown_in_every_language() {
        let s = seeded().await;
        let lang = DescriptionLanguage::parse("jpn");
        let locked: pharos_core::LockedFields = [EditableField::Overview].into_iter().collect();
        let mut items = [MediaItem {
            locked,
            ..episode(1, "Admin episode text.")
        }];
        localize_items_in(&s, &lang, items.iter_mut()).await;
        assert_eq!(
            items[0].metadata.overview.as_deref(),
            Some("Admin episode text.")
        );
        let mut metas = HashMap::from([(
            "/tv/Silver Rock".to_string(),
            SeriesMetadata {
                series_key: "/tv/Silver Rock".into(),
                overview: Some("Admin show text.".into()),
                locked,
                ..Default::default()
            },
        )]);
        localize_series_in(&s, &lang, &mut metas).await;
        assert_eq!(
            metas["/tv/Silver Rock"].overview.as_deref(),
            Some("Admin show text.")
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
        // A field the typed configuration would reject does not lose a valid
        // language setting.
        s.set_user_configuration(
            uid,
            r#"{"DescriptionLanguagePreference":"fra","OrderedViews":null}"#,
        )
        .await
        .unwrap();
        assert_eq!(language_setting(&s, uid).await.unwrap(), "fra");
        assert!(matches!(
            description_language(&s, uid).await,
            DescriptionLanguage::Code(c) if c == "fra"
        ));
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
            Err(SetFieldError::Invalid)
        ));
        s.set_user_configuration(uid, "[1,2]").await.unwrap();
        assert!(matches!(
            set_language_setting(&s, uid, "fra").await,
            Err(SetFieldError::Unreadable)
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
