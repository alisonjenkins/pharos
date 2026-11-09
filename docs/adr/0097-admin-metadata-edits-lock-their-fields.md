# ADR-0097: Admin metadata edits are written in place and lock their fields

- **Status:** Proposed
- **Date:** 2026-11-09T23:55:00Z
- **Deciders:** Alison (spec 015 US3); recorded by the metadata editing thread

## Context

Spec 015 US3 lets an admin fix an item's or a show's metadata: title, sort
title, overview, tagline, year, premiere date, parental rating, scores, and a
show's genres. Metadata in pharos is rewritten from several places:

- the scanner's put (`sqlite_put.rs`, `postgres_put.rs`) writes every column
  from the file and its NFO whenever a file changes or an admin refreshes it;
- the online enricher fills unset fields on items, and its series upsert
  writes a show's whole `series_metadata` row from the provider;
- per-user description languages (ADR-0062) replace an overview at read time.

An edit that any of these overwrite is lost the next time they run, so an edit
has to say which fields are the admin's. Every list, sort, search and DTO
reads the item columns directly (`title_fold` and `sort_name` drive search and
sort), so the edited value has to be visible there too.

## Decision

We will write an edit into the item's own columns (and a show's
`series_metadata` row, created if it has none) and record the edited fields in
a new integer column, `locked_fields`, on both tables (migration 0078). Each
bit is one `pharos_core::EditableField`, numbered by its discriminant; a new
field takes the next number and a number is never reused.

- The scan's put and the series upsert keep the stored value of every locked
  column: `col = CASE WHEN (locked_fields & bit) <> 0 THEN stored ELSE new END`.
  Neither ever writes `locked_fields`; only `MetadataEditStore::edit_metadata`
  does (V1030).
- Unlocking a field keeps its value until the next scan or refresh writes it.
- A sort title follows its title: an edited title rewrites an unlocked sort
  title, and a put keeps the stored sort title while either is locked. A
  blank sort title means "sort by the title": it is derived again and left
  unlocked.
- A locked overview is shown in every description language (V1032).
- A show's name is not editable: it is the show's identity (spec 033), whose
  hand fixes belong to the series identity tools (#1117).
- Item genres, tags, people, studios and images are not in this slice: they
  are join tables the scanner relinks, and need their own lock check.
- The admin REST route `/admin/items/{id}/metadata` and Jellyfin's
  `POST /Items/{id}` share one service (V172). The Jellyfin save edits only
  the fields whose value changed or that its `LockedFields` names, and never
  unlocks, because pharos does not yet send `LockedFields` in item answers.

## Consequences

- Every read path, SQL sort and search sees an edit with no overlay and no
  extra query; `MediaItem` grows by one `u32`.
- Each put pays a bitwise test per lockable column in its conflict clause.
- A field added to `EditableField` must also get its guard in both puts (and
  the series upsert if it lives there); the put tests check the item columns.
- The sqlite→postgres copy (`migrate.rs`) carries the item locks; it does not
  copy `series_metadata` at all today, so show edits are lost on a backend
  move, as provider metadata already is.

## Alternatives considered

- **An overrides table read over the scanned values.** Keeps the scanner's
  values for an unlock, but every read path (lists, the item list cache, SQL
  sort and search by `title_fold`) would need the overlay, on both backends.
- **Writing NFO files.** Survives a database loss and is what Kodi does, but
  pharos does not write into media folders today, and a read-only mount would
  make edits fail.
- **Jellyfin's `LockData` (lock the whole item).** Coarser than the editor
  needs, and a locked item would stop picking up any provider fix.

## References

- `specs/015-ui-account-admin/spec.md` US3
- `crates/pharos-core/src/metadata_edit.rs`, `crates/pharos-store-sqlx/src/metadata_edit.rs`
- ADR-0062 (description languages), ADR-0086 (series identity)
