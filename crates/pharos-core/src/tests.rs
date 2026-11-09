#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

#[derive(Default)]
struct MemStore {
    inner: Mutex<HashMap<MediaId, MediaItem>>,
    states: Mutex<HashMap<MediaId, ScanState>>,
    fps: Mutex<HashMap<MediaId, Fingerprint>>,
    next_scan_id: std::sync::atomic::AtomicI64,
    artwork: Mutex<HashMap<(MediaId, String), (String, String)>>,
    fail_put_ids: Mutex<std::collections::HashSet<MediaId>>,
}

impl MediaStore for MemStore {
    async fn get(&self, id: MediaId) -> DomainResult<MediaItem> {
        self.inner
            .lock()
            .map_err(|e| DomainError::Backend(e.to_string()))?
            .get(&id)
            .cloned()
            .ok_or(DomainError::NotFound(id))
    }
    async fn put(&self, item: MediaItem) -> DomainResult<()> {
        if self
            .fail_put_ids
            .lock()
            .map_err(|e| DomainError::Backend(e.to_string()))?
            .contains(&item.id)
        {
            return Err(DomainError::Backend(format!("put {} refused", item.id)));
        }
        self.inner
            .lock()
            .map_err(|e| DomainError::Backend(e.to_string()))?
            .insert(item.id, item);
        Ok(())
    }
    async fn list(&self) -> DomainResult<Vec<MediaItem>> {
        Ok(self
            .inner
            .lock()
            .map_err(|e| DomainError::Backend(e.to_string()))?
            .values()
            .cloned()
            .collect())
    }

