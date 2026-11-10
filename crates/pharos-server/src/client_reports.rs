//! "Report a problem" documents (spec 016 US5, FR-008, ADR-0099): one JSON
//! file per report under the report directory, named by its id. A report is
//! kept for the retention window, then a sweep deletes it and counts it in
//! `pharos_ui_reports_expired_total`. Only the reporter and admins read one;
//! the RPC layer enforces that with [`StoredReport::owner`].

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use pharos_core::UserId;
use serde::{Deserialize, Serialize};

/// Characters a report id is spelled in: Crockford base32, so an id read
/// aloud or retyped has no 0/O or 1/I/L to confuse.
const ID_ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
/// Characters either side of the dash in `XXXX-XXXX`: 40 bits, plenty for a
/// store capped at [`MAX_STORED_REPORTS`].
const ID_HALF: usize = 4;
/// Reports kept at once across every user, so a public server's disk can't be
/// filled through this call.
pub const MAX_STORED_REPORTS: usize = 500;
/// Reports one user may have stored at once; the 21st is refused until one
/// expires or the reporter deletes one.
pub const MAX_REPORTS_PER_USER: usize = 20;
/// Tries at a fresh id before giving up; a clash among 2^40 ids is already
/// vanishingly rare.
const ID_ATTEMPTS: u32 = 4;
/// Default days a report is kept (016 clarification, 2026-11-01).
pub const DEFAULT_RETENTION_DAYS: u32 = 30;

/// A report's id, `XXXX-XXXX` in [`ID_ALPHABET`]. Parsing one from a client
/// is the only way to name a file, so an id never carries a path.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ReportId(String);

impl ReportId {
    pub fn parse(text: &str) -> Option<Self> {
        let (a, b) = text.split_once('-')?;
        let ok =
            |half: &str| half.len() == ID_HALF && half.bytes().all(|c| ID_ALPHABET.contains(&c));
        (ok(a) && ok(b)).then(|| Self(text.to_owned()))
    }

    fn random() -> Result<Self, getrandom::Error> {
        let mut bytes = [0u8; 2 * ID_HALF];
        getrandom::fill(&mut bytes)?;
        let mut out = String::with_capacity(2 * ID_HALF + 1);
        for (i, b) in bytes.iter().enumerate() {
            if i == ID_HALF {
                out.push('-');
            }
            out.push(char::from(ID_ALPHABET[usize::from(b & 0x1f)]));
        }
        Ok(Self(out))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn file_name(&self) -> String {
        format!("{}.json", self.0)
    }
}

impl TryFrom<String> for ReportId {
    type Error = String;
    fn try_from(s: String) -> Result<Self, String> {
        Self::parse(&s).ok_or_else(|| format!("not a report id: {s:?}"))
    }
}

impl From<ReportId> for String {
    fn from(id: ReportId) -> String {
        id.0
    }
}

impl std::fmt::Display for ReportId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// What a client sent, already bounded and redacted by the RPC layer.
#[derive(Debug, Clone)]
pub struct NewReport {
    pub owner: UserId,
    pub user_name: String,
    pub device: Option<String>,
    pub note: String,
    pub trace_ids: Vec<String>,
    pub bundle: serde_json::Value,
}

/// The stored document. [`Summary`] reads the same file and skips `bundle`
/// without keeping it.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Document {
    id: ReportId,
    received_at: String,
    received_wall_ms: u64,
    owner: UserId,
    user_name: String,
    device: Option<String>,
    note: String,
    trace_ids: Vec<String>,
    bundle: serde_json::Value,
}

/// A stored report without its bundle: what a list shows.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Summary {
    pub id: ReportId,
    pub received_wall_ms: u64,
    pub owner: UserId,
    pub user_name: String,
    pub note: String,
    #[serde(skip)]
    pub bytes: u64,
}

/// One report's whole document and who may read it.
#[derive(Debug)]
pub struct StoredReport {
    pub owner: UserId,
    pub document: Vec<u8>,
}

