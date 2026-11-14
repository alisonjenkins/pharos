# Feature specification: audiobooks (046)

**Created**: 2026-11-14T11:40:00Z
**Status**: Draft. Built on the Books stack (Courses, OPDS, categories).
**Scope**: Audiobooks under a Books library root are found, listed on an Audiobooks tab beside Books and Courses, and play with resume, chapters, speed and a sleep timer.
**Decision record**: ADR-0132 (an audiobook and its chapters are an album of songs).

---

## Problem statement

- **No audiobook concept.** The scanner did not walk `.m4b`. An `.mp3` under a Books root was stored as a loose song, with whatever album tag it had, and listed nowhere in the Books library.
- **No place to find them.** The Books library shows a Books grid and, with courses, a Courses tab (ADR-0126). There is nothing for audio.
- **Music resume rules do not fit a book.** A song stopped past 90% is marked played and its position reset. For a twenty-hour book that is hours unheard.

There are no audiobooks on disk yet. Everything here is built and tested against invented fixtures ("Gadget Tales" by "Ada Field").

## User stories

1. As the owner, I drop `Ada Field/Gadget Tales.m4b` into the Books folder, and it appears on the Audiobooks tab with its cover, title, author and length.
2. As the owner, I drop a folder of mp3 parts, and it appears as one book whose parts play in order.
3. As a listener, I stop halfway and come back later on any device: Play says Resume and starts where I stopped.
4. As a listener, I change the speed, set a sleep timer, and jump to a chapter, without the music player I use for songs changing.

## Requirements

- **FR-001**: The scanner walks `.m4b`.
- **FR-002**: An audio file whose innermost library is a Books library is classified (`audiobook::classify`):
  - any `.m4b` is a single-file book (`verdict=single_file`, `reason=m4b`);
  - another audio file with chapter marks, alone in its book folder or directly in the root, is a single-file book (`reason=chapters`);
  - otherwise, a file in a folder is a track of the folder's book (`verdict=folder_track`, `reason=in_folder`); a disc folder (`CD1`, `Disc 2`, `Part 3`) belongs to the book above it;
  - a file directly in the root with no chapters is no book and is left out (`verdict=skipped`, `reason=loose_without_chapters`).
- **FR-003**: A book's tracks are numbered by natural order of their paths under the book folder (`2` before `10`), from 1, with no disc number.
- **FR-004**: Title: a single file's album tag, else title tag, else cleaned file name; a folder book's cleaned folder name. Author: album-artist tag, else artist tag, else the folder the book sits in when that is not the root. The track title is its title tag, else its cleaned file name.
- **FR-005**: Each classification is logged ("scanner: audio in a books library classified", with path, verdict, reason, chapters, book, author, track, tracks) and counted as `pharos_scan_audiobook_total{verdict,reason}`. Labels come from `label()` methods and are asserted distinct.
- **FR-006**: A Books library listed with no kind named still lists books only (V1393).
- **FR-007**: A Books library's albums are its audiobooks. The native API marks an audiobook album and an audiobook's track with `ItemSummary.audiobook` (schemaVersion 48); an audiobook album's summary carries its author as `artist` and its whole length as `runtimeMs`.
- **FR-008**: The native Books library shows an Audiobooks tab when it has any audiobooks, beside Books and Courses; each tab appears only when it has items. Tiles show cover, title, author and length.
- **FR-009**: An audiobook album's Play starts on the part last begun and not finished, else the first not finished, else the first, and says Resume once any part is begun. The player starts a part at its stored position.
- **FR-010**: An audiobook's track counts as finished only when the stop leaves no more than the smaller of the played threshold's share and one minute.
- **FR-011**: While an audiobook plays, the listen view adds speed pills (0.75, 1, 1.25, 1.5, 1.75, 2.0x; kept per device under `pharos.audiobook.speed`, apart from video speed, and reset to 1x when a song plays), a sleep timer (Off, 15, 30, 60 min, End of chapter) and a chapter list: a single file's chapters (seek), or a folder book's parts (play). The music player is otherwise unchanged.

## Success criteria

- **SC-001**: An `.m4b`, a chaptered `.m4a` and a folder of three mp3s under a Books root are stored as three books; a loose chapterless mp3 at the root is not stored; a song in a music library is untouched.
- **SC-002**: The Audiobooks tab lists two invented books with author and length, and the Books tab lists the book alone.
- **SC-003**: Stopping two hours before the end of a twenty-hour book keeps the position.

## Out of scope

- Narrator: the probe keeps no `composer` / `narrator` tag (ADR-0132 Consequences).
- A per-book page distinct from the album page.
- jellyfin-web: audiobooks show there as music albums.
- Albums keyed by folder: two books, or a book and a music album, with one title share an album.