    async fn query(&self, q: &MediaQuery) -> DomainResult<(Vec<MediaItem>, u64)> {
        // In-memory test store: no entity-join tables, so the entity /
        // parent / user-data pivots aren't modelled here (the SQL backends
        // carry those). Honours the kind / search / sort / page surface the
        // core round-trip tests exercise.
        let mut items: Vec<MediaItem> = self
            .inner
            .lock()
            .map_err(|e| DomainError::Backend(e.to_string()))?
            .values()
            .filter(|i| q.kinds.is_empty() || q.kinds.contains(&i.kind))
            .filter(|i| match q.search_term.as_deref() {
                Some(t) if !t.trim().is_empty() => {
                    i.title.to_lowercase().contains(&t.trim().to_lowercase())
                }
                _ => true,
            })
            .cloned()
            .collect();
        let key = q.sort.first().map(|(k, _)| *k).unwrap_or(SortKey::Id);
        let desc = matches!(q.sort.first().map(|(_, d)| *d), Some(SortDir::Desc));
        match key {
            SortKey::Name => items.sort_by(|a, b| {
                a.title
                    .to_lowercase()
                    .cmp(&b.title.to_lowercase())
                    .then(a.id.cmp(&b.id))
            }),
            SortKey::DateCreated => {
                items.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)))
            }
            _ => items.sort_by_key(|i| i.id),
        }
        if desc {
            items.reverse();
        }
        let total = items.len() as u64;
        let start = usize::try_from(q.start_index).unwrap_or(usize::MAX);
        let mut page: Vec<MediaItem> = items.into_iter().skip(start).collect();
        if let Some(limit) = q.limit {
            page.truncate(limit as usize);
        }
        Ok((page, total))
    }

    async fn search(&self, q: &SearchQuery) -> DomainResult<(Vec<MediaItem>, u64)> {
        // In-memory FTS analogue: token-prefix match on title+overview OR a
        // whole-term substring (the SUPERSET arm), honouring the kind filter.
        let tokens = search_tokens(&q.term);
        if tokens.is_empty() {
            return Ok((Vec::new(), 0));
        }
        let needle = q.term.trim().to_lowercase();
        let mut hits: Vec<MediaItem> = self
            .inner
            .lock()
            .map_err(|e| DomainError::Backend(e.to_string()))?
            .values()
            .filter(|i| q.kinds.is_empty() || q.kinds.contains(&i.kind))
            .filter(|i| {
                let title = i.title.to_lowercase();
                let overview = i.metadata.overview.as_deref().unwrap_or("").to_lowercase();
                let prefix_hit = tokens.iter().all(|tok| {
                    title
                        .split(|c: char| !c.is_alphanumeric())
                        .any(|w| w.starts_with(tok))
                        || overview
                            .split(|c: char| !c.is_alphanumeric())
                            .any(|w| w.starts_with(tok))
                });
                prefix_hit || title.contains(&needle) || overview.contains(&needle)
            })
            .cloned()
            .collect();
        hits.sort_by_key(|i| i.id);
        let total = hits.len() as u64;
        let start = usize::try_from(q.offset).unwrap_or(usize::MAX);
        let mut page: Vec<MediaItem> = hits.into_iter().skip(start).collect();
        page.truncate(q.limit.max(1) as usize);
        Ok((page, total))
    }

    async fn facets(&self, base: &MediaQuery, req: &FacetRequest) -> DomainResult<MediaFacets> {
        // In-memory store has no entity-join tables; only the scalar facets
        // (year / official_rating) are derivable. Honour the kind filter.
        let mut out = MediaFacets::default();
        let inner = self
            .inner
            .lock()
            .map_err(|e| DomainError::Backend(e.to_string()))?;
        let matched: Vec<&MediaItem> = inner
            .values()
            .filter(|i| base.kinds.is_empty() || base.kinds.contains(&i.kind))
            .collect();
        if req.years {
            let mut counts: std::collections::BTreeMap<u32, u32> =
                std::collections::BTreeMap::new();
            for i in &matched {
                if let Some(y) = i.metadata.production_year {
                    *counts.entry(y).or_default() += 1;
                }
            }
            out.years = counts
                .into_iter()
                .rev()
                .map(|(y, c)| FacetValue {
                    value: y.to_string(),
                    wire_id: y.to_string(),
                    count: c,
                })
                .collect();
        }
        if req.official_ratings {
            let mut counts: HashMap<String, u32> = HashMap::new();
            for i in &matched {
                if let Some(r) = i
                    .metadata
                    .official_rating
                    .as_deref()
                    .filter(|s| !s.is_empty())
                {
                    *counts.entry(r.to_string()).or_default() += 1;
                }
            }
            let mut vals: Vec<FacetValue> = counts
                .into_iter()
                .map(|(value, count)| FacetValue {
                    wire_id: value.clone(),
                    value,
                    count,
                })
                .collect();
            vals.sort_by(|a, b| b.count.cmp(&a.count).then(a.value.cmp(&b.value)));
            out.official_ratings = vals;
        }
        Ok(out)
    }

    async fn id_at_path(&self, path: &Path) -> DomainResult<Option<MediaId>> {
        Ok(self
            .inner
            .lock()
            .map_err(|e| DomainError::Backend(e.to_string()))?
            .values()
            .find(|item| item.path == path)
            .map(|item| item.id))
    }
    async fn scan_state(&self, id: MediaId) -> DomainResult<Option<ScanState>> {
        Ok(self
            .states
            .lock()
            .map_err(|e| DomainError::Backend(e.to_string()))?
            .get(&id)
            .copied())
    }

    async fn begin_scan(&self, _root: &Path) -> DomainResult<i64> {
        Ok(self
            .next_scan_id
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1)
    }

    async fn mark_seen(
        &self,
        id: MediaId,
        scan_id: i64,
        mtime: i64,
        size: u64,
    ) -> DomainResult<()> {
        self.states
            .lock()
            .map_err(|e| DomainError::Backend(e.to_string()))?
            .insert(
                id,
                ScanState {
                    last_scanned: 0,
                    file_mtime: mtime,
                    file_size: size,
                    last_seen_scan_id: scan_id,
                    probe_schema_version: crate::PROBE_SCHEMA_VERSION,
                    has_editions: false,
                },
            );
        Ok(())
    }

    async fn sweep_unseen(&self, scan_id: i64, root_prefix: &str) -> DomainResult<Vec<MediaId>> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|e| DomainError::Backend(e.to_string()))?;
        let states = self
            .states
            .lock()
            .map_err(|e| DomainError::Backend(e.to_string()))?;
        let doomed: Vec<MediaId> = inner
            .iter()
            .filter(|(id, item)| {
                item.path.to_string_lossy().starts_with(root_prefix)
                    && states.get(*id).map(|s| s.last_seen_scan_id) != Some(scan_id)
            })
            .map(|(id, _)| *id)
            .collect();
        for id in &doomed {
            inner.remove(id);
        }
        Ok(doomed)
    }

    async fn finish_scan(
        &self,
        _scan_id: i64,
        _items_seen: i64,
        _items_swept: i64,
    ) -> DomainResult<()> {
        Ok(())
    }

    async fn find_by_fp(&self, fp: Fingerprint) -> DomainResult<Option<MediaItem>> {
        let fps = self
            .fps
            .lock()
            .map_err(|e| DomainError::Backend(e.to_string()))?;
        let inner = self
            .inner
            .lock()
            .map_err(|e| DomainError::Backend(e.to_string()))?;
        // First match by ascending id, mirroring the store's ORDER BY id.
        let mut matches: Vec<MediaId> = fps
            .iter()
            .filter(|(_, v)| **v == fp)
            .map(|(id, _)| *id)
            .collect();
        matches.sort_unstable();
        Ok(matches.into_iter().find_map(|id| inner.get(&id).cloned()))
    }

    async fn set_fingerprint(&self, id: MediaId, fp: Fingerprint) -> DomainResult<()> {
        // No-op when the row is absent, mirroring mark_seen.
        if self
            .inner
            .lock()
            .map_err(|e| DomainError::Backend(e.to_string()))?
            .contains_key(&id)
        {
            self.fps
                .lock()
                .map_err(|e| DomainError::Backend(e.to_string()))?
                .insert(id, fp);
        }
        Ok(())
    }

    async fn rebind_path(&self, id: MediaId, new_path: &Path) -> DomainResult<()> {
        // UPDATE-only: no-op when the row is absent.
        if let Some(item) = self
            .inner
            .lock()
            .map_err(|e| DomainError::Backend(e.to_string()))?
            .get_mut(&id)
        {
            item.path = new_path.to_path_buf();
        }
        Ok(())
    }

    async fn set_artwork(
        &self,
        item_id: MediaId,
        role: &str,
        source: &str,
        locator: &str,
    ) -> DomainResult<()> {
        self.artwork
            .lock()
            .map_err(|e| DomainError::Backend(e.to_string()))?
            .insert(
                (item_id, role.to_string()),
                (source.to_string(), locator.to_string()),
            );
        Ok(())
    }

    async fn bump_art_version(&self, item_id: MediaId) -> DomainResult<()> {
        if let Some(it) = self
            .inner
            .lock()
            .map_err(|e| DomainError::Backend(e.to_string()))?
            .get_mut(&item_id)
        {
            it.art_version += 1;
        }
        Ok(())
    }

    async fn artwork_for(&self, item_id: MediaId) -> DomainResult<Vec<(String, String, String)>> {
        let map = self
            .artwork
            .lock()
            .map_err(|e| DomainError::Backend(e.to_string()))?;
        let mut out: Vec<(String, String, String)> = map
            .iter()
            .filter(|((iid, _), _)| *iid == item_id)
            .map(|((_, role), (source, locator))| (role.clone(), source.clone(), locator.clone()))
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(out)
    }

    async fn set_item_match(
        &self,
        item_id: MediaId,
        provider: &str,
        external_id: &str,
        source: &str,
        confidence: Option<f32>,
        refreshed_at: i64,
    ) -> DomainResult<()> {
        // No-op when the row is absent, mirroring set_fingerprint/rebind_path.
        if let Some(item) = self
            .inner
            .lock()
            .map_err(|e| DomainError::Backend(e.to_string()))?
            .get_mut(&item_id)
        {
            item.match_provider = Some(provider.to_string());
            item.match_external_id = Some(external_id.to_string());
            item.match_source = Some(source.to_string());
            item.match_confidence = confidence;
            item.metadata_refreshed_at = Some(refreshed_at);
        }
        Ok(())
    }

    async fn items_needing_match(
        &self,
        limit: i64,
        ttl_cutoff: i64,
    ) -> DomainResult<Vec<MediaItem>> {
        let inner = self
            .inner
            .lock()
            .map_err(|e| DomainError::Backend(e.to_string()))?;
        let mut matches: Vec<MediaItem> = inner
            .values()
            .filter(|item| {
                let source_eligible = matches!(
                    item.match_source.as_deref(),
                    None | Some("search") | Some("none")
                );
                let ttl_eligible = item
                    .metadata_refreshed_at
                    .is_none_or(|refreshed| refreshed < ttl_cutoff);
                let kind_eligible = matches!(item.kind, MediaKind::Movie | MediaKind::Episode);
                source_eligible && ttl_eligible && kind_eligible
            })
            .cloned()
            .collect();
        matches.sort_by_key(|item| item.id);
        matches.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
        Ok(matches)
    }

    async fn audio_items_needing_art(
        &self,
        limit: i64,
        ttl_cutoff: i64,
        current_miss_marker: &str,
    ) -> DomainResult<Vec<MediaItem>> {
        let inner = self
            .inner
            .lock()
            .map_err(|e| DomainError::Backend(e.to_string()))?;
        let mut matches: Vec<MediaItem> = inner
            .values()
            .filter(|item| {
                let source_eligible = matches!(
                    item.match_source.as_deref(),
                    None | Some("search") | Some("none")
                );
                let ttl_eligible = item
                    .metadata_refreshed_at
                    .is_none_or(|refreshed| refreshed < ttl_cutoff);
                // A miss stamped by an older query version is re-admitted
                // regardless of the TTL — see `audio_items_needing_art`.
                let stale_verdict = item.match_source.as_deref() == Some("none")
                    && item.match_external_id.as_deref() != Some(current_miss_marker);
                matches!(item.kind, MediaKind::Audio)
                    && !item.has_primary_art
                    && source_eligible
                    && (ttl_eligible || stale_verdict)
            })
            .cloned()
            .collect();
        matches.sort_by_key(|item| item.id);
        matches.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
        Ok(matches)
    }

    async fn clear_provider_artwork(&self, _provider: &str) -> DomainResult<u64> {
        // MemStore is a lightweight test-double with no artwork table; the
        // invalidation path is exercised against the real sqlx stores.
        Ok(0)
    }

    async fn item_entity_counts(&self, _item_id: MediaId) -> DomainResult<EntityCounts> {
        // MemStore is a lightweight scanner test-double with no genre/people/
        // studio join tables; the online-enrich fill-if-empty gate is exercised
        // against the real sqlx stores.
        Ok(EntityCounts::default())
    }
}

