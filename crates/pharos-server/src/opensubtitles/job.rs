//! Background job: fetch missing subtitles from OpenSubtitles (spec 030
//! slice 2, V567-V569, V571).
//!
//! For each video item with no text subtitle (embedded or already downloaded)
//! in a configured language, ask for the best result and keep it. A language
//! whose only tracks are disc pictures counts as missing (V1508). Bounded on every
//! side: one item at a time, at most [`JobConfig::max_downloads_per_day`]
//! downloads a UTC day, a pause between searches, the hash read behind the
//! background-I/O gate, a persisted memo so a miss is not asked again for a
//! week, and the pass stops the moment the provider reports its quota or rate
//! limit (the shared [`super::QuotaGate`] then stays closed until the reset).

use super::memo::MissMemo;
use super::quota::QuotaGate;
use super::telemetry::{
    provider_language, record_download, record_job, record_search, record_wanted, JobOutcome,
    WantReason,
};
use super::{store, OpenSubtitles, ProviderError, SubtitleHit};
use crate::api::jellyfin::remote_subtitles::{build_query, item_moviehash, rank};
use crate::api::jellyfin::subtitles::{
    discover_sidecars, is_image_subtitle_codec, sidecar_is_forced, sidecar_language_from_name,
};
use crate::bg_io::BgPermit;
use crate::state::Stores;
use pharos_core::{MediaItem, MediaKind, MediaStore};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::Semaphore;

/// Let the scan settle and the library fill before the first pass.
const WARMUP: Duration = Duration::from_secs(10 * 60);
/// A pass lists the whole library; hourly is plenty for a daily-capped job.
const PASS_INTERVAL: Duration = Duration::from_secs(60 * 60);
/// Pause between searches. The client also paces itself; this keeps the job's
/// own rhythm well under the documented 5 requests per second.
pub const DEFAULT_SPACING: Duration = Duration::from_secs(1);

#[derive(Debug, Clone)]
pub struct JobConfig {
    /// OpenSubtitles language codes (`en`), already mapped.
    pub languages: Vec<String>,
    pub max_downloads_per_day: u32,
    pub spacing: Duration,
}

impl JobConfig {
    pub fn from_config(c: &crate::config::OpenSubtitlesConfig) -> Self {
        Self {
            languages: c
                .languages
                .iter()
                .filter_map(|l| provider_language(l))
                .collect(),
            max_downloads_per_day: c.max_downloads_per_day,
            spacing: DEFAULT_SPACING,
        }
    }
}

/// On-disk shape of [`DailyBudget`] and the quota stop, kept in
/// `opensubtitles-budget.json` beside the miss memo so a restart cannot reset
/// the per-UTC-day ceiling or reopen a closed quota gate.
#[derive(Debug, Default, Serialize, Deserialize)]
struct LedgerDoc {
    utc_day: i64,
    downloads_spent: u32,
    quota_blocked_until: i64,
}

/// Downloads made in the current UTC day, persisted when `path` is set.
#[derive(Debug, Default)]
pub struct DailyBudget {
    day: i64,
    used: u32,
    /// The quota gate's closed-until instant as last persisted (unix seconds,
    /// 0 when open). `load` returns it so the caller can restore the gate.
    pub quota_blocked_until: i64,
    path: Option<PathBuf>,
}

impl DailyBudget {
    /// Restore the ledger from `path`. A missing or corrupt file is an empty
    /// ledger: losing it costs at most one day's budget, never a crash.
    pub fn load(path: PathBuf) -> Self {
        let doc: LedgerDoc = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        Self {
            day: doc.utc_day,
            used: doc.downloads_spent,
            quota_blocked_until: doc.quota_blocked_until,
            path: Some(path),
        }
    }

    /// Write the ledger atomically (temp file then rename), recording the
    /// gate's current closed-until instant. A failed write is logged and the
    /// pass carries on with the in-memory state.
    pub fn persist(&mut self, gate: &QuotaGate) {
        self.quota_blocked_until = gate.blocked_until();
        let Some(path) = &self.path else {
            return;
        };
        let doc = LedgerDoc {
            utc_day: self.day,
            downloads_spent: self.used,
            quota_blocked_until: self.quota_blocked_until,
        };
        let result = (|| -> std::io::Result<()> {
            let json = serde_json::to_string(&doc).map_err(std::io::Error::other)?;
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let tmp = path.with_extension("json.tmp");
            std::fs::write(&tmp, json)?;
            std::fs::rename(&tmp, path)
        })();
        if let Err(e) = result {
            tracing::warn!(path = %path.display(), error = %e, "opensubtitles job: could not save the download ledger");
        }
    }

