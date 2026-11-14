//! The browse tree: which server call fills which node. Generic over
//! [`Catalogue`] so tests drive it with a fake and the worker with the
//! native API.

use std::time::{Duration, Instant};

use pharos_client::api::{Item, ItemKind, ItemRef, LibraryKind};
use pharos_client::mix::{presets, MixSpec};

use super::error::ContentError;
use super::fuzzy;
use super::id::NodeId;
use super::node::{Node, Urls};

/// Most children one node returns; a car list is not a place to scroll
/// thousands of rows.
pub const MAX_CHILDREN: u32 = 200;
/// Songs in one mix batch.
pub const MIX_LEN: u32 = 30;
/// Most search hits returned for a voice request.
pub const SEARCH_LIMIT: u32 = 25;

const NAME_ORDER: &str = "SortName";
const TRACK_ORDER: &str = "ParentIndexNumber,IndexNumber";
const ARTIST_ALBUM_ORDER: &str = "ProductionYear,SortName";

/// The server calls the tree needs.
#[allow(async_fn_in_trait)]
pub trait Catalogue {
    /// The libraries the user may see.
    async fn views(&self) -> Result<Vec<Item>, ContentError>;
    /// Music the user listened to lately (`Library.home`'s listening rail).
    async fn recent_music(&self) -> Result<Vec<Item>, ContentError>;
    async fn list(
        &self,
        parent: ItemRef,
        group: Option<ItemKind>,
        sort_by: &str,
        limit: u32,
    ) -> Result<Vec<Item>, ContentError>;
    async fn search(&self, text: &str, limit: u32) -> Result<Vec<Item>, ContentError>;
    async fn mix(&self, spec: &MixSpec, count: u32) -> Result<Vec<Item>, ContentError>;
    async fn item(&self, id: ItemRef) -> Result<Item, ContentError>;
}

fn nodes(urls: &Urls, items: &[Item]) -> Vec<Node> {
    items.iter().filter_map(|i| urls.node(i)).collect()
}

/// The first music library, or an error naming the libraries that exist.
async fn music_library<C: Catalogue>(cat: &C) -> Result<ItemRef, ContentError> {
    let views = cat.views().await?;
    views
        .iter()
        .find(|v| v.library_kind == Some(LibraryKind::Music))
        .map(|v| v.id)
        .ok_or_else(|| {
            let seen: Vec<String> = views
                .iter()
                .map(|v| format!("{:?}={:?}", v.name, v.library_kind))
                .collect();
            ContentError::Rpc {
                op: "find the music library".into(),
                why: format!(
                    "none of the {} libraries is music: [{}]",
                    views.len(),
                    seen.join(", ")
                ),
            }
        })
}

/// What a node holds. A track holds nothing; the sign-in row is not browsable.
pub async fn children<C: Catalogue>(
    cat: &C,
    urls: &Urls,
    id: NodeId,
) -> Result<Vec<Node>, ContentError> {
    Ok(match id {
        NodeId::Root => vec![
            Node::folder(NodeId::Music, "Music"),
            Node::folder(NodeId::Audiobooks, "Audiobooks"),
        ],
        NodeId::Music => vec![
            Node::folder(NodeId::Recent, "Recently played"),
            Node::folder(NodeId::Albums, "Albums"),
            Node::folder(NodeId::Artists, "Artists"),
            Node::folder(NodeId::Mixes, "Mixes"),
        ],
        // Pharos has no audiobook kind yet (specs/004-books are epub, pdf and
        // comics only), so the node exists and is empty.
        NodeId::Audiobooks | NodeId::Track(_) | NodeId::SignIn => Vec::new(),
        NodeId::Recent => nodes(urls, &cat.recent_music().await?),
        NodeId::Albums => {
            let lib = music_library(cat).await?;
            nodes(
                urls,
                &cat.list(lib, Some(ItemKind::Album), NAME_ORDER, MAX_CHILDREN)
                    .await?,
            )
        }
        NodeId::Artists => {
            let lib = music_library(cat).await?;
            nodes(
                urls,
                &cat.list(lib, Some(ItemKind::Artist), NAME_ORDER, MAX_CHILDREN)
                    .await?,
            )
        }
        NodeId::Mixes => presets()
            .iter()
            .zip(0u8..)
            .map(|(m, n)| Node::folder(NodeId::Mix(n), &m.name))
            .collect(),
        NodeId::Artist(a) => nodes(
            urls,
            &cat.list(a, Some(ItemKind::Album), ARTIST_ALBUM_ORDER, MAX_CHILDREN)
                .await?,
        ),
        NodeId::Album(a) => nodes(urls, &cat.list(a, None, TRACK_ORDER, MAX_CHILDREN).await?),
        NodeId::Mix(n) => {
            let all = presets();
            let mix = all.get(usize::from(n)).ok_or_else(|| {
                ContentError::BadId(format!(
                    "{} (only {} built-in mixes)",
                    NodeId::Mix(n),
                    all.len()
                ))
            })?;
            nodes(urls, &cat.mix(&mix.spec, MIX_LEN).await?)
        }
    })
}