struct NoopScanner;

impl Scanner for NoopScanner {
    async fn scan(&self, _root: &Path) -> DomainResult<Vec<MediaItem>> {
        Ok(vec![])
    }
}

fn sample() -> MediaItem {
    MediaItem {
        id: 1,
        path: "/m/a.mkv".into(),
        title: "A".into(),
        kind: MediaKind::Movie,
        ..Default::default()
    }
}

#[test]
fn media_probe_run_time_ticks_converts_ms_to_jellyfin_ticks() {
    let p = MediaProbe {
        duration_ms: Some(2_500),
        ..Default::default()
    };
    assert_eq!(p.run_time_ticks(), Some(25_000_000));
}

#[test]
fn media_probe_frame_rate_round_trips_through_mille() {
    let p = MediaProbe {
        frame_rate_mille: Some(23_976),
        ..Default::default()
    };
    let fps = p.frame_rate_f32().expect("set");
    assert!((fps - 23.976).abs() < 0.001, "got {fps}");
}

#[tokio::test]
async fn store_put_then_get_roundtrip() {
    let s = MemStore::default();
    s.put(sample()).await.unwrap();
    let got = s.get(1).await.unwrap();
    assert_eq!(got, sample());
}

#[tokio::test]
async fn store_get_missing_is_not_found() {
    let s = MemStore::default();
    match s.get(99).await {
        Err(DomainError::NotFound(99)) => {}
        other => panic!("expected NotFound, got {other:?}"),
    }
}

