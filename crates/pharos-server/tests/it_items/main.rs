//! Consolidated integration-test binary: one link instead of 23; each `mod` is a former tests/*.rs.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod direct_bool_filters_audit;
mod filters_audit;
mod genres_filter_audit;
mod has_subtitles_audit;
mod include_types_case_audit;
mod item_path_field;
mod item_sort_semantics;
mod items_audiobook_kind;
mod items_language_filters;
mod items_query_golden;
mod jellyfin_fts_facets;
mod jellyfin_items;
mod jellyfin_latest;
mod jellyfin_music_browse;
mod jellyfin_search;
mod name_prefix_audit;
mod pagination_audit;
mod parent_id_audit;
mod random_sort_seed;
mod resolution_filter_audit;
mod sort_audit;
mod type_filter_audit;
mod unicode_search_audit;
