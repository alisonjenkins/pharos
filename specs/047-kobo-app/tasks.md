# Tasks: Pharos on a Kobo (047)

**Input**: [spec.md](spec.md), [plan.md](plan.md), ADR-0136. Ids are local to this spec (`T047-NN`). No real titles, artists or paths in code, tests or fixtures.

## Slice 1 - Sign in and music

- [x] T047-01 Spike: Cobalt simulator signs in to a local pharos and lists music; pharos's stream decodes with Cobalt's decoder; static ARM build.
- [x] T047-02 `kobo/cobalt.patch`: Pharos built in, official platform updates refused (FR-007). Cobalt's own tests pass with it.
- [x] T047-03 `api.rs` with host tests (FR-002, FR-006).
- [x] T047-04 Sign-in, kept account, rejected token, expired code (FR-001, FR-003).
- [x] T047-05 Albums, tracks, player, reports (FR-004 to FR-006). Disarm the Started report once.
- [x] T047-06 Simulator run against a local pharos (SC-001).
- [ ] T047-07 `kobo/default.nix` and the flake package; build it on a machine with the flake inputs (SC-003).
- [ ] T047-08 Install on the Libra Colour and hear a track (SC-002). Needs the owner's go.

## Slice 2 - Audiobooks

- [ ] T047-09 Audiobooks from the Books library; resume from the server's place (FR-008).
- [ ] T047-10 Parts over 64 MiB download to the shelf in ranged pieces, then play.

## Slice 3 - Our own updates

- [ ] T047-11 Point the platform updater at Pharos's releases and our Ed25519 key; CI signs and publishes `KoboRoot.tgz` (FR-009). Needs a signing seed as a GitHub Actions secret.

## Slice 4 - Ebooks

- [ ] T047-12 Download a book and open it with `kobo-bookview` (FR-010).

## Slice 5 - PDFs

- [x] T047-13 Pharos draws a PDF's pages as a CBZ for the panel; the Kobo reads it off the card a page at a time in Cobalt's comic viewer (ADR-0144, V1460–V1462).
- [ ] T047-14 Install on a Kobo and read a long PDF; check page-turn time and memory. Needs the owner's go.