fn titled(id: MediaId, title: &str) -> MediaItem {
    MediaItem {
        id,
        title: title.into(),
        ..sample()
    }
}

#[tokio::test]
async fn default_put_batch_writes_in_order_and_last_duplicate_wins() {
    let s = MemStore::default();
    s.put_batch(&[titled(1, "first"), titled(2, "two"), titled(1, "last")])
        .await
        .unwrap();
    assert_eq!(s.get(1).await.unwrap().title, "last");
    assert_eq!(s.get(2).await.unwrap().title, "two");
    s.put_batch(&[]).await.unwrap();
}

#[tokio::test]
async fn default_put_batch_writes_the_rest_and_reports_the_first_error() {
    let s = MemStore::default();
    s.fail_put_ids.lock().unwrap().extend([2, 4]);
    let err = s
        .put_batch(&[
            titled(1, "a"),
            titled(2, "b"),
            titled(3, "c"),
            titled(4, "d"),
        ])
        .await
        .unwrap_err();
    assert!(matches!(err, DomainError::Backend(m) if m == "put 2 refused"));
    assert!(s.get(1).await.is_ok() && s.get(3).await.is_ok());
    assert!(s.get(2).await.is_err() && s.get(4).await.is_err());
}

#[tokio::test]
async fn scanner_callable_via_generic_bound() {
    async fn drive<S: Scanner>(s: &S) -> Vec<MediaItem> {
        s.scan(Path::new("/")).await.unwrap()
    }
    assert!(drive(&NoopScanner).await.is_empty());
}

#[test]
fn match_best_prefers_exact_title_and_year() {
    let cands = vec![
        SearchCandidate {
            id: "1".into(),
            title: "The Thing".into(),
            year: Some(2011),
            aliases: Vec::new(),
        },
        SearchCandidate {
            id: "2".into(),
            title: "The Thing".into(),
            year: Some(1982),
            aliases: Vec::new(),
        },
    ];
    let m = match_best("The Thing", Some(1982), &cands, 0.7).unwrap();
    assert_eq!(m.id, "2");
    assert!(m.confidence > 0.9);
}

#[test]
fn match_best_rejects_below_threshold() {
    let cands = vec![SearchCandidate {
        id: "9".into(),
        title: "Completely Different".into(),
        year: None,
        aliases: Vec::new(),
    }];
    assert!(match_best("The Thing", Some(1982), &cands, 0.7).is_none());
}

#[test]
fn match_best_year_off_by_one_is_partial_not_zero() {
    let cands = vec![SearchCandidate {
        id: "3".into(),
        title: "Blade Runner".into(),
        year: Some(1983),
        aliases: Vec::new(),
    }];
    // title exact, year ±1 -> still above threshold
    assert!(match_best("Blade Runner", Some(1982), &cands, 0.7).is_some());
}

fn cand(id: &str, title: &str, year: Option<u32>, aliases: &[&str]) -> SearchCandidate {
    SearchCandidate {
        id: id.into(),
        title: title.into(),
        year,
        aliases: aliases.iter().map(|a| (*a).into()).collect(),
    }
}

/// B557: TVDB names an anime by its Japanese title and carries the English
/// one only as a translation. Scoring the title alone handed "Dragon Ball" to
/// the English-titled fan work "Dragon Ball R&R" on a prefix match.
#[test]
fn match_best_scores_a_candidate_by_its_best_alias() {
    let cands = vec![
        cand("374456", "Dragon Ball R&R", Some(2018), &[]),
        cand("76666", "ドラゴンボール", Some(1986), &["Dragon Ball"]),
    ];
    let m = match_best("Dragon Ball", None, &cands, 0.7).unwrap();
    assert_eq!(m.id, "76666");
}

#[test]
fn match_best_ranks_an_exact_name_over_a_prefix_match_with_a_better_year() {
    let cands = vec![
        cand("fan", "Dragon Ball Z Abridged", Some(1990), &[]),
        cand("real", "ドラゴンボールZ", Some(1989), &["Dragon Ball Z"]),
    ];
    let m = match_best("Dragon Ball Z", Some(1990), &cands, 0.7).unwrap();
    assert_eq!(m.id, "real");
}

/// From the live TVDB search for "One Piece": the 2023 live-action series
/// lists "One Piece" among its aliases, the anime as its English title.
#[test]
fn match_best_prefers_an_exact_title_over_an_exact_alias() {
    let cands = vec![
        cand(
            "392276",
            "ONE PIECE (2023)",
            Some(2023),
            &["ONE PIECE (2023)", "ONE PIECE (US)", "One Piece"],
        ),
        cand(
            "81797",
            "ワンピース",
            Some(1999),
            &["One Piece", "One Piece (1998)"],
        ),
    ];
    let m = match_best("One Piece", None, &cands, 0.7).unwrap();
    assert_eq!(m.id, "81797");
}

