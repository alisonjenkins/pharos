//! The browse tree: which server call fills which node. Generic over
//! [`Catalogue`] so tests drive it with a fake and the worker with the
//! native API.

use pharos_client::api::{Item, ItemKind, ItemRef, LibraryKind};
use pharos_client::mix::{presets, MixSpec};

use super::error::ContentError;
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

/// Songs, albums and artists matching a spoken request, best match first.
pub async fn search<C: Catalogue>(
    cat: &C,
    urls: &Urls,
    text: &str,
) -> Result<Vec<Node>, ContentError> {
    Ok(nodes(urls, &cat.search(text, SEARCH_LIMIT).await?))
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
}