/// One node by id. Folders need no call; an album, artist or track asks the
/// server so a play request for a remembered id gets a fresh stream URL.
pub async fn node<C: Catalogue>(cat: &C, urls: &Urls, id: NodeId) -> Result<Node, ContentError> {
    let fixed = match id {
        NodeId::Root => Some(Node::folder(NodeId::Root, "Pharos")),
        NodeId::Music => Some(Node::folder(NodeId::Music, "Music")),
        NodeId::Audiobooks => Some(Node::folder(NodeId::Audiobooks, "Audiobooks")),
        NodeId::Recent => Some(Node::folder(NodeId::Recent, "Recently played")),
        NodeId::Albums => Some(Node::folder(NodeId::Albums, "Albums")),
        NodeId::Artists => Some(Node::folder(NodeId::Artists, "Artists")),
        NodeId::Mixes => Some(Node::folder(NodeId::Mixes, "Mixes")),
        NodeId::Mix(n) => presets()
            .get(usize::from(n))
            .map(|m| Node::folder(id, &m.name)),
        NodeId::SignIn => Some(Node::sign_in("")),
        NodeId::Album(_) | NodeId::Artist(_) | NodeId::Track(_) => None,
    };
    if let Some(n) = fixed {
        return Ok(n);
    }
    let (NodeId::Album(r) | NodeId::Artist(r) | NodeId::Track(r)) = id else {
        return Err(ContentError::BadId(id.to_string()));
    };
    let item = cat.item(r).await?;
    urls.node(&item).ok_or_else(|| ContentError::Rpc {
        op: format!("node {id}"),
        why: format!("item {r} is a {:?}, which the car does not show", item.kind),
    })
}

/// Most server calls one spoken search may make.
pub const SEARCH_CALLS: usize = 4;
/// A top score at or above this needs no wider net.
const GOOD_SCORE: f64 = 0.8;
/// Hits found only by word-by-word search must score at least this, or they
/// are noise the server matched on one common word.
const MIN_SCORE: f64 = 0.55;
/// Stop widening the search after this long; the caller's budget is 10 s.
const WIDEN_BUDGET: Duration = Duration::from_secs(5);

/// What one search did, for the log.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchReport {
    pub normalized: String,
    pub calls: usize,
    pub candidates: usize,
    pub top_score: f64,
    /// `full-hit`, `fuzzy-hit`, `no-match` or `empty-query`.
    pub verdict: &'static str,
}

struct Scored {
    node: Node,
    score: f64,
}

/// The kinds the car shows, as scored, deduplicated candidates.
fn add_candidates(urls: &Urls, heard: &str, items: &[Item], into: &mut Vec<Scored>) {
    for item in items {
        let Some(node) = urls.node(item) else {
            continue;
        };
        if into.iter().any(|c| c.node.id == node.id) {
            continue;
        }
        let score = fuzzy::best_similarity(heard, &node.title, &node.subtitle);
        into.push(Scored { node, score });
    }
}