/// From the live TVDB search for "Code Geass": every entry extends the name,
/// and an alias of the 2024 sequel ("Code Geass: Dakkan no Z") is the
/// shortest, so it scored highest.
#[test]
fn match_best_a_franchise_name_goes_to_the_original() {
    let cands = vec![
        cand(
            "443905",
            "コードギアス 奪還のロゼ",
            Some(2024),
            &[
                "Code Geass: Rozé of the Recapture",
                "Code Geass: Dakkan no Z",
            ],
        ),
        cand(
            "79525",
            "コードギアス 反逆のルルーシュ",
            Some(2006),
            &["Code Geass: Lelouch of the Rebellion"],
        ),
        cand(
            "471756",
            "コードギアス 星追いのアスパル",
            None,
            &["Code Geass: Aspar the Star Chaser"],
        ),
    ];
    let m = match_best("Code Geass", None, &cands, 0.7).unwrap();
    assert_eq!(m.id, "79525");
}

#[test]
fn match_best_an_exact_name_under_the_floor_does_not_shut_out_a_prefix_match() {
    let cands = vec![
        cand("1963", "Doctor Who", Some(1963), &[]),
        cand("2005", "Doctor Who (2005)", Some(2005), &[]),
    ];
    let m = match_best("Doctor Who", Some(2005), &cands, 0.7).unwrap();
    assert_eq!(m.id, "2005");
}

#[test]
fn title_similarity_ignores_case_and_punctuation() {
    assert!(title_similarity("WALL·E", "wall e") > 0.85);
    assert!(title_similarity("Se7en", "Seven") < 0.9); // not falsely perfect
}

#[test]
fn title_similarity_tolerates_appended_subtitle() {
    // The local folder carries the bare show name; the provider's canonical
    // title appends a subtitle. The shared portion is a whole-token prefix, so
    // the match must clear the 0.7 floor rather than being penalised for the
    // length gap (was ~0.29 with pure Levenshtein).
    let s = title_similarity("Code Geass", "Code Geass: Lelouch of the Rebellion");
    assert!(s >= 0.9, "appended-subtitle score too low: {s}");
    assert!(match_best(
        "Code Geass",
        None,
        &[SearchCandidate {
            id: "cg".into(),
            title: "Code Geass: Lelouch of the Rebellion".into(),
            year: None,
            aliases: Vec::new(),
        }],
        0.7
    )
    .is_some());
}

#[test]
fn title_similarity_tolerates_country_qualifier() {
    // The local title decorates the name with a "(US)" qualifier the provider
    // does not carry; "us" is the appended tail. Symmetric with the subtitle
    // case — the shorter title (provider) is a whole-token prefix of the local.
    let s = title_similarity("Forever (US)", "Forever");
    assert!(s >= 0.9, "country-qualifier score too low: {s}");
    assert!(match_best(
        "Forever (US)",
        None,
        &[SearchCandidate {
            id: "fv".into(),
            title: "Forever".into(),
            year: None,
            aliases: Vec::new(),
        }],
        0.7
    )
    .is_some());
}

#[test]
fn title_similarity_short_prefix_does_not_blanket_match() {
    // A trivially short prefix must NOT count as a subtitle match, or every
    // "Code …" / "The …" show would collide. Guarded by a minimum shared
    // length; here the whole-string Levenshtein still governs and stays low.
    assert!(title_similarity("Code", "Code Geass: Lelouch of the Rebellion") < 0.7);
    assert!(title_similarity("24", "24 Hours in A&E") < 0.7);
}

#[test]
fn parse_ymd_epoch_and_known_dates() {
    assert_eq!(parse_ymd_to_unix("1970-01-01"), Some(0));
    assert_eq!(parse_ymd_to_unix("2021-10-01"), Some(1_633_046_400)); // 2021-10-01T00:00:00Z
    assert_eq!(parse_ymd_to_unix("2000-02-29"), Some(951_782_400)); // leap day
    assert_eq!(parse_ymd_to_unix("  2011-04-17  "), Some(1_302_998_400)); // trimmed
}

#[test]
fn parse_ymd_rejects_malformed() {
    assert_eq!(parse_ymd_to_unix(""), None);
    assert_eq!(parse_ymd_to_unix("2021"), None);
    assert_eq!(parse_ymd_to_unix("2021-13-01"), None);
    assert_eq!(parse_ymd_to_unix("not-a-date"), None);
    assert_eq!(parse_ymd_to_unix("2021-10-01-05"), None);
}

// ───────────────────────────── FrameRate ─────────────────────────────

#[test]
fn frame_rate_rejects_the_container_clock_and_other_implausible_rates() {
    // The exact defect this type exists to prevent: MPEG-TS `r_frame_rate`
    // is the 90 kHz container clock, which a bare scalar happily carried as
    // a "frame rate" and which silently degraded the segment grid.
    assert_eq!(FrameRate::from_mille(90_000_000), None);
    assert_eq!(FrameRate::from_mille(500_000), None);
    assert_eq!(FrameRate::from_mille(0), None);
    assert_eq!(FrameRate::new(90_000, 1), None);
    assert_eq!(FrameRate::new(24_000, 0), None);
    assert_eq!(FrameRate::new(0, 1001), None);
}