    fn roll(&mut self, now_unix: i64) {
        let day = now_unix.div_euclid(86_400);
        if day != self.day {
            self.day = day;
            self.used = 0;
        }
    }

    pub fn has_room(&mut self, now_unix: i64, max: u32) -> bool {
        self.roll(now_unix);
        self.used < max
    }

    pub fn spend(&mut self, now_unix: i64) {
        self.roll(now_unix);
        self.used = self.used.saturating_add(1);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    Quota,
    Budget,
    /// A download could not be stored (full disk, unwritable cache).
    Store,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct PassSummary {
    pub wanted: usize,
    /// Of `wanted`: languages with only picture tracks.
    pub picture_only: usize,
    pub downloaded: usize,
    pub misses: usize,
    pub skipped_memo: usize,
    pub errors: usize,
    pub stopped: Option<StopReason>,
}

fn base_language(code: &str) -> String {
    let mapped = provider_language(code).unwrap_or_else(|| code.to_ascii_lowercase());
    mapped.split('-').next().unwrap_or_default().to_string()
}

pub(crate) fn same_language(have: &str, want: &str) -> bool {
    base_language(have) == base_language(want)
}

/// Why the job wants a subtitle for an item and language.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Want {
    /// No subtitle in the language at all.
    Missing,
    /// Only disc-image (PGS, VobSub, DVB) tracks, which the player's text
    /// engine cannot draw (V1508). `sdh` when one of them is hearing-impaired,
    /// so the text stand-in should be too.
    PictureOnly { sdh: bool },
}

impl Want {
    pub fn reason(self) -> WantReason {
        match self {
            Want::Missing => WantReason::Missing,
            Want::PictureOnly { .. } => WantReason::PictureOnly,
        }
    }

    fn prefers_sdh(self) -> bool {
        matches!(self, Want::PictureOnly { sdh: true })
    }
}

/// What the item needs in `language`: `None` when it already has a
/// (non-forced) text subtitle embedded, beside the media, or downloaded
/// earlier. A forced track never counts; a picture track counts only as
/// [`Want::PictureOnly`].
async fn wants_subtitle(item: &MediaItem, language: &str) -> Option<Want> {
    let tracks: Vec<_> = item
        .probe
        .subtitle_tracks
        .iter()
        .filter(|t| {
            !t.is_forced
                && t.language
                    .as_deref()
                    .is_some_and(|l| same_language(l, language))
        })
        .collect();
    let is_picture =
        |t: &&pharos_core::SubtitleTrack| t.codec.as_deref().is_some_and(is_image_subtitle_codec);
    if !tracks.iter().all(is_picture) {
        return None;
    }
    // A forced sidecar carries only foreign parts, so like a forced track it
    // does not count as a full text subtitle.
    let has_sidecar = discover_sidecars(&item.path).await.iter().any(|(p, _)| {
        p.file_name()
            .and_then(|n| n.to_str())
            .filter(|n| !sidecar_is_forced(n))
            .and_then(sidecar_language_from_name)
            .is_some_and(|l| same_language(&l, language))
    });
    if has_sidecar {
        return None;
    }
    Some(if tracks.is_empty() {
        Want::Missing
    } else {
        Want::PictureOnly {
            sdh: tracks.iter().any(|t| t.is_hearing_impaired),
        }
    })
}

async fn file_sig(path: &Path) -> Option<(i64, u64)> {
    let m = tokio::fs::metadata(path).await.ok()?;
    let mtime = m
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_secs()).ok())?;
    Some((mtime, m.len()))
}

