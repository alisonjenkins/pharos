# ADR-0062: Show descriptions are stored per language and shown in each user's language

- **Status:** Accepted
- **Date:** 2026-11-05T16:00:00Z
- **Deciders:** Alison

## Context

TVDB's base series and episode records are written in the language the show
was made in. Pharos read them as they were, so Dragon Ball and other anime
showed Japanese descriptions to English-speaking users (B564). Alison wants
to keep the ability to show other languages, but chosen per user: "we should
make it so we can set the language in the per user settings on what language
the show descriptions should be in."

Descriptions live in one `overview` column on `media_items` and
`series_metadata`. Full-text search reads that column. The Jellyfin API and
the native capnp API both serve it.

## Decision

1. **English is the default text.** Every TVDB text request names a language.
   Series come from `/series/{id}/extended?meta=translations`, and episodes
   from `/episodes/{id}/translations/{lang}` when the episode lists that
   language. The default `overview` and `title` columns hold English when TVDB
   has it, and otherwise the base record. Each decision is counted in
   `pharos_metadata_translation_total{provider,language,outcome}`, with
   `language` bounded to the offered codes plus `other`.
2. **Every fetched language is a row in `metadata_translations`**
   (migration 0066, sqlite and postgres). The key is
   `(owner_kind, owner_key, language)`, where the owner is an item id or a
   series key. The base record's own text is stored too, flagged
   `is_original`. A row with no text records that the provider has none in
   that language. It is asked for again after 30 days, and a failed request
   after a day.
3. **A backfill pass fills the table.** After each metadata run, the pass
   asks for English plus every language a user has picked, at most 500 gaps
   per run, one request per owner. It stops after three failures in a row.
   When the English row arrives and the stored default column still equals
   the base record's text, the column is replaced with English. It is on by
   default, and `[metadata] fetch_translations = false` turns it off.
4. **The setting is `DescriptionLanguagePreference` in the user's Jellyfin
   configuration blob.** Its values are a three-letter code, empty for
   English, or `OriginalLanguage`. The native API reads and writes it as the
   `Prefs` key `description.language`, which the server maps onto the blob,
   so there is one setting and no capnp schema change.
5. **The language is resolved per request in one service**,
   `services::metadata_text`, used by both APIs. A user who picked a
   language gets the stored row in that language, else the default column,
   which holds English when the provider has it and otherwise the original;
   "original" reads the row flagged `is_original`, else the default column.
   No English or original row is looked up as a further fallback. A
   user on the default costs one configuration read per page that shows
   shows or episodes, and no translation lookup. Overviews are resolved;
   titles stay the default column.
6. **Search stays on the default column.** FTS indexes `overview` and `title`
   as stored, which is English where TVDB has it. Text in other languages is
   not searchable.

## Consequences

- An English speaker sees English without setting anything, and someone who
  wants Japanese or the original text picks it once for every device and
  client.
- Enriching an episode of a show not made in English costs a second TVDB
  request, `/episodes/{id}/translations/eng`, when the episode lists English,
  which roughly doubles enrichment requests for anime.
- The backfill makes one TVDB request per show and per episode in a picked
  language. 500 per run spreads a large library over several runs. A failed
  request is asked again after a day, and three in a row stop the run.
- Every view a user reads with a non-default language pays one indexed
  lookup per page.
- Titles are not localized per user, and TMDB sends no language (it
  defaults to en-US). Both are open.
- Searching for a word that appears only in a translated description finds
  nothing. Indexing per language would need a different FTS layout.
- A default column written before B558, still holding HTML entities, does
  not equal the base text, so the backfill leaves it alone.