#[test]
fn frame_rate_recovers_exact_ntsc_rationals_from_the_lossy_scalar() {
    // 23.976 is a rounded decimal; the true rate is 24000/1001. Recovering
    // the rational is what keeps frame times from drifting over a long title.
    assert_eq!(
        FrameRate::from_mille(23_976).map(FrameRate::as_ratio),
        Some((24_000, 1001))
    );
    // Probes report the NTSC family with slightly different rounding.
    assert_eq!(
        FrameRate::from_mille(23_970).map(FrameRate::as_ratio),
        Some((24_000, 1001))
    );
    assert_eq!(
        FrameRate::from_mille(23_977).map(FrameRate::as_ratio),
        Some((24_000, 1001))
    );
    assert_eq!(
        FrameRate::from_mille(29_970).map(FrameRate::as_ratio),
        Some((30_000, 1001))
    );
    assert_eq!(
        FrameRate::from_mille(59_940).map(FrameRate::as_ratio),
        Some((60_000, 1001))
    );
    // Integer rates stay exact and reduced.
    assert_eq!(
        FrameRate::from_mille(25_000).map(FrameRate::as_ratio),
        Some((25, 1))
    );
    assert_eq!(
        FrameRate::from_mille(24_000).map(FrameRate::as_ratio),
        Some((24, 1))
    );
}

#[test]
fn frame_times_are_exact_and_consecutive_frames_are_one_frame_apart() {
    let r = FrameRate::from_mille(23_976).expect("valid ntsc rate");
    // Frame 144 of 24000/1001 is exactly 6.006 s — the value the decimal
    // 23.976 only approximates.
    assert!((r.secs_of_frame(144) - 6.006).abs() < 1e-9);
    // The load-bearing property: neighbouring frames differ by exactly one
    // frame duration, with no accumulation deep into a title.
    let fd = 1001.0 / 24000.0;
    for idx in [1_u64, 144, 14_386, 172_800] {
        let d = r.secs_of_frame(idx + 1) - r.secs_of_frame(idx);
        assert!((d - fd).abs() < 1e-9, "idx {idx}: {d} vs {fd}");
    }
}

#[test]
fn frame_index_at_round_trips_through_the_frame_grid() {
    let r = FrameRate::from_mille(23_976).expect("valid ntsc rate");
    for idx in [0_u64, 1, 143, 144, 100_000] {
        assert_eq!(r.frame_index_at(r.secs_of_frame(idx)), idx);
    }
    // Degenerate inputs clamp rather than panicking or wrapping.
    assert_eq!(r.frame_index_at(-5.0), 0);
    assert_eq!(r.frame_index_at(f64::NAN), 0);
}

// ---------------------------------------------------------------------------
// 004-books — the wire discriminators for the Book kind and the Books library.
//
// Ordering note (Constitution III / V11): a test that NAMES an enum variant
// cannot compile before the variant exists, so these follow their variants by
// necessity rather than by preference. What they do guard is the wire tokens,
// which are load-bearing and easy to change by accident — every one of them is
// read by a client, not by pharos.
// ---------------------------------------------------------------------------

#[test]
fn book_is_parsed_from_the_wire_discriminator() {
    use crate::MediaKind;

    // The internal/store token and the PascalCase wire token are DIFFERENT
    // fields and both are read: `as_str` is the store discriminator,
    // `base_item_kind` is `BaseItemDto.Type`, which strict kotlin clients
    // hard-require on every item.
    assert_eq!(MediaKind::Book.as_str(), "book");
    assert_eq!(MediaKind::Book.base_item_kind(), "Book");

    // Round-trip through the store token.
    assert_eq!(
        "book".parse::<MediaKind>().ok(),
        Some(MediaKind::Book),
        "FromStr must accept what as_str emits, or a stored row cannot be read back"
    );

    // Every kind's tokens stay distinct — a collision would silently merge two
    // kinds in the store or on the wire.
    let kinds = [
        MediaKind::Movie,
        MediaKind::Episode,
        MediaKind::Audio,
        MediaKind::Book,
    ];
    let store: std::collections::HashSet<_> = kinds.iter().map(|k| k.as_str()).collect();
    let wire: std::collections::HashSet<_> = kinds.iter().map(|k| k.base_item_kind()).collect();
    assert_eq!(
        store.len(),
        kinds.len(),
        "store discriminators must be distinct"
    );
    assert_eq!(wire.len(), kinds.len(), "wire Type tokens must be distinct");
}

#[test]
fn a_books_library_reports_the_books_collection_type() {
    use crate::LibraryKind;

    assert_eq!(LibraryKind::parse("books"), LibraryKind::Books);
    assert_eq!(LibraryKind::parse("book"), LibraryKind::Books);
    assert_eq!(LibraryKind::parse("Books"), LibraryKind::Books);

    // The token jellyfin-web matches on. Anything else and it renders a video
    // grid over items that will never play.
    assert_eq!(LibraryKind::Books.collection_type(), "books");

    // An unknown token still falls back to Mixed rather than to Books.
    assert_eq!(LibraryKind::parse("bookshelf"), LibraryKind::Mixed);
}

#[test]
fn book_format_round_trips_and_knows_what_a_client_can_open() {
    use crate::BookFormat;

    let all = [
        BookFormat::Epub,
        BookFormat::Pdf,
        BookFormat::Comic,
        BookFormat::Unreadable,
    ];

    // Store round-trip: whatever `as_str` writes, `parse` must read back, or a
    // stored book row decodes to the wrong format.
    for f in all {
        assert_eq!(
            BookFormat::parse(f.as_str()),
            Some(f),
            "{} did not round-trip",
            f.as_str()
        );
    }

    // Distinct discriminators — a collision would silently merge two formats.
    let tokens: std::collections::HashSet<_> = all.iter().map(|f| f.as_str()).collect();
    assert_eq!(tokens.len(), all.len());

    // An unrecognised token must NOT decode to a readable format. A hand-edited
    // or future-version row becomes None, and the caller decides, rather than
    // silently becoming an epub.
    assert_eq!(BookFormat::parse("azw3"), None);
    assert_eq!(BookFormat::parse(""), None);

    // THE authority on readability (see BookFormat::readable_by_client): exactly
    // one variant is unreadable. If a fifth format is added and no client can
    // open it, this assertion is what forces the decision to be explicit.
    for f in all {
        assert_eq!(
            f.readable_by_client(),
            f != BookFormat::Unreadable,
            "{} disagreed about client readability",
            f.as_str()
        );
    }
}

