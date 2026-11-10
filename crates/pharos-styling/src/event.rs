//! One `Dialogue:` line to emit.

use crate::source::SourceLine;
use crate::tags;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Event {
    pub layer: i32,
    /// Always a whole number of centiseconds, rounded inward, so the event
    /// never leaves its source line's range.
    pub start_ms: i64,
    pub end_ms: i64,
    pub style: String,
    /// The source line's index, so a reader can group events by source line.
    pub name: String,
    pub text: String,
}

impl Event {
    /// An event spanning `line`, rounded inward to centiseconds.
    pub fn on(line: &SourceLine, index: usize, style: &str, text: String) -> Event {
        Event {
            layer: line.layer.max(0),
            start_ms: (line.start_ms + 9).div_euclid(10) * 10,
            end_ms: line.end_ms.div_euclid(10) * 10,
            style: style.to_owned(),
            name: index.to_string(),
            text,
        }
    }

    pub fn on_layer(mut self, layer: i32) -> Event {
        self.layer = layer;
        self
    }

    /// False when rounding left no time to draw it.
    pub fn is_visible(&self) -> bool {
        self.end_ms > self.start_ms
    }

    /// Has any animation tag (`\t \move \fad \fade \k*`).
    pub fn is_animated(&self) -> bool {
        tags::scan_text(&self.text)
            .iter()
            .any(|t| tags::is_animation(&t.name))
    }

    /// The `Dialogue:` line.
    pub fn render(&self) -> String {
        format!(
            "Dialogue: {},{},{},{},{},0,0,0,,{}",
            self.layer,
            cs_time(self.start_ms),
            cs_time(self.end_ms),
            self.style,
            self.name,
            self.text
        )
    }
}

/// `H:MM:SS.cc`.
fn cs_time(ms: i64) -> String {
    let ms = ms.max(0);
    let cs = ms / 10 % 100;
    let s = ms / 1000 % 60;
    let m = ms / 60_000 % 60;
    let h = ms / 3_600_000;
    format!("{h}:{m:02}:{s:02}.{cs:02}")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::source::SourceFormat;

    fn line(start_ms: i64, end_ms: i64) -> SourceLine {
        SourceLine {
            start_ms,
            end_ms,
            text: "x".into(),
            raw: "x".into(),
            format: SourceFormat::Srt,
            style: "Default".into(),
            layer: 0,
            italic: false,
            typeset: false,
            karaoke: None,
            style_def: None,
            speaker: None,
        }
    }

    #[test]
    fn rounds_inward() {
        let e = Event::on(&line(1001, 2009), 3, "Default", "x".into());
        assert_eq!((e.start_ms, e.end_ms), (1010, 2000));
        assert_eq!(
            e.render(),
            "Dialogue: 0,0:00:01.01,0:00:02.00,Default,3,0,0,0,,x"
        );
    }

    #[test]
    fn tiny_lines_vanish() {
        assert!(!Event::on(&line(1001, 1005), 0, "Default", String::new()).is_visible());
    }

    #[test]
    fn formats_long_times() {
        assert_eq!(cs_time(3_723_450), "1:02:03.45");
    }

    #[test]
    fn animated_follows_the_tags() {
        let mk = |t: &str| Event::on(&line(0, 1000), 0, "Default", t.into());
        assert!(mk(r"{\fad(1,1)}x").is_animated());
        assert!(mk(r"{\kf10}x").is_animated());
        assert!(!mk(r"{\bord4\i1}x").is_animated());
    }
}
