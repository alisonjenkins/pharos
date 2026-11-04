-- Films and episodes matched online before enrichment fetched credits carry no
-- cast. Clearing their refresh time puts them back in front of the enrichment
-- sweep, which fills the cast in; items with any credit, and manual matches,
-- are left alone.
UPDATE media_items SET metadata_refreshed_at = NULL
WHERE kind IN ('movie', 'episode')
  AND match_source IN ('search', 'nfo_id')
  AND NOT EXISTS (SELECT 1 FROM item_people ip WHERE ip.item_id = media_items.id);