/// The hit worth keeping unattended: a hash match or a trusted uploader, never
/// a machine translation or a foreign-parts-only track. With `prefer_sdh`, a
/// hearing-impaired file wins within the best tier (a hash match beats an SDH
/// file that was not timed to this release).
fn pick(mut hits: Vec<SubtitleHit>, prefer_sdh: bool) -> Option<SubtitleHit> {
    hits.retain(|h| {
        !h.ai_translated
            && !h.machine_translated
            && !h.foreign_parts_only
            && (h.hash_match || h.from_trusted)
    });
    rank(&mut hits);
    let best_tier = hits.first()?.hash_match;
    let at = prefer_sdh
        .then(|| {
            hits.iter()
                .position(|h| h.hash_match == best_tier && h.hearing_impaired)
        })
        .flatten()
        .unwrap_or(0);
    (at < hits.len()).then(|| hits.swap_remove(at))
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// One pass over `items`. `now_unix` is a parameter so tests control the clock.
#[allow(clippy::too_many_arguments)]
pub async fn run_pass(
    os: &OpenSubtitles,
    stores: &Stores,
    memo: &MissMemo,
    budget: &mut DailyBudget,
    cfg: &JobConfig,
    items: &[MediaItem],
    now_unix: i64,
    bg_io: &Arc<Semaphore>,
) -> PassSummary {
    let mut sum = PassSummary::default();
    'items: for item in items
        .iter()
        .filter(|i| matches!(i.kind, MediaKind::Movie | MediaKind::Episode))
    {
        // The hash reads the media file, so it is computed once per item, on
        // the first language that reaches the search, and shared by the rest.
        let mut moviehash: Option<Option<String>> = None;
        for language in &cfg.languages {
            let Some(want) = wants_subtitle(item, language).await else {
                continue;
            };
            sum.wanted += 1;
            if want != Want::Missing {
                sum.picture_only += 1;
            }
            record_wanted(want.reason());
            let Some(sig) = file_sig(&item.path).await else {
                continue;
            };
            if memo.is_fresh_miss(item.id, language, sig, now_unix) {
                sum.skipped_memo += 1;
                record_job(JobOutcome::SkippedMemo);
                continue;
            }
            if let Some(secs) = os.gate.blocked_for(now_unix) {
                tracing::info!(secs, "opensubtitles job: quota gate closed; pass stopped");
                record_job(JobOutcome::QuotaStop);
                sum.stopped = Some(StopReason::Quota);
                break 'items;
            }
            if !budget.has_room(now_unix, cfg.max_downloads_per_day) {
                tracing::info!(
                    max = cfg.max_downloads_per_day,
                    "opensubtitles job: daily download ceiling reached; pass stopped"
                );
                record_job(JobOutcome::BudgetStop);
                sum.stopped = Some(StopReason::Budget);
                break 'items;
            }
            if !cfg.spacing.is_zero() {
                tokio::time::sleep(cfg.spacing).await;
            }
            let query = {
                let _permit = BgPermit::acquire(bg_io).await;
                let hash = match moviehash.clone() {
                    Some(h) => h,
                    None => {
                        let h = item_moviehash(item).await;
                        moviehash = Some(h.clone());
                        h
                    }
                };
                build_query(stores, item, language.clone(), hash).await
            };
            let searched = os.provider.search(&query).await;
            record_search(&searched);
            let hits = match searched {
                Ok(h) => h,
                Err(e) => {
                    if stop_on(os, &e, &mut sum) {
                        break 'items;
                    }
                    tracing::warn!(item = item.id, language = %language, error = %e, "opensubtitles job: search failed");
                    sum.errors += 1;
                    record_job(JobOutcome::Error);
                    continue;
                }
            };
            let Some(hit) = pick(hits, want.prefers_sdh()) else {
                memo.record_miss(item.id, language, sig, now_unix);
                sum.misses += 1;
                record_job(JobOutcome::Miss);
                tracing::info!(
                    item = item.id,
                    language = %language,
                    reason = want.reason().label(),
                    "opensubtitles job: nothing good enough; remembered"
                );
                continue;
            };
            let downloaded = os.provider.download(hit.file_id).await;
            record_download(&downloaded);
            match downloaded {
                Ok(d) => {
                    // The provider debited its quota for this file, so it is
                    // charged whatever becomes of the bytes below.
                    budget.spend(now_unix);
                    budget.persist(&os.gate);
                    if d.bytes.is_empty() {
                        memo.record_miss(item.id, language, sig, now_unix);
                        tracing::warn!(
                            item = item.id,
                            file_id = hit.file_id,
                            "opensubtitles job: empty download; remembered"
                        );
                        sum.errors += 1;
                        record_job(JobOutcome::Error);
                        continue;
                    }
                    match store::save(
                        &os.root,
                        &item.path,
                        language,
                        hit.file_id,
                        hit.hearing_impaired,
                        &d.bytes,
                    )
                    .await
                    {
                        Ok(path) => {
                            sum.downloaded += 1;
                            record_job(JobOutcome::Downloaded);
                            tracing::info!(
                                item = item.id,
                                language = %language,
                                file_id = hit.file_id,
                                hash_match = hit.hash_match,
                                reason = want.reason().label(),
                                wanted_sdh = want.prefers_sdh(),
                                sdh = hit.hearing_impaired,
                                remaining = d.remaining,
                                path = %path.display(),
                                "opensubtitles job: downloaded"
                            );
                        }
                        Err(e) => {
                            // A full disk will not clear mid-pass: stop here.
                            tracing::warn!(item = item.id, error = %e, "opensubtitles job: could not store the download; pass stopped");
                            sum.errors += 1;
                            record_job(JobOutcome::Error);
                            sum.stopped = Some(StopReason::Store);
                            break 'items;
                        }
                    }
                }
                Err(e) => {
                    if stop_on(os, &e, &mut sum) {
                        break 'items;
                    }
                    tracing::warn!(item = item.id, file_id = hit.file_id, error = %e, "opensubtitles job: download failed");
                    sum.errors += 1;
                    record_job(JobOutcome::Error);
                }
            }
        }
    }
    if let Err(e) = memo.save() {
        tracing::warn!(path = %memo.path().display(), error = %e, "opensubtitles job: could not save the miss memo");
    }
    budget.persist(&os.gate);
    tracing::info!(
        wanted = sum.wanted,
        picture_only = sum.picture_only,
        downloaded = sum.downloaded,
        misses = sum.misses,
        skipped_memo = sum.skipped_memo,
        errors = sum.errors,
        stopped = ?sum.stopped,
        "opensubtitles job: pass done"
    );
    sum
}

/// Quota and rate-limit errors end the pass (V567); the gate is closed for a
/// quota so the next pass does not try either.
fn stop_on(os: &OpenSubtitles, e: &ProviderError, sum: &mut PassSummary) -> bool {
    match e {
        ProviderError::QuotaExceeded { reset_unix } => os.gate.block_until(*reset_unix),
        ProviderError::RateLimited { .. } => {}
        _ => return false,
    }
    tracing::warn!(error = %e, "opensubtitles job: provider limit hit; pass stopped");
    record_job(JobOutcome::QuotaStop);
    sum.stopped = Some(StopReason::Quota);
    true
}

/// Spawn the job. A failure ends only the pass (logged), never the server.
pub fn spawn(
    os: OpenSubtitles,
    stores: Stores,
    cfg: JobConfig,
    memo_path: std::path::PathBuf,
    bg_leader: Arc<AtomicBool>,
    bg_io: Arc<Semaphore>,
) {
    let budget_path = memo_path.with_file_name("opensubtitles-budget.json");
    tracing::info!(
        languages = ?cfg.languages,
        max_per_day = cfg.max_downloads_per_day,
        memo = %memo_path.display(),
        budget = %budget_path.display(),
        "opensubtitles job: spawning missing-subtitle sweep"
    );
    tokio::spawn(async move {
        let memo = MissMemo::load(memo_path);
        let mut budget = DailyBudget::load(budget_path);
        // A quota closed before the restart stays closed after it.
        os.gate.block_until(budget.quota_blocked_until);
        tokio::time::sleep(WARMUP).await;
        // One replica sweeps; a second would spend the same quota.
        while !bg_leader.load(Ordering::Relaxed) {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        loop {
            match stores.list().await {
                Ok(items) => {
                    run_pass(
                        &os,
                        &stores,
                        &memo,
                        &mut budget,
                        &cfg,
                        &items,
                        now_unix(),
                        &bg_io,
                    )
                    .await;
                }
                Err(e) => tracing::warn!(error = %e, "opensubtitles job: item list failed"),
            }
            tokio::time::sleep(PASS_INTERVAL).await;
        }
    });
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::opensubtitles::{BoxFut, Downloaded, QuotaGate, SearchQuery, SubtitleProvider};
    use std::sync::atomic::AtomicU32;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Fake {
        /// Per file name stem: hits to return.
        hits: Mutex<Vec<SubtitleHit>>,
        searches: AtomicU32,
        downloads: AtomicU32,
        quota_on_download: bool,
        /// Answer every download with an empty body.
        empty_body: bool,
    }

    impl SubtitleProvider for Fake {
        fn search<'a>(
            &'a self,
            _q: &'a SearchQuery,
        ) -> BoxFut<'a, Result<Vec<SubtitleHit>, ProviderError>> {
            self.searches.fetch_add(1, Ordering::SeqCst);
            let hits = self.hits.lock().unwrap().clone();
            Box::pin(async move { Ok(hits) })
        }

        fn download(&self, _id: u64) -> BoxFut<'_, Result<Downloaded, ProviderError>> {
            self.downloads.fetch_add(1, Ordering::SeqCst);
            let quota = self.quota_on_download;
            let empty = self.empty_body;
            Box::pin(async move {
                if quota {
                    return Err(ProviderError::QuotaExceeded {
                        reset_unix: 9_000_000_000,
                    });
                }
                Ok(Downloaded {
                    file_name: "x.srt".into(),
                    bytes: if empty {
                        Vec::new()
                    } else {
                        b"1\n00:00:01,000 --> 00:00:02,000\nhi\n".to_vec()
                    },
                    remaining: Some(1),
                })
            })
        }
    }

    fn hit(file_id: u64, hash_match: bool) -> SubtitleHit {
        SubtitleHit {
            file_id,
            subtitle_id: file_id.to_string(),
            language: "en".into(),
            release: "r".into(),
            download_count: 1,
            ratings: 0.0,
            from_trusted: false,
            hearing_impaired: false,
            ai_translated: false,
            machine_translated: false,
            foreign_parts_only: false,
            fps: None,
            uploader: None,
            upload_date: None,
            hash_match,
        }
    }

    struct Rig {
        _dir: tempfile::TempDir,
        os: OpenSubtitles,
        fake: Arc<Fake>,
        stores: Stores,
        memo: MissMemo,
        items: Vec<MediaItem>,
        bg_io: Arc<Semaphore>,
    }

    async fn rig(n_items: u64, fake: Fake) -> Rig {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(fake);
        let provider: Arc<dyn SubtitleProvider> = fake.clone();
        let os = OpenSubtitles {
            provider,
            root: store::shared_test_root(),
            gate: Arc::new(QuotaGate::new()),
        };
        let mut items = Vec::new();
        for i in 0..n_items {
            let path = dir.path().join(format!("movie{i}.mkv"));
            std::fs::write(&path, vec![1u8; 130 * 1024]).unwrap();
            items.push(MediaItem {
                id: 1000 + i,
                path,
                title: format!("Movie {i}"),
                kind: MediaKind::Movie,
                ..MediaItem::default()
            });
        }
        Rig {
            memo: MissMemo::load(dir.path().join("misses.json")),
            _dir: dir,
            os,
            fake,
            stores: Stores::connect("sqlite::memory:").await.unwrap(),
            items,
            bg_io: Arc::new(Semaphore::new(4)),
        }
    }

    fn cfg(max: u32) -> JobConfig {
        JobConfig {
            languages: vec!["en".into()],
            max_downloads_per_day: max,
            spacing: Duration::ZERO,
        }
    }

    async fn pass(r: &Rig, budget: &mut DailyBudget, c: &JobConfig, now: i64) -> PassSummary {
        run_pass(
            &r.os, &r.stores, &r.memo, budget, c, &r.items, now, &r.bg_io,
        )
        .await
    }

    #[tokio::test]
    async fn downloads_what_is_missing_and_leaves_what_is_there() {
        let mut r = rig(2, Fake::default()).await;
        r.fake.hits.lock().unwrap().push(hit(5, true));
        // Item 1 already has an English subtitle embedded.
        r.items[1]
            .probe
            .subtitle_tracks
            .push(pharos_core::SubtitleTrack {
                stream_index: 2,
                language: Some("eng".into()),
                ..Default::default()
            });
        let mut budget = DailyBudget::default();
        let sum = pass(&r, &mut budget, &cfg(20), 1_000).await;
        assert_eq!(
            (sum.wanted, sum.downloaded, sum.stopped),
            (1, 1, None),
            "{sum:?}"
        );
        assert_eq!(r.fake.searches.load(Ordering::SeqCst), 1);
        assert!(store::item_dir(&r.os.root, &r.items[0].path)
            .join("00000000-os5.en.srt")
            .exists());
        // A second pass sees the downloaded file and asks for nothing.
        let sum = pass(&r, &mut budget, &cfg(20), 1_001).await;
        assert_eq!(sum.wanted, 0, "{sum:?}");
        assert_eq!(r.fake.searches.load(Ordering::SeqCst), 1);
    }

    /// V569: a miss is remembered; the next pass does not search again.
    #[tokio::test]
    async fn a_miss_is_not_asked_again_until_the_memo_expires() {
        let r = rig(1, Fake::default()).await;
        let mut budget = DailyBudget::default();
        let sum = pass(&r, &mut budget, &cfg(20), 1_000).await;
        assert_eq!((sum.misses, sum.downloaded), (1, 0));
        let sum = pass(&r, &mut budget, &cfg(20), 2_000).await;
        assert_eq!((sum.skipped_memo, sum.misses), (1, 0));
        assert_eq!(r.fake.searches.load(Ordering::SeqCst), 1);
        let later = 1_000 + crate::opensubtitles::memo::MISS_TTL_SECS;
        let sum = pass(&r, &mut budget, &cfg(20), later).await;
        assert_eq!(sum.misses, 1, "expired: asked again");
        assert_eq!(r.fake.searches.load(Ordering::SeqCst), 2);
    }

    /// V568: the daily ceiling stops the pass, and a new UTC day resumes.
    #[tokio::test]
    async fn the_daily_ceiling_stops_the_pass_and_the_next_day_resumes() {
        let r = rig(3, Fake::default()).await;
        r.fake.hits.lock().unwrap().push(hit(5, true));
        let mut budget = DailyBudget::default();
        let sum = pass(&r, &mut budget, &cfg(2), 1_000).await;
        assert_eq!((sum.downloaded, sum.stopped), (2, Some(StopReason::Budget)));
        assert_eq!(r.fake.downloads.load(Ordering::SeqCst), 2);
        let sum = pass(&r, &mut budget, &cfg(2), 1_000 + 86_400).await;
        assert_eq!((sum.downloaded, sum.stopped), (1, None));
    }

    /// V567: the provider's quota ends the pass and keeps later passes out
    /// until the reset.
    #[tokio::test]
    async fn a_quota_answer_stops_the_pass_and_closes_the_gate() {
        let r = rig(
            3,
            Fake {
                quota_on_download: true,
                ..Fake::default()
            },
        )
        .await;
        r.fake.hits.lock().unwrap().push(hit(5, true));
        let mut budget = DailyBudget::default();
        let sum = pass(&r, &mut budget, &cfg(20), 1_000).await;
        assert_eq!((sum.downloaded, sum.stopped), (0, Some(StopReason::Quota)));
        assert_eq!(
            r.fake.downloads.load(Ordering::SeqCst),
            1,
            "only one attempt"
        );
        let sum = pass(&r, &mut budget, &cfg(20), 1_100).await;
        assert_eq!(sum.stopped, Some(StopReason::Quota));
        assert_eq!(
            r.fake.downloads.load(Ordering::SeqCst),
            1,
            "no attempt while closed"
        );
    }

    /// A download the provider charged for is charged here too, even when the
    /// body is empty, and the empty answer is remembered so it is not re-bought
    /// every hour.
    #[tokio::test]
    async fn an_empty_download_is_charged_and_remembered() {
        let r = rig(
            1,
            Fake {
                empty_body: true,
                ..Fake::default()
            },
        )
        .await;
        r.fake.hits.lock().unwrap().push(hit(5, true));
        let mut budget = DailyBudget::default();
        let sum = pass(&r, &mut budget, &cfg(20), 1_000).await;
        assert_eq!((sum.downloaded, sum.errors), (0, 1), "{sum:?}");
        assert_eq!(budget.used, 1, "the provider was paid");
        let sum = pass(&r, &mut budget, &cfg(20), 1_001).await;
        assert_eq!(sum.skipped_memo, 1, "{sum:?}");
        assert_eq!(r.fake.downloads.load(Ordering::SeqCst), 1);
    }

    /// A store failure is charged, stops the pass (a full disk does not clear
    /// mid-pass), and does not reach the second item.
    #[tokio::test]
    async fn a_store_failure_is_charged_and_stops_the_pass() {
        let mut r = rig(2, Fake::default()).await;
        r.fake.hits.lock().unwrap().push(hit(5, true));
        // A regular file where the store root's parent directory should be:
        // `create_dir_all` fails whoever the test runs as.
        let blocker = r._dir.path().join("blocker");
        std::fs::write(&blocker, b"not a directory").unwrap();
        r.os.root = blocker.join("opensubtitles");
        let mut budget = DailyBudget::default();
        let sum = pass(&r, &mut budget, &cfg(20), 1_000).await;
        assert_eq!(
            (sum.downloaded, sum.errors, sum.stopped),
            (0, 1, Some(StopReason::Store)),
            "{sum:?}"
        );
        assert_eq!(r.fake.downloads.load(Ordering::SeqCst), 1, "one attempt");
        assert_eq!(budget.used, 1, "the provider was paid");
    }

    /// The ceiling and the quota stop survive a restart; a new UTC day resets
    /// the count but not the absolute quota instant.
    #[test]
    fn the_ledger_survives_a_restart_and_resets_on_a_new_utc_day() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("budget.json");
        let mut b = DailyBudget::load(p.clone());
        let gate = QuotaGate::new();
        b.spend(1_000);
        b.spend(1_000);
        gate.block_until(5_000);
        b.persist(&gate);
        let mut again = DailyBudget::load(p);
        assert_eq!(again.quota_blocked_until, 5_000);
        let restored = QuotaGate::new();
        restored.block_until(again.quota_blocked_until);
        assert_eq!(restored.blocked_for(1_000), Some(4_000), "still closed");
        assert!(!again.has_room(1_001, 2), "still spent after a restart");
        assert!(again.has_room(1_000 + 86_400, 2), "a new UTC day resets");
    }

    /// The pass persists what it spent before it returns.
    #[tokio::test]
    async fn a_pass_persists_its_spend() {
        let r = rig(2, Fake::default()).await;
        r.fake.hits.lock().unwrap().push(hit(5, true));
        let path = r.memo.path().with_file_name("budget.json");
        let mut budget = DailyBudget::load(path.clone());
        let sum = pass(&r, &mut budget, &cfg(1), 1_000).await;
        assert_eq!((sum.downloaded, sum.stopped), (1, Some(StopReason::Budget)));
        let mut after = DailyBudget::load(path);
        assert!(
            !after.has_room(1_001, 1),
            "the restart remembers the download"
        );
    }

    #[test]
    fn only_a_hash_match_or_trusted_hit_is_kept_unattended() {
        let mut machine = hit(1, true);
        machine.machine_translated = true;
        let plain = hit(2, false);
        let mut trusted = hit(3, false);
        trusted.from_trusted = true;
        assert_eq!(
            pick(vec![machine, plain.clone()], false).map(|h| h.file_id),
            None
        );
        assert_eq!(
            pick(vec![plain, trusted], false).map(|h| h.file_id),
            Some(3)
        );
    }

    fn sdh(mut h: SubtitleHit) -> SubtitleHit {
        h.hearing_impaired = true;
        h
    }

    /// V1508: an SDH file wins when the disc track was SDH, but only within
    /// the best tier, so timing to this release still comes first.
    #[test]
    fn sdh_is_preferred_within_the_best_tier_only() {
        let mut trusted_sdh = sdh(hit(1, false));
        trusted_sdh.from_trusted = true;
        let mut popular = hit(2, true);
        popular.download_count = 99;
        let matched_sdh = sdh(hit(3, true));
        let all = vec![trusted_sdh.clone(), popular.clone(), matched_sdh];
        assert_eq!(pick(all.clone(), true).map(|h| h.file_id), Some(3));
        assert_eq!(pick(all, false).map(|h| h.file_id), Some(2));
        assert_eq!(
            pick(vec![trusted_sdh, popular], true).map(|h| h.file_id),
            Some(2),
            "a hash match beats an SDH file timed to another release"
        );
    }

    fn picture_track(index: u32, sdh: bool) -> pharos_core::SubtitleTrack {
        pharos_core::SubtitleTrack {
            stream_index: index,
            language: Some("eng".into()),
            codec: Some("hdmv_pgs_subtitle".into()),
            is_hearing_impaired: sdh,
            ..Default::default()
        }
    }

    /// V1508: a language whose only tracks are pictures is fetched as text,
    /// SDH when the picture track is, stored with an `sdh` tag, and a text
    /// track in the language still means nothing is wanted.
    #[tokio::test]
    async fn a_picture_only_language_fetches_a_text_sdh_stand_in() {
        let recorder = metrics_util::debugging::DebuggingRecorder::new();
        let snapshotter = pharos_test_metrics::snapshotter(&recorder);
        let _guard = metrics::set_default_local_recorder(&recorder);
        let mut r = rig(2, Fake::default()).await;
        r.fake
            .hits
            .lock()
            .unwrap()
            .extend([hit(5, true), sdh(hit(6, true))]);
        r.items[0]
            .probe
            .subtitle_tracks
            .push(picture_track(2, true));
        r.items[1]
            .probe
            .subtitle_tracks
            .push(picture_track(2, false));
        r.items[1]
            .probe
            .subtitle_tracks
            .push(pharos_core::SubtitleTrack {
                stream_index: 3,
                language: Some("eng".into()),
                codec: Some("subrip".into()),
                ..Default::default()
            });
        let mut budget = DailyBudget::default();
        let sum = pass(&r, &mut budget, &cfg(20), 1_000).await;
        assert_eq!(
            (sum.wanted, sum.picture_only, sum.downloaded),
            (1, 1, 1),
            "{sum:?}"
        );
        assert!(store::item_dir(&r.os.root, &r.items[0].path)
            .join("00000000-os6.en.sdh.srt")
            .exists());
        let reasons: Vec<String> = snapshotter
            .snapshot()
            .into_vec()
            .into_iter()
            .filter(|(k, ..)| k.key().name() == "pharos_opensubtitles_wanted_total")
            .flat_map(|(k, ..)| {
                k.key()
                    .labels()
                    .filter(|l| l.key() == "reason")
                    .map(|l| l.value().to_string())
                    .collect::<Vec<_>>()
            })
            .collect();
        assert_eq!(reasons, ["picture_only"]);
        let sum = pass(&r, &mut budget, &cfg(20), 1_001).await;
        assert_eq!(sum.wanted, 0, "the stand-in counts as text: {sum:?}");
    }

    /// A forced sidecar holds only foreign parts, so it does not stop the job
    /// fetching full text for a picture-only language; a full one does.
    #[tokio::test]
    async fn a_forced_sidecar_does_not_count_as_text() {
        // One folder each: the sidecar scan is cached per folder by its mtime
        // in whole seconds, so a file added to the same folder within the
        // second would not be seen.
        async fn wants_with(sidecars: &[&str]) -> Option<Want> {
            let dir = tempfile::tempdir().unwrap();
            let media = dir.path().join("film.mkv");
            std::fs::write(&media, b"x").unwrap();
            for name in sidecars {
                std::fs::write(dir.path().join(name), b"1").unwrap();
            }
            let mut item = MediaItem {
                id: 1,
                path: media,
                kind: MediaKind::Movie,
                ..MediaItem::default()
            };
            item.probe.subtitle_tracks.push(picture_track(2, false));
            wants_subtitle(&item, "en").await
        }
        assert!(matches!(
            wants_with(&["film.en.forced.srt"]).await,
            Some(Want::PictureOnly { sdh: false })
        ));
        assert!(wants_with(&["film.en.forced.srt", "film.en.srt"])
            .await
            .is_none());
    }

    #[test]
    fn every_job_outcome_is_counted_with_op_job() {
        use metrics_util::debugging::DebuggingRecorder;
        let recorder = DebuggingRecorder::new();
        let snapshotter = pharos_test_metrics::snapshotter(&recorder);
        let _guard = metrics::set_default_local_recorder(&recorder);
        for o in JobOutcome::ALL {
            record_job(o);
        }
        let n = snapshotter
            .snapshot()
            .into_vec()
            .into_iter()
            .filter(|(k, ..)| {
                k.key().name() == "pharos_opensubtitles_total"
                    && k.key()
                        .labels()
                        .any(|l| l.key() == "op" && l.value() == "job")
            })
            .count();
        assert_eq!(n, JobOutcome::ALL.len());
    }
}
