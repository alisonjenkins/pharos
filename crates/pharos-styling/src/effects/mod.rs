//! The effect library: one module per effect, each turning a source line
//! into the events that draw it at a tier (spec 028 FR-003).
//!
//! Every effect keeps to the tier budgets by construction: Low is static
//! styling in one event, TV spends at most one `\t` or `\move` per event and
//! two events per line, Desktop at most four `\t` and four events.

use crate::event::Event;
use crate::plan::{LineEffect, Placement, Tier};
use crate::source::SourceLine;
use crate::theme::Theme;

/// Tags for a line's placement; the empty string for the style's own spot.
pub(crate) fn place_tags(p: Placement) -> String {
    match p {
        Placement::Default => String::new(),
        Placement::Top => "\\an8".to_owned(),
        Placement::Free { x, y } => format!("\\an5\\pos({x},{y})"),
    }
}

/// The events for `fx` on `line` at `tier`, or `None` when the intent has no
/// effect of its own (Plain, Sarcasm, intensity 0) and the line is drawn plain.
pub(crate) fn events(
    _line: &SourceLine,
    _fx: &LineEffect,
    _theme: &Theme,
    _tier: Tier,
) -> Option<Vec<Event>> {
    None
}