#[test]
fn sort_name_drops_one_leading_article_case_insensitively() {
    assert_eq!(sort_name("The Matrix"), "matrix");
    assert_eq!(sort_name("THE Matrix"), "matrix");
    assert_eq!(sort_name("A Quiet Place"), "quiet place");
    assert_eq!(sort_name("An American Tail"), "american tail");
    assert_eq!(sort_name("The The"), "the");
}

#[test]
fn sort_name_keeps_titles_that_only_start_like_an_article() {
    assert_eq!(sort_name("Theory of Everything"), "theory of everything");
    assert_eq!(sort_name("Anchorman"), "anchorman");
    assert_eq!(sort_name("A.I."), "a.i.");
    assert_eq!(sort_name("The"), "the");
    assert_eq!(sort_name("A"), "a");
}

#[test]
fn sort_name_pads_digit_runs_so_numbers_compare_by_value() {
    assert!(sort_name("Ep 2") < sort_name("Ep 10"));
    assert!(sort_name("Ep 10") < sort_name("Ep 100"));
    assert_eq!(sort_name("Ep 2"), "ep 00000000000000000002");
}

#[test]
fn sort_name_clamps_runs_longer_than_the_pad_width() {
    let twenty = "9".repeat(20);
    let long = "1".repeat(22);
    let longer = "2".repeat(25);
    assert!(sort_name(&format!("Ep {twenty}")) <= sort_name(&format!("Ep {long}")));
    assert_eq!(
        sort_name(&format!("Ep {long}")),
        sort_name(&format!("Ep {longer}"))
    );
    assert!(sort_name("Ep 5") < sort_name(&format!("Ep {long}")));
}

#[test]
fn sort_name_folds_unicode_case_like_title_fold() {
    assert_eq!(sort_name("Émile"), "Émile".to_lowercase());
}

/// Spec 033 B930: two shows carry the name and no year separates them, so the
/// provider's order picks one. The decision must say so, or the bug stays
/// invisible.
#[test]
fn match_decide_names_a_provider_order_tie_as_ambiguous() {
    let cands = vec![
        cand("71173", "Battlestar Galactica", Some(1978), &[]),
        cand("73545", "Battlestar Galactica", Some(2003), &[]),
    ];
    let d = match_decide("Battlestar Galactica", None, &cands, 0.7);
    assert_eq!(d.chosen.as_ref().map(|c| c.id.as_str()), Some("71173"));
    assert_eq!(d.runner_up.as_ref().map(|c| c.id.as_str()), Some("73545"));
    assert_eq!(d.decided_by, Some(MatchDecidedBy::ProviderOrder));
    assert!(d.ambiguous);
    assert_eq!(d.margin(), Some(0.0));
}

#[test]
fn match_decide_a_year_that_separates_two_names_is_not_ambiguous() {
    let cands = vec![
        cand("71173", "Battlestar Galactica", Some(1978), &[]),
        cand("73545", "Battlestar Galactica", Some(2003), &[]),
    ];
    // A floor low enough that the 1978 show's far-off year still clears it.
    let d = match_decide("Battlestar Galactica", Some(2003), &cands, 0.5);
    assert_eq!(d.chosen.as_ref().map(|c| c.id.as_str()), Some("73545"));
    assert_eq!(d.decided_by, Some(MatchDecidedBy::Year));
    assert!(!d.ambiguous, "margin {:?}", d.margin());
}

#[test]
fn match_decide_says_why_for_each_rule() {
    let sole = [cand("1", "Buffy", Some(1997), &[])];
    assert_eq!(
        match_decide("Buffy", None, &sole, 0.7).decided_by,
        Some(MatchDecidedBy::SoleCandidate)
    );
    let name = [
        cand("374456", "Dragon Ball R&R", Some(2018), &[]),
        cand("76666", "ドラゴンボール", Some(1986), &["Dragon Ball"]),
    ];
    let d = match_decide("Dragon Ball", None, &name, 0.7);
    assert_eq!(d.decided_by, Some(MatchDecidedBy::Name));
    assert!(!d.ambiguous, "an exact name over a prefix match is decided");
    let abridged = [
        cand("fan", "Dragon Ball Z Abridged", Some(1990), &[]),
        cand("real", "ドラゴンボールZ", Some(1989), &["Dragon Ball Z"]),
    ];
    let d = match_decide("Dragon Ball Z", Some(1990), &abridged, 0.7);
    assert_eq!(d.decided_by, Some(MatchDecidedBy::Name));
    assert!(d.margin().is_some_and(|m| m < 0.0), "{:?}", d.margin());
    let franchise = [
        cand("443905", "x", Some(2024), &["Code Geass: Dakkan no Z"]),
        cand(
            "79525",
            "y",
            Some(2006),
            &["Code Geass: Lelouch of the Rebellion"],
        ),
    ];
    let d = match_decide("Code Geass", None, &franchise, 0.7);
    assert_eq!(d.chosen.map(|c| c.id), Some("79525".into()));
    assert_eq!(d.decided_by, Some(MatchDecidedBy::EarliestPremiere));
    let none = [cand("9", "Completely Different", None, &[])];
    let d = match_decide("The Thing", None, &none, 0.7);
    assert_eq!((d.chosen, d.decided_by, d.ambiguous), (None, None, false));
    assert_eq!(
        d.candidates.len(),
        1,
        "a rejected candidate is still listed"
    );
}

