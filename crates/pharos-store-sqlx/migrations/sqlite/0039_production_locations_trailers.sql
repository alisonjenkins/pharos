-- T67: persist MediaMetadata.production_locations + trailers on media_items
-- so /Items detail can serve Jellyfin ProductionLocations + RemoteTrailers.
-- Both nullable JSON-array strings (see string_list_json); older rows +
-- items whose NFO carries no <country>/<trailer> report empty.
ALTER TABLE media_items ADD COLUMN production_locations_json TEXT;
ALTER TABLE media_items ADD COLUMN trailers_json TEXT;
