//! Spec 024 — the `media_items` audio analysis columns, shared by both stores
//! so the validity rule cannot drift between them (V385).

use pharos_core::{AudioAnalysis, TempoSource};

/// The analysis columns of one row, as read.
pub(crate) struct AnalysisColumns<'a> {
    pub bpm_mille: Option<i64>,
    pub tempo_source: Option<&'a str>,
    pub energy_mille: Option<i64>,
    pub version: Option<i64>,
    /// The file signature the analysis read.
    pub analysed_mtime: Option<i64>,
    pub analysed_size: Option<i64>,
    /// The row's scan signature (`file_mtime`, `file_size_seen`).
    pub file_mtime: Option<i64>,
    pub file_size: Option<i64>,
}

/// The stored analysis, or `None` when there is none or it was taken from a
/// different version of the file than the one the last scan saw. A row the
/// scanner has never stamped (`file_mtime` NULL) has nothing to compare with,
/// so its analysis stands.
pub(crate) fn decode(c: AnalysisColumns<'_>) -> Option<AudioAnalysis> {
    let version = u32::try_from(c.version?).ok()?;
    if c.file_mtime.is_some() && (c.analysed_mtime, c.analysed_size) != (c.file_mtime, c.file_size)
    {
        return None;
    }
    Some(AudioAnalysis {
        bpm_mille: c.bpm_mille.and_then(|v| u32::try_from(v).ok()),
        tempo_source: c.tempo_source.and_then(TempoSource::from_label),
        energy_mille: c
            .energy_mille
            .and_then(|v| u16::try_from(v).ok())
            .map(|v| v.min(1000)),
        version,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]
    use super::*;

    fn cols(analysed: (i64, i64), file: Option<(i64, i64)>) -> AnalysisColumns<'static> {
        AnalysisColumns {
            bpm_mille: Some(128_000),
            tempo_source: Some("tag"),
            energy_mille: Some(700),
            version: Some(1),
            analysed_mtime: Some(analysed.0),
            analysed_size: Some(analysed.1),
            file_mtime: file.map(|f| f.0),
            file_size: file.map(|f| f.1),
        }
    }

    #[test]
    fn an_analysis_of_the_file_the_scan_saw_is_read_back() {
        let a = decode(cols((10, 500), Some((10, 500)))).expect("matching signature");
        assert_eq!(a.bpm(), Some(128.0));
        assert_eq!(a.tempo_source, Some(TempoSource::Tag));
        assert_eq!(a.energy(), Some(0.7));
        assert_eq!(a.version, 1);
    }

    /// V385 — a file replaced since it was analysed reads as unanalysed, so
    /// the sweep redoes it and no rule sees the old song's numbers.
    #[test]
    fn a_replaced_file_reads_as_unanalysed() {
        assert_eq!(decode(cols((10, 500), Some((11, 500)))), None, "new mtime");
        assert_eq!(decode(cols((10, 500), Some((10, 501)))), None, "new size");
    }

    #[test]
    fn a_row_never_stamped_by_the_scanner_keeps_its_analysis() {
        assert!(decode(cols((10, 500), None)).is_some());
    }

    #[test]
    fn no_version_means_never_analysed() {
        let mut c = cols((10, 500), Some((10, 500)));
        c.version = None;
        assert_eq!(decode(c), None);
    }
}
