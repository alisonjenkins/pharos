//! Consolidated integration-test binary: one link instead of 23; each `mod` is a former tests/*.rs.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod image_spans;
mod jellyfin_add_library;
mod jellyfin_channels;
mod jellyfin_chapter_image_route;
mod jellyfin_collections_entity;
mod jellyfin_genres_entity;
mod jellyfin_grouping_options;
mod jellyfin_item_ancestors;
mod jellyfin_item_collections;
mod jellyfin_item_images_list;
mod jellyfin_libraries_typed;
mod jellyfin_live_tv;
mod jellyfin_media_segments;
mod jellyfin_people_entity;
mod jellyfin_persons_filtering;
mod jellyfin_season_images;
mod jellyfin_segment_bounds;
mod jellyfin_studios_entity;
mod jellyfin_tags_entity;
mod jellyfin_thumb_falls_back_to_backdrop;
mod jellyfin_user_configuration;
mod jellyfin_user_data;
mod jellyfin_wire_id_routes;
