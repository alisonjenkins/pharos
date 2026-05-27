//! Recursive media filesystem scan. Generic over `Prober` (V12).
//! Walk lives in `spawn_blocking` — never parks async runtime (V5).

use pharos_core::{
    DomainError, DomainResult, MediaItem, MediaKind, MediaStore, Prober, Scanner, SeriesInfo,
};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use xxhash_rust::xxh3::xxh3_64;

pub const DEFAULT_EXTENSIONS: &[&str] = &[
    "mkv", "mp4", "mov", "avi", "webm", "m4v", "flac", "mp3", "opus", "m4a", "ogg", "wav",
];

/// SIMD-accelerated stable ID for a path. xxh3_64 hashes UTF-8 bytes,
/// then masks to 63 bits so the value always survives the
/// `u64 -> i64` conversion the sqlite store does on insert. (Half of
/// real xxh3_64 outputs exceed i64::MAX; without the mask roughly
/// half the library hits a silent "conflict" on import.) Keyspace
/// stays 2^63, which still puts collisions out of reach for any
/// realistic library size.
pub fn stable_id(path: &Path) -> u64 {
    xxh3_64(path.to_string_lossy().as_bytes()) & 0x7FFFFFFFFFFFFFFF
}

#[derive(Debug, Clone)]
pub struct FsScanner<P: Prober> {
    prober: P,
    extensions: HashSet<String>,
}

impl<P: Prober> FsScanner<P> {
    pub fn new(prober: P) -> Self {
        Self {
            prober,
            extensions: DEFAULT_EXTENSIONS
                .iter()
                .map(|s| (*s).to_string())
                .collect(),
        }
    }

    pub fn with_extensions(prober: P, exts: impl IntoIterator<Item = String>) -> Self {
        Self {
            prober,
            extensions: exts.into_iter().collect(),
        }
    }

    /// Scan and push items into the given store. Streaming variant — avoids
    /// holding the entire library in memory. V10 atomicity holds per `put`.
    #[tracing::instrument(skip(self, store), fields(root = %root.display()))]
    pub async fn scan_into<S: MediaStore>(&self, root: &Path, store: &S) -> DomainResult<usize> {
        let paths = walk(root.to_path_buf(), self.extensions.clone()).await?;
        let mut n = 0;
        for p in paths {
            if let Some(item) = self.probe_one(p).await {
                store.put(item).await?;
                n += 1;
            }
        }
        Ok(n)
    }

    async fn probe_one(&self, path: PathBuf) -> Option<MediaItem> {
        match self.prober.probe(&path).await {
            Ok(info) => {
                let title = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("unknown")
                    .to_string();
                // Stat the file so MediaProbe.size_bytes is set even when
                // ffprobe didn't report `format.size` (some containers).
                let mut probe = info.probe;
                if probe.size_bytes.is_none() {
                    if let Ok(meta) = tokio::fs::metadata(&path).await {
                        probe.size_bytes = Some(meta.len());
                    }
                }
                // Promote video-kind items to Episode when the path
                // looks like a TV layout. Audio stays as classified.
                let kind = if matches!(info.kind, MediaKind::Movie) && is_episode_path(&path) {
                    MediaKind::Episode
                } else {
                    info.kind
                };
                let series = if matches!(kind, MediaKind::Episode) {
                    parse_series_info(&path)
                } else {
                    None
                };
                Some(MediaItem {
                    id: stable_id(&path),
                    path,
                    title,
                    kind,
                    probe,
                    series,
                    // Let the store-side `now_secs` populate. Passing
                    // None preserves the original `created_at` on
                    // rescan via the COALESCE in put().
                    created_at: None,
                })
            }
            Err(err) => {
                tracing::warn!(path = %path.display(), error = %err, "probe failed, skipping");
                None
            }
        }
    }
}

impl<P: Prober + Clone + 'static> Scanner for FsScanner<P> {
    #[tracing::instrument(skip(self), fields(root = %root.display()))]
    async fn scan(&self, root: &Path) -> DomainResult<Vec<MediaItem>> {
        let paths = walk(root.to_path_buf(), self.extensions.clone()).await?;
        let mut items = Vec::with_capacity(paths.len());
        for p in paths {
            if let Some(item) = self.probe_one(p).await {
                items.push(item);
            }
        }
        Ok(items)
    }
}

