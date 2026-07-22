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
        },
        SearchCandidate {
            id: "2".into(),
            title: "The Thing".into(),
            year: Some(1982),
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
    }];
    assert!(match_best("The Thing", Some(1982), &cands, 0.7).is_none());
}

#[test]
fn match_best_year_off_by_one_is_partial_not_zero() {
    let cands = vec![SearchCandidate {
        id: "3".into(),
        title: "Blade Runner".into(),
        year: Some(1983),
    }];
    // title exact, year ±1 -> still above threshold
    assert!(match_best("Blade Runner", Some(1982), &cands, 0.7).is_some());
}

#[test]
fn title_similarity_ignores_case_and_punctuation() {
    assert!(title_similarity("WALL·E", "wall e") > 0.85);
    assert!(title_similarity("Se7en", "Seven") < 0.9); // not falsely perfect
}
