//! The `media_items` picture check columns, shared by both stores so the
//! validity rule cannot drift between them (same rule as `audio_analysis_row`).

use pharos_core::{PictureArea, PictureCheck};

/// The picture check columns of one row, as read.
pub(crate) struct PictureColumns {
    pub version: Option<i64>,
    pub x: Option<i64>,
    pub y: Option<i64>,
    pub width: Option<i64>,
    pub height: Option<i64>,
    /// The file signature the check read.
    pub checked_mtime: Option<i64>,
    pub checked_size: Option<i64>,
    /// The row's scan signature (`file_mtime`, `file_size_seen`).
    pub file_mtime: Option<i64>,
    pub file_size: Option<i64>,
}

/// The stored check, or `None` when there is none or it was taken from a
/// different version of the file than the one the last scan saw. A row the
/// scanner has never stamped has nothing to compare with, so its check stands.
pub(crate) fn decode(c: PictureColumns) -> Option<PictureCheck> {
    let version = u32::try_from(c.version?).ok()?;
    if c.file_mtime.is_some() && (c.checked_mtime, c.checked_size) != (c.file_mtime, c.file_size) {
        return None;
    }
    let dim = |v: Option<i64>| v.and_then(|v| u32::try_from(v).ok());
    let area = match (dim(c.x), dim(c.y), dim(c.width), dim(c.height)) {
        (Some(x), Some(y), Some(width), Some(height)) if width > 0 && height > 0 => {
            Some(PictureArea {
                x,
                y,
                width,
                height,
            })
        }
        _ => None,
    };
    Some(PictureCheck { version, area })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cols(checked: (i64, i64), file: Option<(i64, i64)>) -> PictureColumns {
        PictureColumns {
            version: Some(1),
            x: Some(242),
            y: Some(0),
            width: Some(1436),
            height: Some(1080),
            checked_mtime: Some(checked.0),
            checked_size: Some(checked.1),
            file_mtime: file.map(|f| f.0),
            file_size: file.map(|f| f.1),
        }
    }

    #[test]
    fn a_check_of_the_file_the_scan_saw_is_read_back() {
        assert_eq!(
            decode(cols((10, 500), Some((10, 500)))),
            Some(PictureCheck {
                version: 1,
                area: Some(PictureArea {
                    x: 242,
                    y: 0,
                    width: 1436,
                    height: 1080
                })
            })
        );
    }

    #[test]
    fn a_replaced_file_reads_as_unchecked() {
        assert_eq!(decode(cols((10, 500), Some((11, 500)))), None, "new mtime");
        assert_eq!(decode(cols((10, 500), Some((10, 501)))), None, "new size");
    }

    #[test]
    fn a_row_never_stamped_by_the_scanner_keeps_its_check() {
        assert!(decode(cols((10, 500), None)).is_some());
    }

    #[test]
    fn no_version_means_never_checked() {
        let mut c = cols((10, 500), Some((10, 500)));
        c.version = None;
        assert_eq!(decode(c), None);
    }

    #[test]
    fn a_version_with_no_width_is_a_check_that_found_no_bars() {
        let mut c = cols((10, 500), Some((10, 500)));
        c.width = None;
        c.x = None;
        c.y = None;
        c.height = None;
        assert_eq!(
            decode(c),
            Some(PictureCheck {
                version: 1,
                area: None
            })
        );
    }
}
