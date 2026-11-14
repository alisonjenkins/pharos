# ADR-0130: A book's formats fold into one item with versions

- **Status:** Accepted
- **Date:** 2026-11-14T08:35:00Z
- **Deciders:** Alison (asked for a tidy Books library), Claude

## Context

A Books library often holds the same book more than once: an epub, a pdf
and a Kindle file side by side, or an exact copy with a `(1)` suffix. Each
file became its own item, so the grid showed the same book several times and
reading progress was split across them. Films already fold copies of one
film into one item with alternate versions (`copy_buckets`, the Version
picker), but that fold reads probe facts and never ran for books.

## Decision

We will fold book files into one item at scan time when all three hold:

- they are in the same folder;
- their titles match after `book::title::clean_stem_title` and case folding;
- their authors match (case, punctuation and word order ignored), or one
  side has no author.

The primary is the format the reader handles best: epub, then pdf, then
comic, then a Kindle file that converts, then a locked one. Ties go to the
clean stem over a `(1)` copy, then the bigger file, then the path. The other
files become versions. Each version advertises the format the client can
read, so a converting Kindle file advertises `epub`. Versions are read with
the book parser, never ffmpeg. The detail page shows a **Format** picker when
there is more than one version. Download (`mediaSourceId`) and the reader open
the picked version. When a row folds into another, its played state,
favourite and reading place move to the surviving item, and the newer
reading place wins.

The rule lives in `crates/pharos-scanner/src/book/fold.rs`. Each decision is
counted in `pharos_book_fold_total{outcome,reason}`.

## Consequences

- The grid shows one tile per book, and progress lives on one item.
- Folding is a scan-time decision: undoing it means a rescan with the rule
  changed. Rows already folded lose their separate ids.
- A reading place saved in one format opens another format at the start,
  because locators name their format. The progress fraction still carries.
- The watcher's single-file path does not fold. A second format added while
  the server runs shows as its own item until the next full scan.
- Two different books with the same title, no authors and the same folder
  will fold. The counter and its log line make that visible.

## Alternatives considered

- **Group at listing time.** No stored change, but every listing, search,
  Home rail and the Jellyfin API would each need to group, and progress
  would still be split across items.
- **Fold across folders.** Catches more duplicates, but risks folding
  different books that share a title in different collections.
- **Leave duplicates and add a filter.** Hides the symptom only.

## References

- B1422, V1391 in `specs/001-pharos-baseline/`.
