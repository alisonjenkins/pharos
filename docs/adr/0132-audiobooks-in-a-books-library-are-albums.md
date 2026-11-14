# ADR-0132: Audiobooks in a Books library are albums of songs

- **Status:** Accepted
- **Date:** 2026-11-14T11:37:15Z
- **Deciders:** the owner (chose an Audiobooks tab inside Books), Claude

## Context

- The owner wants audiobooks in Pharos, listed on an Audiobooks tab of the
  Books library, next to the Books grid and the Courses tab (ADR-0126).
- An audiobook on disk is one of two shapes. Invented examples:
  - one file with chapter marks: `Ada Field/Gadget Tales.m4b`, or an `.m4a`
    or `.mp3` that carries chapters;
  - a folder of audio files, one per part: `Ada Field/Widget Days/01.mp3`,
    `02.mp3`, ..., sometimes split into `CD1/`, `CD2/`.
- Before this change the scanner did not walk `.m4b`, and an `.mp3` under a
  Books root was stored as a loose song, with whatever album tag it had.
- Pharos already has a whole path for "audio files that play in order as one
  unit": music albums. An album is synthesised from the `album` tag of its
  `Audio` rows; its page lists the tracks in disc and track order; Play
  queues the rest of the album after the first track; each track has its
  own per-user resume position; embedded cover art is extracted.
- `MediaKind` has four variants. Adding a fifth means redoing the
  `specs/004-books/kind-decision-audit.md` audit of every `matches!` on it.

## Decision

We will store an audiobook as an album of `Audio` rows, and know it is an
audiobook by where it lives.

1. **Representation.** Every audio file of a book is an `Audio` row. The
   book is the row's album (`probe.album`), its author the album artist.
   - A single-file book is an album of one track. Its chapters are the
     file's own (`probe.chapters`, from ffprobe / libav), already stored.
   - A folder book's files are its tracks. Their track numbers are their
     place in natural order of their paths under the book folder, with no
     disc number, so the album's existing order is the book's. Each track
     is one chapter.
2. **Which files.** Only a file whose innermost library is a Books library
   (`CourseRoots::root_of`, as courses) is read as a book:
   - any `.m4b` is a single-file book;
   - an `.m4a` / `.mp3` (or other audio) with chapters, alone in its folder
     or directly in the root, is a single-file book;
   - the audio files of a folder are one book; a disc folder (`CD1`,
     `Disc 2`, `Part 3`) belongs to the book above it;
   - an audio file directly in the root with no chapters is no book, and is
     left out of the library.
3. **Title and author.** A single file's title is its album tag, else its
   title tag, else its cleaned file name. A folder book's title is its
   folder's name (cleaned of leading numbers), because the folder is the
   book and every track must agree on it to form one album. The author is
   the album-artist tag, else the artist tag, else the folder the book sits
   in when that is not the root (`Author/Title/`).
4. **No marker column, no migration.** "Is an audiobook" is derived at read
   time: an `Audio` row whose innermost library is a Books library
   (`AppState::is_audiobook`). The native API carries it as
   `ItemSummary.audiobook` (schemaVersion 48) on tracks and on album tiles,
   where an audiobook album also names its author and its whole length.
5. **Resume.** The existing per-user, per-item position is the resume
   point: a single file resumes in the file; a folder book resumes on the
   part last begun. An audiobook's track counts as finished only in its
   last minute (or past the played threshold, whichever leaves less), not
   at 90% (B1430, V1404).
6. **Player.** The music player is unchanged; while an audiobook plays it
   adds a row of speed (0.75 to 2.0x, kept per device and apart from video
   speed), a sleep timer (off, 15, 30, 60 min, end of chapter) and the
   chapter list (the file's chapters, or the book's parts) with
   jump-to-chapter.

## Consequences

- Playback, queueing, cover art, resume, played marks and the album page
  come from the music path with no new store query or migration.
- The decision is counted: `pharos_scan_audiobook_total{verdict,reason}`
  and the log line "scanner: audio in a books library classified".
- Albums are keyed by name across the server (`album_id_for`). A book with
  the same title as a music album, or two books with the same title, fold
  into one album. Accepted for now; a folder-keyed album id would fix it.
- An audiobook is an album, so a listing of every album with no parent (the
  Jellyfin `/Items?IncludeItemTypes=MusicAlbum` with no `ParentId`, the
  global search) shows audiobooks with music. A music library's own listing
  does not, as it is scoped to its root.
- A folder book's track numbers are fixed when each file is probed. A file
  added between existing ones by the watcher takes its place, but its later
  siblings keep their old numbers until they are rescanned.
- Existing audio rows under a Books root are reclassified only when their
  file changes or a full rescan re-probes them; no probe schema bump was
  made, to avoid re-probing every video.
- Narrators are not read: the probe keeps no `composer` / `narrator` tag.

## Alternatives considered

- **A fifth `MediaKind::Audiobook`.** Every kind match, wire enum, the store
  and the clients would need a new arm, and the music player and album page
  would have to learn a second kind of song. Rejected for its size, as
  `MediaKind::Lesson` was for courses.
- **Chapters as stored rows, a book as one item.** A folder book would need a
  new table of parts, a player that walks files inside one item, and its own
  resume. Rejected: the album path already plays files in sequence.
- **A `match_source = 'audiobook'` marker, as courses have.** Needs a stamp
  after each put on both scan paths and still depends on the Books roots;
  the roots alone answer the question. Rejected.
- **A new `Audiobooks` library kind.** The owner keeps audiobooks with the
  books on disk and chose a tab inside Books.

## References

- Spec `specs/046-audiobooks/`.
- B1429, B1430; V1402 to V1404 in `specs/001-pharos-baseline/`.
- `crates/pharos-scanner/src/audiobook.rs`, ADR-0126.
