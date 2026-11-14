# ADR-0129: A book's subjects come from Open Library when the file has none

- **Status:** Accepted
- **Date:** 2026-11-14T09:20:00Z
- **Deciders:** the project owner (asked for automatic book categories), Claude

## Context

Books carry no genres, so the Books grid's Genre filter is empty. Many files
carry subjects of their own (EPUB `dc:subject`, Kindle EXTH 105), and spec 043
reads those first. Many do not: PDFs rarely do, and DRM-locked Kindle files
often carry only a title, author, publisher and ISBN. About a quarter of the
library has an ISBN.

A book's subjects have to come from somewhere outside the file for the rest.
The choices are a keyed catalogue (Google Books, ISBNdb), a free one (Open
Library), or none.

## Decision

We will look up a book with no usable subjects on Open Library, in a
background job, opt-in:

- By ISBN first: `GET https://openlibrary.org/isbn/{isbn}.json`, then the
  first work it names, `GET {work}.json`, and its `subjects`.
- Else by title and author: `GET /search.json?title=&author=&fields=key,subject,title,author_name&limit=5`.
  A result counts only when its normalised title equals the book's and one of
  its author names shares a word with the book's author. Anything less is a
  miss: a wrong shelf looks deliberate.
- Only the title, author and ISBN are sent. The User-Agent is
  `pharos/<version> (self-hosted media server; book subjects)` with no contact
  address.
- At most one request per 1.1 s. A 429 or 503 backs off (`Retry-After`, else
  5 s) and retries once.
- Answers and misses are memoised in `.book_subjects.json` beside the cache;
  a miss is retried after 30 days.
- The switch is `[books].online_subjects` (env `PHAROS_BOOKS_ONLINE`), off by
  default, like `[lyrics].online` for LRCLIB: each lookup tells a third party
  what the operator reads.

The subjects are mapped to the fixed shelf set in
`pharos-scanner/src/book/shelf.rs` and stored as the book's genres.

## Consequences

- No API key and no account. Books with an ISBN or a clean title and author
  get a shelf.
- Open Library's subjects are noisy (`Accessible book`, `In library`); the
  shelf module drops known noise before mapping.
- A rescan relinks genres from the file, which drops a looked-up shelf; the
  next job pass restores it from the memo with no request.
- Open Library availability matters only for books not yet memoised; an
  outage counts `unavailable` and the next pass retries.

## Alternatives considered

- **Google Books.** Better coverage, but needs a key for steady use and its
  terms tie the data to display on Google's terms.
- **ISBNdb.** Paid.
- **Folder names.** 95% of the files sit flat in one folder.
- **Filename keywords.** Too weak to tell a Python book from a novel called
  "Python".

## References

- `specs/043-book-categories/`.
- <https://openlibrary.org/developers/api>.
- ADR-0097 (admin metadata edits and locks).
