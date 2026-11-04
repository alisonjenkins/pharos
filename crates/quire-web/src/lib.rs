//! Draw a [`quire`] book in a browser.
//!
//! [`View::mount`] attaches an open shadow root to an element the caller
//! sizes, so the book's stylesheets cannot reach the page and the page's
//! cannot reach the book. A section is built node by node from
//! [`quire::Section::root`] (never through `innerHTML`, so text offsets match
//! [`quire::Section::text`]) and flowed into CSS columns, one column per page.
//! The browser does the layout.
//!
//! The view reports what the reader asks for (next, previous, a link
//! followed, a resize) through [`View::on_action`] and leaves the decision to
//! the caller, so it carries no key bindings and no UI framework.

#![forbid(unsafe_code)]

mod layout;
mod settings;
mod view;

pub use layout::Columns;
pub use settings::{Font, Settings, Theme};
pub use view::{Action, Start, View, ViewError, WeakView};