/// Heuristic: does `path` look like a TV episode?
///
/// We accept either signal:
/// - filename contains an `SxxEyy` token (case-insensitive, with any
///   non-letter separator before the `S` to avoid matching mid-word
///   IDs like "GS9E2-clip"); or
/// - any parent directory is named `Season N`, `Season NN`, `S<NN>`,
///   `Specials`, or `Season 0` (the Plex/Jellyfin layout convention).
///
/// Path-only — no probe required. Files in a "Movies/" tree never hit
/// either signal and stay Movie.
pub fn is_episode_path(path: &Path) -> bool {
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    if has_sxxeyy_token(name) {
        return true;
    }
    for component in path.components() {
        let comp = component.as_os_str().to_string_lossy();
        if looks_like_season_dir(&comp) {
            return true;
        }
    }
    false
}

fn has_sxxeyy_token(name: &str) -> bool {
    let bytes = name.as_bytes();
    let lower: Vec<u8> = bytes.iter().map(|b| b.to_ascii_lowercase()).collect();
    let mut i = 0;
    while i + 5 < lower.len() {
        // boundary: start or non-letter before 's'
        let at_boundary = i == 0 || !lower[i - 1].is_ascii_alphabetic();
        if at_boundary
            && lower[i] == b's'
            && lower[i + 1].is_ascii_digit()
        {
            // optional second season digit
            let mut j = i + 2;
            if j < lower.len() && lower[j].is_ascii_digit() {
                j += 1;
            }
            if j < lower.len() && lower[j] == b'e' {
                let mut k = j + 1;
                if k < lower.len() && lower[k].is_ascii_digit() {
                    k += 1;
                    if k < lower.len() && lower[k].is_ascii_digit() {
                        return true;
                    }
                    return true;
                }
            }
        }
        i += 1;
    }
    false
}

/// Extract `SeriesInfo { series_name, season_number, episode_number }`
/// from a TV-layout path. Heuristic:
/// - series_name = the closest ancestor directory of `path` that is
///   *not* a "Season N" / "S01" / "Specials" / a configured media
///   root token. Falls back to the immediate parent directory name
///   when nothing else fits.
/// - season_number = parsed from a "Season N" / "S<NN>" parent dir
///   if present, or from the `SxxEyy` token in the filename.
/// - episode_number = parsed from the `SxxEyy` token in the filename.
///
/// Returns `None` when `path` has no parent — pathological case.
pub fn parse_series_info(path: &Path) -> Option<SeriesInfo> {
    let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
    let (filename_season, episode) = parse_sxxeyy(name);

    // Walk parents from closest to farthest.
    let mut parents: Vec<&str> = path
        .ancestors()
        .skip(1)
        .filter_map(|p| p.file_name().and_then(|s| s.to_str()))
        .collect();

    let mut season_from_dir: Option<u32> = None;
    let mut series_name: Option<String> = None;

    for parent in parents.drain(..) {
        if let Some(n) = parse_season_dir(parent) {
            season_from_dir = season_from_dir.or(Some(n));
            continue;
        }
        if parent.eq_ignore_ascii_case("specials") {
            season_from_dir = season_from_dir.or(Some(0));
            continue;
        }
        // First non-season ancestor wins as the series name.
        if series_name.is_none() {
            series_name = Some(parent.to_string());
            break;
        }
    }

    let series_name = series_name?;
    let season_number = season_from_dir.or(filename_season);
    Some(SeriesInfo {
        series_name,
        season_number,
        episode_number: episode,
    })
}

/// Return the (season, episode) numbers when `name` carries an
/// `SxxEyy` token at any letter-boundary. `None` if absent.
fn parse_sxxeyy(name: &str) -> (Option<u32>, Option<u32>) {
    let lower: Vec<u8> = name.bytes().map(|b| b.to_ascii_lowercase()).collect();
    let mut i = 0;
    while i + 5 < lower.len() {
        let at_boundary = i == 0 || !lower[i - 1].is_ascii_alphabetic();
        if at_boundary && lower[i] == b's' && lower[i + 1].is_ascii_digit() {
            // collect season digits
            let s_start = i + 1;
            let mut s_end = s_start + 1;
            while s_end < lower.len() && lower[s_end].is_ascii_digit() {
                s_end += 1;
            }
            if s_end < lower.len() && lower[s_end] == b'e' {
                let e_start = s_end + 1;
                let mut e_end = e_start;
                while e_end < lower.len() && lower[e_end].is_ascii_digit() {
                    e_end += 1;
                }
                if e_end > e_start {
                    let season = std::str::from_utf8(&lower[s_start..s_end])
                        .ok()
                        .and_then(|s| s.parse().ok());
                    let episode = std::str::from_utf8(&lower[e_start..e_end])
                        .ok()
                        .and_then(|s| s.parse().ok());
                    return (season, episode);
                }
            }
        }
        i += 1;
    }
    (None, None)
}

