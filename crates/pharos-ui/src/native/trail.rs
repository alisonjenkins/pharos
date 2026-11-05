//! The breadcrumb trail above a page: the item shown and what holds it,
//! walked up through each item's `parent` to its library.

use pharos_client::api::{Item, ItemKind, ItemRef, PharosApi};
use pharos_client::{CallInfo, ClientError};

/// The most steps a trail holds: library, series, season, episode, and one
/// spare for a server that nests deeper.
const MAX_STEPS: usize = 5;

/// `id` and its ancestors, outermost first. Only a failure to read `id`
/// itself is an error; a failed step higher up ends the trail there.
pub async fn load<A: PharosApi>(
    api: &A,
    call: &CallInfo,
    id: ItemRef,
) -> Result<Vec<Item>, ClientError> {
    let mut trail = vec![api.detail(call, id).await?.item];
    while trail.len() < MAX_STEPS {
        let last = &trail[trail.len() - 1];
        let Some(parent) = last.parent.filter(|_| last.kind != ItemKind::Library) else {
            break;
        };
        if trail.iter().any(|item| item.id == parent) {
            break;
        }
        match api.detail(call, parent).await {
            Ok(detail) => trail.push(detail.item),
            Err(_) => break,
        }
    }
    trail.reverse();
    Ok(trail)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use std::cell::Cell;
    use std::collections::HashMap;

    use pharos_client::api::{
        Capabilities, Decision, Detail, Home, ListIndex, Listing, Page, PlaybackGrant, TrackPicks,
    };

    use super::*;

    struct NoGrant;
    impl PlaybackGrant for NoGrant {
        async fn report(
            &self,
            _: &CallInfo,
            _: u64,
            _: bool,
            _: TrackPicks,
        ) -> Result<(), ClientError> {
            Ok(())
        }
        async fn stop(&self, _: &CallInfo, _: u64) -> Result<(), ClientError> {
            Ok(())
        }
    }

    /// A server that answers `detail` from a fixed map and counts the calls.
    #[derive(Default)]
    struct Fake {
        details: HashMap<ItemRef, Item>,
        asked: Cell<usize>,
    }

    fn missing() -> ClientError {
        ClientError::Api {
            kind: "notFound",
            reason: "item not found".into(),
            trace_id: String::new(),
        }
    }

    impl PharosApi for Fake {
        type Playing = NoGrant;
        async fn home(&self, _: &CallInfo) -> Result<Home, ClientError> {
            Ok(Home::default())
        }
        async fn views(&self, _: &CallInfo) -> Result<Page, ClientError> {
            Ok(Page::default())
        }
        async fn query(
            &self,
            _: &CallInfo,
            _: &Listing,
            _: u32,
            _: u32,
        ) -> Result<Page, ClientError> {
            Ok(Page::default())
        }
        async fn index(&self, _: &CallInfo, _: &Listing) -> Result<ListIndex, ClientError> {
            Ok(ListIndex::default())
        }
        async fn search(&self, _: &CallInfo, _: &str, _: u32) -> Result<Page, ClientError> {
            Ok(Page::default())
        }
        async fn detail(&self, _: &CallInfo, id: ItemRef) -> Result<Detail, ClientError> {
            self.asked.set(self.asked.get() + 1);
            let item = self.details.get(&id).cloned().ok_or_else(missing)?;
            Ok(Detail {
                item,
                overview: String::new(),
                genres: Vec::new(),
                chapters: Vec::new(),
                audio_tracks: Vec::new(),
                subtitle_tracks: Vec::new(),
                trickplay: Vec::new(),
                segments: Vec::new(),
                fonts: Vec::new(),
            })
        }
        async fn lyrics(
            &self,
            _: &CallInfo,
            _: ItemRef,
            _: bool,
        ) -> Result<pharos_client::api::Lyrics, ClientError> {
            Ok(Default::default())
        }
        async fn mix(
            &self,
            _: &CallInfo,
            _: &pharos_client::mix::MixSpec,
            _: &[ItemRef],
            _: u32,
        ) -> Result<pharos_client::mix::MixBatch, ClientError> {
            Ok(Default::default())
        }
        async fn play(
            &self,
            _: &CallInfo,
            _: ItemRef,
            _: &Capabilities,
            _: TrackPicks,
        ) -> Result<(Decision, NoGrant), ClientError> {
            Err(missing())
        }
    }

    /// Runs a future the fake answers without waiting.
    fn run<T>(f: impl std::future::Future<Output = T>) -> T {
        use std::task::{Context, Poll, Waker};
        let mut f = std::pin::pin!(f);
        match f.as_mut().poll(&mut Context::from_waker(Waker::noop())) {
            Poll::Ready(v) => v,
            Poll::Pending => panic!("the fake server never waits"),
        }
    }

    fn item(id: ItemRef, kind: ItemKind, name: &str, parent: Option<ItemRef>) -> Item {
        Item {
            id,
            kind,
            name: name.into(),
            year: None,
            runtime_ms: 0,
            image: None,
            played: false,
            favorite: false,
            position_ms: 0,
            series_name: None,
            season: None,
            episode: None,
            parent,
            library_kind: None,
            imdb_rating: None,
            critic_rating: None,
        }
    }

    const LIBRARY: ItemRef = ItemRef::Synth(1);
    const SERIES: ItemRef = ItemRef::Synth(2);
    const SEASON: ItemRef = ItemRef::Synth(3);
    const EPISODE: ItemRef = ItemRef::Real(4);

    fn show() -> Fake {
        let mut fake = Fake::default();
        for i in [
            item(LIBRARY, ItemKind::Library, "TV Shows", None),
            item(SERIES, ItemKind::Series, "Breaking Bad", Some(LIBRARY)),
            item(SEASON, ItemKind::Season, "Season 1", Some(SERIES)),
            item(EPISODE, ItemKind::Episode, "Pilot", Some(SEASON)),
        ] {
            fake.details.insert(i.id, i);
        }
        fake
    }

    fn names(trail: &[Item]) -> Vec<&str> {
        trail.iter().map(|i| i.name.as_str()).collect()
    }

    #[test]
    fn an_episode_trails_back_to_its_library() {
        let fake = show();
        let trail = run(load(&fake, &CallInfo::default(), EPISODE)).unwrap();
        assert_eq!(
            names(&trail),
            ["TV Shows", "Breaking Bad", "Season 1", "Pilot"]
        );
    }

    #[test]
    fn a_library_is_a_trail_of_one() {
        let fake = show();
        let trail = run(load(&fake, &CallInfo::default(), LIBRARY)).unwrap();
        assert_eq!(names(&trail), ["TV Shows"]);
        assert_eq!(fake.asked.get(), 1);
    }

    #[test]
    fn an_unreadable_ancestor_ends_the_trail_there() {
        let mut fake = show();
        fake.details.remove(&SERIES);
        let trail = run(load(&fake, &CallInfo::default(), EPISODE)).unwrap();
        assert_eq!(names(&trail), ["Season 1", "Pilot"]);
    }

    #[test]
    fn an_unreadable_item_is_an_error() {
        let fake = show();
        assert!(run(load(&fake, &CallInfo::default(), ItemRef::Real(99))).is_err());
    }

    #[test]
    fn a_parent_loop_stops_rather_than_spinning() {
        let mut fake = Fake::default();
        let a = ItemRef::Synth(5);
        let b = ItemRef::Synth(6);
        fake.details
            .insert(a, item(a, ItemKind::Collection, "A", Some(b)));
        fake.details
            .insert(b, item(b, ItemKind::Collection, "B", Some(a)));
        let trail = run(load(&fake, &CallInfo::default(), a)).unwrap();
        assert_eq!(names(&trail), ["B", "A"]);
    }
}
