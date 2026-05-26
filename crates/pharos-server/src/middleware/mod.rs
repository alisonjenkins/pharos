//! HTTP middleware applied to the Jellyfin-compat surface.

pub mod path_case;
pub mod red_metrics;

pub use path_case::LowercasePath;
pub use red_metrics::RedMetrics;
