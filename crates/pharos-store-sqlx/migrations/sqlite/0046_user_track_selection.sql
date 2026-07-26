-- The audio and subtitle track a user last played an item with, so resuming
-- returns to it. Aliens leads with three Ukrainian tracks; a viewer who
-- switched to English had to switch again on every resume, because nothing
-- recorded that they had chosen.
--
-- NULL means "never chosen" — distinct from a chosen -1, which is subtitles
-- explicitly OFF and must survive a resume just as a chosen track does.
ALTER TABLE user_data ADD COLUMN audio_stream_index INTEGER;
ALTER TABLE user_data ADD COLUMN subtitle_stream_index INTEGER;
