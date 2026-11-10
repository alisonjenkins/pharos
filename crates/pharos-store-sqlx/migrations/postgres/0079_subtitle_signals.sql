-- Spec 028 (B1202): each item's subtitle signal pass, so a new style director
-- re-directs from the stored measures instead of decoding the audio again.
-- source_mtime/source_size are the video file's signature and audio_stream the
-- stream analysed (NULL: the file has no audio); a mismatch, or an older
-- signals_version, means the pass runs again. signals_json is the line windows
-- with their measures, opaque to the store.
CREATE TABLE IF NOT EXISTS subtitle_signals (
    item_id         BIGINT NOT NULL PRIMARY KEY REFERENCES media_items(id) ON DELETE CASCADE,
    source_mtime    BIGINT NOT NULL,
    source_size     BIGINT NOT NULL,
    audio_stream    BIGINT,
    signals_version BIGINT NOT NULL,
    signals_json    TEXT NOT NULL,
    measured_at     BIGINT NOT NULL
);