/// Parse a "Season N" / "Season NN" / "S01" / "S1" directory name → N.
fn parse_season_dir(name: &str) -> Option<u32> {
    let n = name.trim();
    if let Some(rest) = n.to_ascii_lowercase().strip_prefix("season ") {
        return rest.trim().parse().ok();
    }
    let lower = n.to_ascii_lowercase();
    if lower.starts_with('s')
        && lower.len() >= 2
        && lower.len() <= 4
        && lower[1..].chars().all(|c| c.is_ascii_digit())
    {
        return lower[1..].parse().ok();
    }
    None
}

fn looks_like_season_dir(name: &str) -> bool {
    let n = name.trim();
    if n.eq_ignore_ascii_case("specials") {
        return true;
    }
    let lower = n.to_ascii_lowercase();
    // "Season 1", "Season 02", "Season 10"
    if let Some(rest) = lower.strip_prefix("season ") {
        return rest.trim().chars().all(|c| c.is_ascii_digit())
            && !rest.trim().is_empty();
    }
    // Compact "S01", "S1" — only when whole component is that form so
    // we don't grab a file named "S01E03.mkv" (handled by SxxEyy path).
    if lower.starts_with('s')
        && lower.len() >= 2
        && lower.len() <= 4
        && lower[1..].chars().all(|c| c.is_ascii_digit())
    {
        return true;
    }
    false
}

