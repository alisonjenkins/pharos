//! `X-Content-Type-Options: nosniff` on every response.
//!
//! pharos serves bytes whose type comes from the media file: embedded
//! attachments, subtitles, images. Without `nosniff` a browser may sniff a
//! body labelled one thing as HTML and run it on pharos's origin, where it
//! can read the signed-in user's token (B254, V194).

use actix_web::{http::header, middleware::DefaultHeaders};

/// Sets the header on any response that has not set it itself.
pub fn nosniff() -> DefaultHeaders {
    DefaultHeaders::new().add((header::X_CONTENT_TYPE_OPTIONS, "nosniff"))
}
