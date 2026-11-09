# ADR-0084: A person's details come from TMDB on the first visit to their page

- **Status:** Accepted
- **Date:** 2026-11-09T10:30:00Z
- **Deciders:** Alison Jenkins

## Context

A person's page (T152) lists what they act in on this server and links out
to IMDb and Wikipedia. Alison asked for those links to lead to the person
(2026-11-09: "The same page should also link to them on IMDB", "A wikipedia
page would also be good for that actor page too"). The server knew only a
name, so the links were name searches, which land on a list when the name is
shared.

Cast rows written by the TMDB credits fetch (#1032) carry the person's TMDB
id (`people.provider_ids = "tmdb:1234"`). TMDB's person record gives a
biography, birth and death dates, birth place, the IMDb id and the Wikidata
id in one request (`/person/{id}?append_to_response=external_ids`). Wikidata
leads to the English Wikipedia article. The library holds tens of thousands
of people; most pages will never be opened.

## Decision

We will fetch a person's details from TMDB when their page is opened, not in
a backfill, and keep them in a `person_details` table keyed by the person's
wire id (migration 0072):

- Kept details younger than 30 days are shown as they are. Older ones, or
  none, are fetched again, with a 4 s budget so a slow provider does not hold
  the page. A failed fetch shows what was kept, or nothing, and is retried on
  the next visit. A person TMDB does not know is kept as an empty record, so
  they are not asked about again for 30 days; one it no longer knows keeps
  the details already kept.
- The fetch sits behind `PersonDetailsSource` (`crates/pharos-server/src/
  person_details.rs`), implemented by `TmdbClient` and installed only when a
  TMDB key is configured (`PHAROS_TMDB_API_KEY`). Tests use a fake.
- Only an IMDb id of the form `nm` plus digits and a Wikidata id of the form
  `Q` plus digits are kept, because the UI builds links from them.
- The UI links to `https://www.imdb.com/name/{imdb}/` and to
  `https://www.wikidata.org/wiki/Special:GoToLinkedPage/enwiki/{Q}`, and
  keeps the name searches when an id is unknown.
- Every answer counts under `pharos_person_details_total{outcome}` (`kept`,
  `fetched`, `no_provider_id`, `no_source`, `timeout`, `not_found`,
  `transport`, `status`, `decode`, `store_error`).

## Consequences

- A first visit to someone's page can wait up to 4 s for TMDB. Later visits
  within 30 days read the store only.
- People with no TMDB id (cast taken only from NFO files) keep the name
  searches until a credits fetch gives them one.
- A biography is in whatever language TMDB returns by default (English); the
  metadata language setting is not applied yet.
- The Wikipedia link goes through Wikidata, so a person with a Wikidata item
  but no English article lands on Wikidata's page instead (not verified
  against every case).

## Alternatives considered

- **A background backfill of every person.** Tens of thousands of TMDB
  requests for pages nobody opens, and it would compete with the credits
  fetch for TMDB's rate limit.
- **Asking Wikipedia's API for the article title.** A second provider and a
  second request per page; Wikidata's redirect gives the same result from
  the id TMDB already returns.
- **Fetching on every visit.** Simple, but each page view would cost a TMDB
  request and the page would show nothing whenever TMDB is down.

## References

- ADR-0076 (provider portraits behind `PortraitSource`), the adapter pattern
  this follows.
- T152, V862, V863 in `specs/001-pharos-baseline/`.
