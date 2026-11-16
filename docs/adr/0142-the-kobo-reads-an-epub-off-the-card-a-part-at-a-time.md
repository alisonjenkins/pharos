# ADR-0142: The Kobo reads an EPUB off the card a part at a time

- **Status:** Accepted
- **Date:** 2026-11-16T19:00:00Z
- **Deciders:** the project owner (chose no per-book size cap, a 4 GiB shelf and a 2 GiB card reserve), Claude

## Context

The Kobo app downloaded a book whole into memory, checked it, then saved it
to Cobalt's shelf. Opening it read the file back whole. So a book could be no
larger than `MAX_SHELF_DOWNLOAD` (64 MiB), and a large illustrated EPUB, which
is mostly pictures, did not open at all. Opening a smaller one still spent the
time and memory of reading every picture before the first page was drawn.

ADR-0138 slims pictures on the server, but a slimmed book can still be large,
and the reader has a few hundred megabytes of memory for everything.

Cobalt's shelf allowed 256 MiB per application and kept 64 MiB of the card
free for the stock reader's database. Shelf offsets and sizes travel as `u32`,
and the card is FAT32, whose files stop one byte short of 4 GiB.

## Decision

We will stream a book to the shelf as it downloads and read it back off the
shelf in parts.

- **Download.** Each fetched piece is written to the shelf as it arrives; the
  first piece is checked to be the format promised (a Kindle book Pharos could
  not convert says so). Nothing holds the whole file in memory. A cancelled or
  failed download removes what reached the shelf.
- **Open.** `kobo-bookview`'s shelved reader reads the archive's end record
  and central directory first, then only the text, styles and fonts (at most
  64 MiB together), and each picture's first bytes to learn its size. A
  picture's whole is read when the reader comes near it, under a picture
  budget. `kobo-doc` gains the zip reading this needs.
- **Size.** There is no per-book limit beyond the shelf's own: one book may be
  up to 4 GiB − 1 byte (`MAX_SHELF_BLOB_BYTES`, the `u32`/FAT32 bound).
- **Shelf.** An application may keep 4 GiB on the shelf (`MAX_SHELF_BYTES`),
  and the shelf refuses a write that would leave less than 2 GiB of the card
  free (`SHELF_RESERVE`), so the stock reader keeps room for its database and
  the books copied beside ours.

## Consequences

- Books of any size up to 4 GiB open, and the first page is drawn after the
  text alone is read.
- A picture over 2 MiB unpacked, or over 4 MiB stored in the archive, shows as
  its description instead.
- Opening a book has no cancel; a very large book's directory and text read
  takes as long as it takes.
- A card with less than 2 GiB free refuses new downloads that a 64 MiB reserve
  allowed. The shelf reports this as full.
- The patched Cobalt carries the change; an upstream Cobalt update has to keep
  the shelf limits and the shelved reader.
