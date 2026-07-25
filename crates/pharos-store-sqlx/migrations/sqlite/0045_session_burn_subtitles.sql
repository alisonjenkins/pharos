-- Which subtitle streams must be BURNED for the client that negotiated this
-- session, resolved from the SubtitleProfiles it declared at PlaybackInfo.
--
-- The client's profile is visible on that one request only; the segment URLs it
-- later fetches carry a bare SubtitleStreamIndex with no indication of whether
-- the client can render that track itself. Without this the segment handler had
-- to assume every requested subtitle needed burning, so a browser that renders
-- subrip natively still got a filter graph on every segment.
--
-- Default '[]' — an empty set means "burn no TEXT track", which is correct for
-- every client that renders text itself. Image subtitles are unaffected: they
-- have no text rendition to fall back to and burn regardless of this column.
ALTER TABLE transcode_sessions
  ADD COLUMN burn_subtitle_indices_json TEXT NOT NULL DEFAULT '[]';
