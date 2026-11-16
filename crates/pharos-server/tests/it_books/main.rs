//! Consolidated integration-test binary: one link instead of 7; each `mod` is a former tests/*.rs.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod audiobook_embedded_cover;
mod book_cover;
mod book_end_to_end;
mod book_formats_fold;
mod book_offers_nothing_to_play;
mod book_opening;
mod book_read_progress;
mod book_slim;
mod course_lessons;
mod item_download;
mod library_watch_fallback;
mod opds;
mod remote_end_to_end;
mod rescan_held_path;
