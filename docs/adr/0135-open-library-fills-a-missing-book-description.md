# ADR-0135: Open Library fills a book's missing description

- **Status:** Accepted
- **Date:** 2026-11-14T20:30:00Z
- **Deciders:** the project owner (asked for a description on every book page, chose to turn the lookup on), Claude

## Context

A book's detail page shows its overview. The overview comes from the file:
an EPUB's `dc:description`, a PDF's `/Subject`, a Kindle file's EXTH
description, a comic's `Summary`. Many files carry none, so their pages are
blank under the title.

ADR-0129 already asks Open Library about books, for subjects, behind
`[books].online_subjects`. The work record it reads (`/works/{id}.json`)
carries a `description` too, as a plain string or as
`{"type": "/type/text", "value": …}`.

## Decision

We will fill a book's missing description from the same Open Library work,
in the same job, behind the same switch:

- A book is asked about when it needs shelves (ADR-0129) or its overview is
  empty, and the field it needs is not locked by an admin. A book needing
  only locked fields is not asked about.
- The ISBN rung reads the description from the work it already fetches. A
  search match costs one more request, the match's work, since search
  results carry no description.
- The description is cleaned to plain text by
  `pharos_scanner::book::description` (V1414) and stored as the item's
  overview. A file's own description always wins: the job only fills an
  empty one.
- The memo keeps the raw description beside the subjects, and its version
  goes from 1 to 2, so every book is asked once more. A rescan that empties
  the overview is undone by the next pass from the memo, with no request.
- The switch stays `[books].online_subjects` (env `PHAROS_BOOKS_ONLINE`): the
  same title, author and ISBN go to the same service, and an operator who
  turned it on for subjects has already accepted that.

## Consequences

- Books with an ISBN or a confident title and author match get a
  description, and their shelves where they need them.
- After a rescan re-reads a book whose file has no description, the page is
  blank until the next pass (at most six hours).
- Signals: `pharos_book_description_total{source="openlibrary",outcome}` and
  the `book described` log line naming the item, the rung and the work.
- Open Library's descriptions are community-written; a wrong one is fixed
  by an admin edit, which locks the field.

## Alternatives considered

- **A separate switch.** Two switches for one service and one set of fields
  sent would let an operator believe one is off when its data is already
  leaving. Rejected.
- **Writing through the scanner.** Making the book provider read the memo
  would keep descriptions across rescans, but would tie the scanner to a
  server-side job's file. Not done now; worth revisiting if the gap after
  rescans matters.