/// Recursive walk inside `spawn_blocking`. Returns paths of files whose
/// lowercased extension is in `exts`.
async fn walk(root: PathBuf, exts: HashSet<String>) -> DomainResult<Vec<PathBuf>> {
    tokio::task::spawn_blocking(move || -> DomainResult<Vec<PathBuf>> {
        let mut out = Vec::new();
        for entry in walkdir::WalkDir::new(&root).follow_links(false) {
            let e = entry.map_err(|err| DomainError::Backend(err.to_string()))?;
            if !e.file_type().is_file() {
                continue;
            }
            let lower = e
                .path()
                .extension()
                .and_then(|s| s.to_str())
                .map(|s| s.to_ascii_lowercase());
            if let Some(ext) = lower {
                if exts.contains(&ext) {
                    out.push(e.into_path());
                }
            }
        }
        Ok(out)
    })
    .await
    .map_err(|e| DomainError::Backend(format!("walk join: {e}")))?
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use pharos_core::{MediaKind, ProbeInfo};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tempfile::TempDir;

    #[derive(Clone, Default)]
    struct FakeProber {
        calls: Arc<AtomicUsize>,
        force_fail_for: Option<String>,
    }

    impl Prober for FakeProber {
        async fn probe(&self, path: &Path) -> DomainResult<ProbeInfo> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if let Some(needle) = &self.force_fail_for {
                if path.to_string_lossy().contains(needle.as_str()) {
                    return Err(DomainError::Backend("forced fail".into()));
                }
            }
            let kind = match path.extension().and_then(|s| s.to_str()) {
                Some("flac") | Some("mp3") | Some("opus") | Some("m4a") | Some("ogg")
                | Some("wav") => MediaKind::Audio,
                _ => MediaKind::Movie,
            };
            Ok(ProbeInfo {
                kind,
                probe: Default::default(),
            })
        }
    }

    async fn touch(dir: &Path, name: &str) {
        let p = dir.join(name);
        if let Some(parent) = p.parent() {
            tokio::fs::create_dir_all(parent).await.unwrap();
        }
        tokio::fs::write(&p, b"").await.unwrap();
    }

    #[tokio::test]
    async fn finds_recognized_extensions_and_skips_others() {
        let td = TempDir::new().unwrap();
        touch(td.path(), "movie.mkv").await;
        touch(td.path(), "song.flac").await;
        touch(td.path(), "notes.txt").await;
        let s = FsScanner::new(FakeProber::default());
        let items = s.scan(td.path()).await.unwrap();
        let titles: Vec<_> = items.iter().map(|i| i.title.clone()).collect();
        assert_eq!(items.len(), 2, "got {titles:?}");
        let kinds: HashSet<MediaKind> = items.iter().map(|i| i.kind).collect();
        assert!(kinds.contains(&MediaKind::Movie));
        assert!(kinds.contains(&MediaKind::Audio));
    }

    #[tokio::test]
    async fn empty_dir_returns_empty() {
        let td = TempDir::new().unwrap();
        let s = FsScanner::new(FakeProber::default());
        assert!(s.scan(td.path()).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn recurses_subdirs() {
        let td = TempDir::new().unwrap();
        touch(td.path(), "top.mkv").await;
        touch(td.path(), "show/season1/ep1.mkv").await;
        touch(td.path(), "show/season1/ep2.mkv").await;
        touch(td.path(), "music/album/track.flac").await;
        let s = FsScanner::new(FakeProber::default());
        let items = s.scan(td.path()).await.unwrap();
        assert_eq!(items.len(), 4);
    }

    #[tokio::test]
    async fn probe_failure_is_logged_and_skipped() {
        let td = TempDir::new().unwrap();
        touch(td.path(), "good.mkv").await;
        touch(td.path(), "bad.mkv").await;
        let prober = FakeProber {
            calls: Arc::new(AtomicUsize::new(0)),
            force_fail_for: Some("bad".into()),
        };
        let s = FsScanner::new(prober.clone());
        let items = s.scan(td.path()).await.unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, "good");
        assert_eq!(prober.calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn promotes_to_episode_when_path_matches_sxxeyy() {
        let td = TempDir::new().unwrap();
        touch(td.path(), "Show/Season 1/Show.S01E02.mkv").await;
        let s = FsScanner::new(FakeProber::default());
        let items = s.scan(td.path()).await.unwrap();
        assert_eq!(items.len(), 1);
        assert!(matches!(items[0].kind, MediaKind::Episode));
    }

    #[tokio::test]
    async fn movies_path_stays_movie() {
        let td = TempDir::new().unwrap();
        touch(td.path(), "Movies/Big Buck Bunny (2008).mkv").await;
        let s = FsScanner::new(FakeProber::default());
        let items = s.scan(td.path()).await.unwrap();
        assert!(matches!(items[0].kind, MediaKind::Movie));
    }

    #[test]
    fn sxxeyy_token_recognises_common_patterns() {
        assert!(has_sxxeyy_token("Show.S01E02.mkv"));
        assert!(has_sxxeyy_token("show s1e1.mp4"));
        assert!(has_sxxeyy_token("Series_S12E07_HDTV.mkv"));
        assert!(!has_sxxeyy_token("classS5English.mp4")); // mid-word "S5" rejected
        assert!(!has_sxxeyy_token("Movie 2024.mkv"));
    }

    #[test]
    fn parses_series_info_from_canonical_layout() {
        let p = Path::new("/srv/media/TV/My Show/Season 2/My.Show.S02E07.mkv");
        let info = parse_series_info(p).expect("series info");
        assert_eq!(info.series_name, "My Show");
        assert_eq!(info.season_number, Some(2));
        assert_eq!(info.episode_number, Some(7));
    }

    #[test]
    fn parses_series_info_with_compact_season_dir() {
        let p = Path::new("/m/Another Show/S03/file.s03e01.mkv");
        let info = parse_series_info(p).expect("series info");
        assert_eq!(info.series_name, "Another Show");
        assert_eq!(info.season_number, Some(3));
        assert_eq!(info.episode_number, Some(1));
    }

    #[test]
    fn parses_series_info_specials_is_season_zero() {
        let p = Path::new("/m/Some Show/Specials/Some.Show.S00E04.mkv");
        let info = parse_series_info(p).expect("series info");
        assert_eq!(info.series_name, "Some Show");
        assert_eq!(info.season_number, Some(0));
        assert_eq!(info.episode_number, Some(4));
    }

    #[test]
    fn series_info_falls_back_to_filename_season_when_no_season_dir() {
        let p = Path::new("/m/Show Without Season Dir/Show.S05E11.mkv");
        let info = parse_series_info(p).expect("series info");
        assert_eq!(info.series_name, "Show Without Season Dir");
        assert_eq!(info.season_number, Some(5));
        assert_eq!(info.episode_number, Some(11));
    }

    #[test]
    fn season_dir_patterns_recognised() {
        assert!(looks_like_season_dir("Season 1"));
        assert!(looks_like_season_dir("season 02"));
        assert!(looks_like_season_dir("S01"));
        assert!(looks_like_season_dir("Specials"));
        assert!(!looks_like_season_dir("Movies"));
        assert!(!looks_like_season_dir("Some Movie 2024"));
    }

    #[tokio::test]
    async fn stable_id_is_deterministic() {
        let a = stable_id(Path::new("/srv/media/movie.mkv"));
        let b = stable_id(Path::new("/srv/media/movie.mkv"));
        assert_eq!(a, b);
        let c = stable_id(Path::new("/srv/media/other.mkv"));
        assert_ne!(a, c);
    }
}