#[derive(Debug, thiserror::Error)]
pub enum SaveError {
    #[error("report store holds {held} reports, cap {max}")]
    StoreFull { held: usize, max: usize },
    #[error("user {owner} holds {held} reports, cap {max}")]
    UserFull {
        owner: UserId,
        held: usize,
        max: usize,
    },
    #[error("listing report dir {dir}: {source}")]
    List {
        dir: PathBuf,
        source: std::io::Error,
    },
    #[error("no random source for a report id: {0}")]
    Random(getrandom::Error),
    #[error("no free report id after {ID_ATTEMPTS} tries")]
    NoFreeId,
    #[error("encoding report {id}: {source}")]
    Encode {
        id: ReportId,
        source: serde_json::Error,
    },
    #[error("writing report {path}: {source}")]
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
}

/// Why one report was refused, for `pharos_ui_reports_total{outcome}`.
impl SaveError {
    pub fn outcome(&self) -> &'static str {
        match self {
            SaveError::StoreFull { .. } => "store_full",
            SaveError::UserFull { .. } => "user_full",
            SaveError::List { .. }
            | SaveError::Random(_)
            | SaveError::NoFreeId
            | SaveError::Encode { .. }
            | SaveError::Write { .. } => "store_failed",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ReportStore {
    dir: PathBuf,
    retention: Duration,
}

impl ReportStore {
    pub fn new(dir: PathBuf, retention_days: u32) -> Self {
        Self {
            dir,
            retention: Duration::from_secs(u64::from(retention_days) * 86_400),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn retention(&self) -> Duration {
        self.retention
    }

    /// Writes `report` under a fresh id, refusing when the store or the
    /// owner is at its cap. Two saves racing can each pass the cap check, so
    /// a cap may be overshot by the number in flight. The file is written
    /// beside its final name and renamed, so a reader never sees half a report.
    pub async fn save(&self, report: NewReport, now: SystemTime) -> Result<ReportId, SaveError> {
        let held = self.summaries().await.map_err(|source| SaveError::List {
            dir: self.dir.clone(),
            source,
        })?;
        if held.len() >= MAX_STORED_REPORTS {
            return Err(SaveError::StoreFull {
                held: held.len(),
                max: MAX_STORED_REPORTS,
            });
        }
        let mine = held.iter().filter(|s| s.owner == report.owner).count();
        if mine >= MAX_REPORTS_PER_USER {
            return Err(SaveError::UserFull {
                owner: report.owner,
                held: mine,
                max: MAX_REPORTS_PER_USER,
            });
        }
        let mut id = None;
        for _ in 0..ID_ATTEMPTS {
            let candidate = ReportId::random().map_err(SaveError::Random)?;
            if !held.iter().any(|s| s.id == candidate) {
                id = Some(candidate);
                break;
            }
        }
        let id = id.ok_or(SaveError::NoFreeId)?;
        let wall_ms = wall_ms(now);
        let doc = Document {
            id: id.clone(),
            received_at: pharos_jellyfin_api::dto::format_iso8601_ms(
                i64::try_from(wall_ms).unwrap_or(i64::MAX),
            ),
            received_wall_ms: wall_ms,
            owner: report.owner,
            user_name: report.user_name,
            device: report.device,
            note: report.note,
            trace_ids: report.trace_ids,
            bundle: report.bundle,
        };
        let bytes = serde_json::to_vec(&doc).map_err(|source| SaveError::Encode {
            id: id.clone(),
            source,
        })?;
        let path = self.dir.join(id.file_name());
        let tmp = self.dir.join(format!(".{}.tmp", id.as_str()));
        let write = async {
            tokio::fs::create_dir_all(&self.dir).await?;
            tokio::fs::write(&tmp, &bytes).await?;
            tokio::fs::rename(&tmp, &path).await
        };
        if let Err(source) = write.await {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(SaveError::Write { path, source });
        }
        Ok(id)
    }

    /// Every stored report's summary, newest first. A file that is not a
    /// report document is skipped with a WARN naming it, never served. The
    /// directory is read and parsed on the blocking pool: at the caps it is
    /// up to 500 files of 256 KiB, too much to parse on an RPC thread.
    pub async fn summaries(&self) -> std::io::Result<Vec<Summary>> {
        let dir = self.dir.clone();
        tokio::task::spawn_blocking(move || summaries_in(&dir))
            .await
            .map_err(std::io::Error::other)?
    }

    /// The report `id`'s whole document, or `None` when there is none.
    pub async fn read(&self, id: &ReportId) -> std::io::Result<Option<StoredReport>> {
        let path = self.dir.join(id.file_name());
        let document = match tokio::fs::read(&path).await {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        let summary: Summary = serde_json::from_slice(&document)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        Ok(Some(StoredReport {
            owner: summary.owner,
            document,
        }))
    }

    /// Deletes the report `id`; `false` when there was none.
    pub async fn delete(&self, id: &ReportId) -> std::io::Result<bool> {
        match tokio::fs::remove_file(self.dir.join(id.file_name())).await {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Deletes every report received longer than the retention window before
    /// `now`, logging and counting each. Returns how many went.
    pub async fn expire(&self, now: SystemTime) -> std::io::Result<u32> {
        let cutoff = wall_ms(now)
            .saturating_sub(u64::try_from(self.retention.as_millis()).unwrap_or(u64::MAX));
        let mut expired = 0u32;
        for summary in self.summaries().await? {
            if summary.received_wall_ms >= cutoff {
                continue;
            }
            match self.delete(&summary.id).await {
                Ok(true) => {
                    expired = expired.saturating_add(1);
                    metrics::counter!("pharos_ui_reports_expired_total").increment(1);
                    tracing::info!(
                        report_id = %summary.id,
                        user = %summary.owner.0,
                        received_wall_ms = summary.received_wall_ms,
                        retention_days = self.retention.as_secs() / 86_400,
                        "client report expired"
                    );
                }
                Ok(false) => {}
                Err(e) => tracing::warn!(
                    report_id = %summary.id,
                    error = %e,
                    "client report expiry failed"
                ),
            }
        }
        Ok(expired)
    }
}

fn summaries_in(dir: &Path) -> std::io::Result<Vec<Summary>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut out = Vec::new();
    for entry in entries {
        let entry = entry?;
        // A symlink or directory named like a report is not one of ours.
        if !entry.file_type()?.is_file() {
            continue;
        }
        let path = entry.path();
        let Some(id) = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_suffix(".json"))
            .and_then(ReportId::parse)
        else {
            continue;
        };
        match read_summary(&path) {
            Ok(summary) if summary.id == id => out.push(summary),
            Ok(summary) => tracing::warn!(
                path = %path.display(),
                inner = %summary.id,
                "client report skipped: id inside differs from its file name"
            ),
            Err(e) => tracing::warn!(
                path = %path.display(),
                error = %e,
                "client report skipped: unreadable"
            ),
        }
    }
    out.sort_by_key(|s| std::cmp::Reverse(s.received_wall_ms));
    Ok(out)
}

fn read_summary(path: &Path) -> std::io::Result<Summary> {
    let bytes = std::fs::read(path)?;
    let mut summary: Summary = serde_json::from_slice(&bytes)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    summary.bytes = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    Ok(summary)
}

fn wall_ms(now: SystemTime) -> u64 {
    now.duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn report(owner: UserId) -> NewReport {
        NewReport {
            owner,
            user_name: "ali".into(),
            device: Some("pharos-native-1".into()),
            note: "subtitles vanished".into(),
            trace_ids: vec!["4bf92f3577b34da6a3ce929d0e0e4736".into()],
            bundle: serde_json::json!({"route": "/play/42"}),
        }
    }

    fn user() -> UserId {
        UserId(uuid::Uuid::new_v4())
    }

    #[test]
    fn ids_parse_only_in_their_own_shape() {
        let id = ReportId::random().unwrap();
        assert_eq!(ReportId::parse(id.as_str()), Some(id));
        for bad in [
            "",
            "ABCD",
            "ABCD-EFG",
            "abcd-efgh",
            "ABCD-EFGI",
            "../ABC-DEFG",
            "ABCD-EFGH.json",
        ] {
            assert_eq!(ReportId::parse(bad), None, "{bad:?} must not parse");
        }
    }

    #[tokio::test]
    async fn a_saved_report_reads_back_lists_and_deletes() {
        let dir = tempfile::tempdir().unwrap();
        let store = ReportStore::new(dir.path().join("reports"), DEFAULT_RETENTION_DAYS);
        let owner = user();
        let id = store.save(report(owner), SystemTime::now()).await.unwrap();

        let stored = store.read(&id).await.unwrap().unwrap();
        assert_eq!(stored.owner, owner);
        let doc: serde_json::Value = serde_json::from_slice(&stored.document).unwrap();
        assert_eq!(doc["note"], "subtitles vanished");
        assert_eq!(doc["bundle"]["route"], "/play/42");
        assert!(doc["receivedAt"].as_str().unwrap().ends_with('Z'));

        let list = store.summaries().await.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, id);
        assert!(list[0].bytes > 0);

        assert!(store.delete(&id).await.unwrap());
        assert!(!store.delete(&id).await.unwrap());
        assert!(store.read(&id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn one_user_is_capped_and_others_are_not() {
        let dir = tempfile::tempdir().unwrap();
        let store = ReportStore::new(dir.path().to_path_buf(), DEFAULT_RETENTION_DAYS);
        let owner = user();
        for _ in 0..MAX_REPORTS_PER_USER {
            store.save(report(owner), SystemTime::now()).await.unwrap();
        }
        let err = store
            .save(report(owner), SystemTime::now())
            .await
            .unwrap_err();
        assert!(matches!(err, SaveError::UserFull { held, .. } if held == MAX_REPORTS_PER_USER));
        assert_eq!(err.outcome(), "user_full");
        store.save(report(user()), SystemTime::now()).await.unwrap();
    }

    /// Disarm check: without the `is_file` filter the report linked in from
    /// outside the directory is listed.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_symlink_named_like_a_report_is_not_listed() {
        let dir = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let store = ReportStore::new(dir.path().to_path_buf(), DEFAULT_RETENTION_DAYS);
        let other = ReportStore::new(elsewhere.path().to_path_buf(), DEFAULT_RETENTION_DAYS);
        let outside = other.save(report(user()), SystemTime::now()).await.unwrap();
        let name = format!("{outside}.json");
        std::os::unix::fs::symlink(elsewhere.path().join(&name), dir.path().join(&name)).unwrap();
        let listed = store.summaries().await.unwrap();
        assert!(listed.is_empty(), "{listed:?}");
    }

    /// Disarm check: with the cutoff comparison inverted the fresh report
    /// goes and the old one stays.
    #[tokio::test]
    async fn expiry_removes_only_reports_past_retention() {
        let dir = tempfile::tempdir().unwrap();
        let store = ReportStore::new(dir.path().to_path_buf(), 30);
        let now = SystemTime::now();
        let old = store
            .save(report(user()), now - Duration::from_secs(31 * 86_400))
            .await
            .unwrap();
        let fresh = store
            .save(report(user()), now - Duration::from_secs(29 * 86_400))
            .await
            .unwrap();
        assert_eq!(store.expire(now).await.unwrap(), 1);
        assert!(store.read(&old).await.unwrap().is_none());
        assert!(store.read(&fresh).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn a_stray_file_is_never_listed() {
        let dir = tempfile::tempdir().unwrap();
        let store = ReportStore::new(dir.path().to_path_buf(), 30);
        std::fs::write(dir.path().join("ABCD-EFGH.json"), b"not json").unwrap();
        std::fs::write(dir.path().join("notes.txt"), b"x").unwrap();
        assert!(store.summaries().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_missing_dir_lists_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let store = ReportStore::new(dir.path().join("absent"), 30);
        assert!(store.summaries().await.unwrap().is_empty());
    }
}