#[test]
fn match_decided_by_labels_are_stable_and_distinct() {
    use std::collections::BTreeSet;
    let all = [
        MatchDecidedBy::SoleCandidate,
        MatchDecidedBy::Name,
        MatchDecidedBy::Year,
        MatchDecidedBy::EarliestPremiere,
        MatchDecidedBy::ProviderOrder,
        MatchDecidedBy::EpisodeFit,
        MatchDecidedBy::NfoVote,
    ];
    let labels: BTreeSet<&str> = all.iter().map(|d| d.label()).collect();
    assert_eq!(labels.len(), all.len());
    assert_eq!(MatchDecidedBy::ProviderOrder.label(), "provider_order");
    assert_eq!(MatchDecidedBy::EpisodeFit.label(), "episode_fit");
}

/// `match_best` before spec 033 split out [`match_decide`], kept verbatim so
/// the split is shown to change no answer.
fn match_best_before_033(
    query_title: &str,
    query_year: Option<u32>,
    candidates: &[SearchCandidate],
    min_confidence: f32,
) -> Option<MatchOutcome> {
    let query = normalize_title(query_title);
    let mut best: Option<(Option<usize>, f32, &SearchCandidate)> = None;
    let mut inexact: Vec<(f32, &SearchCandidate)> = Vec::new();
    for c in candidates {
        let names = || std::iter::once(&c.title).chain(&c.aliases);
        let exact_at = names().position(|n| normalize_title(n) == query);
        let similarity = names()
            .map(|n| title_similarity(query_title, n))
            .fold(0.0_f32, f32::max);
        let score = similarity * year_factor(query_year, c.year);
        if score < min_confidence {
            continue;
        }
        let beats = match &best {
            None => true,
            Some((best_at, best_score, _)) => match (exact_at, best_at) {
                (Some(_), None) => true,
                (None, Some(_)) => false,
                (Some(at), Some(best_at)) if at != *best_at => at < *best_at,
                _ => score > *best_score,
            },
        };
        if beats {
            best = Some((exact_at, score, c));
        }
        if exact_at.is_none() {
            inexact.push((score, c));
        }
    }
    if let Some((None, top, _)) = best {
        best = inexact
            .into_iter()
            .filter(|(score, _)| *score >= top - PREFIX_NEAR_TIE)
            .min_by_key(|(_, c)| c.year.unwrap_or(u32::MAX))
            .map(|(score, c)| (None, score, c));
    }
    best.map(|(_, confidence, c)| MatchOutcome {
        id: c.id.clone(),
        confidence,
    })
}

#[test]
fn match_best_answers_as_it_did_before_the_decision_was_split_out() {
    let names = [
        "Battlestar Galactica",
        "Battlestar Galactica (2003)",
        "Galactica",
        "The Flash",
        "Flash",
        "Code Geass",
        "Code Geass: Lelouch",
        "Dragon Ball",
        "Doctor Who",
        "One Piece",
        "Cosmos",
    ];
    let years = [
        None,
        Some(1978),
        Some(1979),
        Some(2003),
        Some(2004),
        Some(2014),
    ];
    let pool = [
        cand("a", "Battlestar Galactica", Some(1978), &[]),
        cand("b", "Battlestar Galactica", Some(2003), &["Galactica"]),
        cand("c", "Battlestar Galactica (2003)", Some(2004), &[]),
        cand("d", "The Flash", Some(1990), &[]),
        cand("e", "The Flash", Some(2014), &["Flash"]),
        cand(
            "f",
            "コードギアス",
            Some(2006),
            &["Code Geass: Lelouch of the Rebellion"],
        ),
        cand(
            "g",
            "コードギアス 奪還",
            Some(2024),
            &["Code Geass: Dakkan no Z"],
        ),
        cand("h", "Dragon Ball R&R", Some(2018), &[]),
        cand("i", "ドラゴンボール", Some(1986), &["Dragon Ball"]),
        cand("j", "Doctor Who", Some(1963), &[]),
        cand("k", "Doctor Who (2005)", Some(2005), &[]),
        cand("l", "ONE PIECE (2023)", Some(2023), &["One Piece"]),
        cand("m", "ワンピース", Some(1999), &["One Piece"]),
        cand("n", "Cosmos", None, &[]),
        cand("o", "Cosmos: A Spacetime Odyssey", Some(2014), &["Cosmos"]),
    ];
    let mut checked = 0;
    for name in names {
        for year in years {
            for start in 0..pool.len() {
                for len in 1..=4 {
                    let Some(slice) = pool.get(start..(start + len).min(pool.len())) else {
                        continue;
                    };
                    for floor in [0.5, 0.7, 0.9] {
                        assert_eq!(
                            match_best(name, year, slice, floor),
                            match_best_before_033(name, year, slice, floor),
                            "{name} {year:?} {slice:?} {floor}"
                        );
                        checked += 1;
                    }
                }
            }
        }
    }
    assert!(checked > 1000);
}