fn rank(c: &mut [Scored]) {
    // Stable: ties keep the server's own order.
    c.sort_by(|a, b| b.score.total_cmp(&a.score));
}

/// Songs, albums and artists matching a spoken request, best match first.
///
/// The voice command ("play ... on pharos") is stripped, the server is asked
/// for the rest, and the hits are re-ranked by how alike they read and sound
/// to what was heard. When that finds little, each significant word is
/// searched too and the merged candidates must clear [`MIN_SCORE`]. Returns
/// empty rather than guess; the caller decides what to do then.
pub async fn search<C: Catalogue>(
    cat: &C,
    urls: &Urls,
    text: &str,
) -> Result<Vec<Node>, ContentError> {
    let (nodes, report) = search_reported(cat, urls, text).await?;
    log::info!(
        "search query={text:?} normalized={:?} calls={} candidates={} top_score={:.2} verdict={} returned={}",
        report.normalized,
        report.calls,
        report.candidates,
        report.top_score,
        report.verdict,
        nodes.len()
    );
    Ok(nodes)
}

pub async fn search_reported<C: Catalogue>(
    cat: &C,
    urls: &Urls,
    text: &str,
) -> Result<(Vec<Node>, SearchReport), ContentError> {
    let started = Instant::now();
    let heard = fuzzy::strip_command(text);
    let mut report = SearchReport {
        normalized: heard.clone(),
        calls: 0,
        candidates: 0,
        top_score: 0.0,
        verdict: "empty-query",
    };
    if heard.is_empty() {
        return Ok((Vec::new(), report));
    }

    let mut found: Vec<Scored> = Vec::new();
    report.calls += 1;
    add_candidates(
        urls,
        &heard,
        &cat.search(&heard, SEARCH_LIMIT).await?,
        &mut found,
    );
    rank(&mut found);
    let top = found.first().map_or(0.0, |c| c.score);
    let full_hits = !found.is_empty();

    if found.len() < 3 || top < GOOD_SCORE {
        for word in fuzzy::significant_words(&heard) {
            if report.calls >= SEARCH_CALLS || started.elapsed() > WIDEN_BUDGET {
                break;
            }
            if word == heard {
                continue;
            }
            report.calls += 1;
            match cat.search(&word, SEARCH_LIMIT).await {
                Ok(items) => add_candidates(urls, &heard, &items, &mut found),
                Err(e) if e.is_revoked() => return Err(e),
                Err(e) => log::warn!("search word {word:?}: {e}"),
            }
        }
        rank(&mut found);
    }

    report.candidates = found.len();
    report.top_score = found.first().map_or(0.0, |c| c.score);
    // A full-query hit is the server's own text match: keep it however it
    // scores. Anything found only by widening must look like the request.
    // (Words alone that matched nothing in full can add noise, so the filter
    // applies whenever the full query found nothing.)
    if !full_hits {
        found.retain(|c| c.score >= MIN_SCORE);
    }
    report.verdict = if found.is_empty() {
        "no-match"
    } else if full_hits && report.calls == 1 {
        "full-hit"
    } else {
        "fuzzy-hit"
    };
    found.truncate(SEARCH_LIMIT as usize);
    Ok((found.into_iter().map(|c| c.node).collect(), report))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::cell::RefCell;

    use futures::executor::block_on;

    use super::*;
    use crate::content::node::tests::item;

    /// Records each call; answers fixed items.
    #[derive(Default)]
    struct Fake {
        calls: RefCell<Vec<String>>,
        no_music: bool,
    }

    impl Catalogue for Fake {
        async fn views(&self) -> Result<Vec<Item>, ContentError> {
            let mut lib = item(ItemRef::Real(100), ItemKind::Library, "Lib");
            lib.library_kind = Some(if self.no_music {
                LibraryKind::Movies
            } else {
                LibraryKind::Music
            });
            Ok(vec![
                item(ItemRef::Real(99), ItemKind::Library, "Other"),
                lib,
            ])
        }
        async fn recent_music(&self) -> Result<Vec<Item>, ContentError> {
            self.calls.borrow_mut().push("recent".into());
            Ok(vec![
                item(ItemRef::Real(1), ItemKind::Audio, "T"),
                item(ItemRef::Real(2), ItemKind::Movie, "dropped"),
            ])
        }
        async fn list(
            &self,
            parent: ItemRef,
            group: Option<ItemKind>,
            sort_by: &str,
            limit: u32,
        ) -> Result<Vec<Item>, ContentError> {
            self.calls
                .borrow_mut()
                .push(format!("list {parent} {group:?} {sort_by} {limit}"));
            Ok(vec![item(ItemRef::Synth(3), ItemKind::Album, "Al")])
        }
        async fn search(&self, text: &str, limit: u32) -> Result<Vec<Item>, ContentError> {
            self.calls
                .borrow_mut()
                .push(format!("search {text} {limit}"));
            Ok(vec![
                item(ItemRef::Real(5), ItemKind::Audio, "S"),
                item(ItemRef::Real(6), ItemKind::Episode, "no"),
            ])
        }
        async fn mix(&self, _: &MixSpec, count: u32) -> Result<Vec<Item>, ContentError> {
            self.calls.borrow_mut().push(format!("mix {count}"));
            Ok(vec![item(ItemRef::Real(7), ItemKind::Audio, "M")])
        }
        async fn item(&self, id: ItemRef) -> Result<Item, ContentError> {
            Ok(item(
                id,
                if matches!(id, ItemRef::Real(_)) {
                    ItemKind::Audio
                } else {
                    ItemKind::Movie
                },
                "I",
            ))
        }
    }

    fn urls() -> Urls {
        Urls::new("https://s", "t")
    }

    fn ids(nodes: &[Node]) -> Vec<&str> {
        nodes.iter().map(|n| n.id.as_str()).collect()
    }

    #[test]
    fn root_music_and_mixes_are_fixed_and_need_no_call() {
        let f = Fake::default();
        let root = block_on(children(&f, &urls(), NodeId::Root)).unwrap();
        assert_eq!(ids(&root), ["music", "audiobooks"]);
        let music = block_on(children(&f, &urls(), NodeId::Music)).unwrap();
        assert_eq!(
            ids(&music),
            [
                "music/recent",
                "music/albums",
                "music/artists",
                "music/mixes"
            ]
        );
        let mixes = block_on(children(&f, &urls(), NodeId::Mixes)).unwrap();
        assert!(
            !mixes.is_empty()
                && mixes
                    .iter()
                    .all(|m| m.browsable && m.id.starts_with("mix/"))
        );
        assert!(f.calls.borrow().is_empty());
    }

    #[test]
    fn audiobooks_are_empty_until_the_server_has_them() {
        let f = Fake::default();
        assert!(block_on(children(&f, &urls(), NodeId::Audiobooks))
            .unwrap()
            .is_empty());
    }

    #[test]
    fn albums_list_the_music_library_not_the_first_library() {
        let f = Fake::default();
        let got = block_on(children(&f, &urls(), NodeId::Albums)).unwrap();
        assert_eq!(ids(&got), ["album/s3"]);
        assert_eq!(f.calls.borrow()[0], "list 100 Some(Album) SortName 200");
    }

    #[test]
    fn a_server_with_no_music_library_names_what_it_has() {
        let f = Fake {
            no_music: true,
            ..Fake::default()
        };
        let e = block_on(children(&f, &urls(), NodeId::Artists)).expect_err("no music");
        let text = e.to_string();
        assert!(
            text.contains("2 libraries") && text.contains("Movies"),
            "{text}"
        );
    }

    #[test]
    fn album_tracks_are_in_disc_and_track_order_and_artist_albums_by_year() {
        let f = Fake::default();
        block_on(children(&f, &urls(), NodeId::Album(ItemRef::Synth(3)))).unwrap();
        block_on(children(&f, &urls(), NodeId::Artist(ItemRef::Synth(4)))).unwrap();
        let calls = f.calls.borrow();
        assert_eq!(calls[0], "list s3 None ParentIndexNumber,IndexNumber 200");
        assert_eq!(calls[1], "list s4 Some(Album) ProductionYear,SortName 200");
    }

    #[test]
    fn recent_keeps_only_what_the_car_shows() {
        let f = Fake::default();
        let got = block_on(children(&f, &urls(), NodeId::Recent)).unwrap();
        assert_eq!(ids(&got), ["track/1"]);
    }

    #[test]
    fn a_mix_plays_a_batch_and_an_unknown_mix_is_a_bad_id() {
        let f = Fake::default();
        let got = block_on(children(&f, &urls(), NodeId::Mix(0))).unwrap();
        assert_eq!(ids(&got), ["track/7"]);
        assert_eq!(f.calls.borrow()[0], "mix 30");
        let e = block_on(children(&f, &urls(), NodeId::Mix(250))).expect_err("unknown mix");
        assert!(
            matches!(e, ContentError::BadId(ref s) if s.starts_with("mix/250")),
            "{e}"
        );
    }

    #[test]
    fn search_drops_kinds_the_car_does_not_show() {
        let f = Fake::default();
        let got = block_on(search(&f, &urls(), "a song")).unwrap();
        assert_eq!(ids(&got), ["track/5"]);
        assert_eq!(f.calls.borrow()[0], "search a song 25");
    }

    #[test]
    fn a_track_node_is_fetched_with_a_fresh_stream_url_and_a_movie_is_refused() {
        let f = Fake::default();
        let n = block_on(node(&f, &urls(), NodeId::Track(ItemRef::Real(8)))).unwrap();
        assert!(n
            .stream_url
            .unwrap()
            .contains("00000000000000000000000000000008"));
        let e = block_on(node(&f, &urls(), NodeId::Album(ItemRef::Synth(8)))).expect_err("movie");
        assert!(e.to_string().contains("Movie"), "{e}");
    }

    /// Answers `search` from a table of (query text, hits); records the calls.
    struct Table {
        rows: Vec<(&'static str, Vec<Item>)>,
        calls: RefCell<Vec<String>>,
    }

    impl Catalogue for Table {
        async fn views(&self) -> Result<Vec<Item>, ContentError> {
            Ok(vec![])
        }
        async fn recent_music(&self) -> Result<Vec<Item>, ContentError> {
            Ok(vec![])
        }
        async fn list(
            &self,
            _: ItemRef,
            _: Option<ItemKind>,
            _: &str,
            _: u32,
        ) -> Result<Vec<Item>, ContentError> {
            Ok(vec![])
        }
        async fn search(&self, text: &str, _: u32) -> Result<Vec<Item>, ContentError> {
            self.calls.borrow_mut().push(text.to_owned());
            Ok(self
                .rows
                .iter()
                .find(|(q, _)| *q == text)
                .map(|(_, v)| v.clone())
                .unwrap_or_default())
        }
        async fn mix(&self, _: &MixSpec, _: u32) -> Result<Vec<Item>, ContentError> {
            Ok(vec![])
        }
        async fn item(&self, _: ItemRef) -> Result<Item, ContentError> {
            Err(ContentError::WorkerGone)
        }
    }

    fn track(n: u64, name: &str, artist: &str) -> Item {
        let mut i = item(ItemRef::Real(n), ItemKind::Audio, name);
        i.artist = Some(artist.to_owned());
        i
    }

    fn titles(table: &Table, said: &str) -> (Vec<String>, SearchReport) {
        let urls = Urls::new("https://s", "t");
        let (nodes, report) = block_on(search_reported(table, &urls, said)).unwrap();
        (nodes.into_iter().map(|n| n.title).collect(), report)
    }

    #[test]
    fn a_misheard_title_ranks_the_real_one_first() {
        let t = Table {
            rows: vec![
                (
                    "cast tone c",
                    vec![
                        track(1, "Cast Away Blues", "Ned Orchard"),
                        track(2, "Tone Poem", "Ned Orchard"),
                    ],
                ),
                (
                    "tone",
                    vec![
                        track(2, "Tone Poem", "Ned Orchard"),
                        track(3, "Test tone C", ""),
                    ],
                ),
                ("cast", vec![track(1, "Cast Away Blues", "Ned Orchard")]),
            ],
            calls: RefCell::default(),
        };
        let (got, report) = titles(&t, "play cast tone c on pharos");
        assert_eq!(got[0], "Test tone C", "{got:?}");
        assert_eq!(report.verdict, "fuzzy-hit");
        assert_eq!(report.normalized, "cast tone c");
        assert!(
            report.calls > 1 && report.calls <= SEARCH_CALLS,
            "{report:?}"
        );
        assert!(report.top_score > 0.8, "{report:?}");
    }

    #[test]
    fn a_good_full_hit_costs_one_call_and_is_reranked() {
        let t = Table {
            rows: vec![(
                "abbey road",
                vec![
                    track(1, "Road to Nowhere", "Ned Orchard"),
                    track(2, "Abbey Road", "Ned Orchard"),
                    track(3, "Abbey Road Medley", "Ned Orchard"),
                ],
            )],
            calls: RefCell::default(),
        };
        let (got, report) = titles(&t, "play abbey road on pharos");
        assert_eq!(got[0], "Abbey Road");
        assert_eq!(*t.calls.borrow(), ["abbey road"]);
        assert_eq!(report.verdict, "full-hit");
    }

    #[test]
    fn a_misheard_artist_ranks_that_artists_songs_first() {
        let t = Table {
            rows: vec![
                ("kel varo", vec![track(1, "Varo Waltz", "Someone Else")]),
                (
                    "kel",
                    vec![
                        track(1, "Varo Waltz", "Someone Else"),
                        track(2, "Lamp Song", "Kelvaro"),
                    ],
                ),
                ("varo", vec![track(1, "Varo Waltz", "Someone Else")]),
            ],
            calls: RefCell::default(),
        };
        let (got, _) = titles(&t, "play kel varo");
        assert_eq!(got[0], "Lamp Song", "{got:?}");
    }

    #[test]
    fn nonsense_returns_nothing_and_does_not_invent_a_hit() {
        let t = Table {
            rows: vec![
                ("zzq wvx", vec![]),
                ("zzq", vec![track(1, "Sunrise Over Quarry", "Ned Orchard")]),
                ("wvx", vec![track(2, "Harbour Lights", "Ned Orchard")]),
            ],
            calls: RefCell::default(),
        };
        let (got, report) = titles(&t, "play zzq wvx");
        assert!(got.is_empty(), "{got:?}");
        assert_eq!(report.verdict, "no-match");
        assert!(report.calls <= SEARCH_CALLS);
    }

    #[test]
    fn only_the_command_said_asks_the_server_nothing() {
        let t = Table {
            rows: vec![],
            calls: RefCell::default(),
        };
        let (got, report) = titles(&t, "play");
        assert!(got.is_empty() && t.calls.borrow().is_empty());
        assert_eq!(report.verdict, "empty-query");
    }

    #[test]
    fn server_calls_are_capped() {
        let t = Table {
            rows: vec![],
            calls: RefCell::default(),
        };
        let (_, report) = titles(&t, "alpha bravo charlie delta echo foxtrot");
        assert_eq!(t.calls.borrow().len(), SEARCH_CALLS);
        assert_eq!(report.calls, SEARCH_CALLS);
    }
}
