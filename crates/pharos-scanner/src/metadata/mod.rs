//! LIB-D1 — local-first metadata resolution.
//!
//! A [`MetadataResolver`] holds an ordered set of boxed
//! [`MetadataProvider`]s (local NFO, sidecar artwork, filename
//! conventions today; online providers later) and merges their results
//! into one [`MetadataResult`] per item. The provider trait itself is
//! declared in `pharos-core` (V12: IO-free); the IO-bearing impls (NFO
//! XML read, sidecar `stat`) land in sibling modules under here.
//!
//! ## Merge rules
//! Providers are consulted highest-[`priority`] first.
//! - **Scalar `Option` fields** (`title`, `overview`, ratings, years,
//!   each `provider_ids` slot): first `Some` wins. A higher-priority
//!   provider's value is never overwritten by a lower one — so a
//!   user-curated local NFO beats an online provider.
//! - **`Vec` fields** (`genres` / `studios` / `people` / `tags` /
//!   `collections` / `artwork`): union across providers, de-duplicated,
//!   keeping the first occurrence (priority order) for a stable result.
//!
//! ## V6 isolation
//! A provider whose [`fetch`] returns `Err` is logged at `warn` and
//! skipped; the merge proceeds with the remaining providers. A malformed
//! NFO / missing sidecar / IO error on one source never aborts metadata
//! resolution for the item (nor the scan).
//!
//! [`priority`]: pharos_core::MetadataProvider::priority
//! [`fetch`]: pharos_core::MetadataProvider::fetch

pub mod filename;
pub mod nfo;
pub mod sidecar;

use std::collections::HashSet;

use pharos_core::{
    ArtworkRef, MediaKind, MetadataProvider, MetadataRequest, MetadataResult, PersonRef,
    ProviderIds,
};

/// LIB-D1 — priority-ordered merge of one or more [`MetadataProvider`]s.
///
/// Construct via [`new`](Self::new) / [`with_provider`](Self::with_provider)
/// (or [`from_providers`](Self::from_providers)); the resolver sorts its
/// providers by descending priority once at registration so [`resolve`]
/// can walk them in merge order without re-sorting per item.
///
/// [`resolve`]: Self::resolve
#[derive(Default)]
pub struct MetadataResolver {
    /// Boxed providers, kept sorted by descending priority.
    providers: Vec<Box<dyn ErasedProvider>>,
}

impl MetadataResolver {
    /// An empty resolver. [`resolve`](Self::resolve) on it yields
    /// `MetadataResult::default()` (the scanner then keeps its
    /// filename-derived fields).
    pub fn new() -> Self {
        Self {
            providers: Vec::new(),
        }
    }

    /// Register one provider, keeping the internal order sorted by
    /// descending [`priority`](MetadataProvider::priority). Ties preserve
    /// insertion order (stable sort) so registration order breaks
    /// equal-priority ties deterministically.
    pub fn with_provider<P>(mut self, provider: P) -> Self
    where
        P: MetadataProvider + 'static,
    {
        self.providers.push(Box::new(provider));
        // Highest priority first; stable so equal priorities keep
        // registration order.
        self.providers
            .sort_by_key(|p| std::cmp::Reverse(p.priority()));
        self
    }

    /// Build a resolver from an iterator of boxed providers (e.g. a
    /// config-driven set). Sorted by descending priority once.
    pub fn from_providers(providers: impl IntoIterator<Item = Box<dyn ErasedProvider>>) -> Self {
        let mut providers: Vec<Box<dyn ErasedProvider>> = providers.into_iter().collect();
        providers.sort_by_key(|p| std::cmp::Reverse(p.priority()));
        Self { providers }
    }

    /// Number of registered providers (test/observability helper).
    pub fn provider_count(&self) -> usize {
        self.providers.len()
    }

    /// Resolve + merge metadata for `req`. Consults every provider that
    /// [`supports`](MetadataProvider::supports) the item's kind, in
    /// descending-priority order, and field-merges their results (see the
    /// module docs). A provider returning `Err` is logged + skipped (V6);
    /// the merge never fails — when nothing matches, the default
    /// (all-empty) result is returned.
    pub async fn resolve(&self, req: &MetadataRequest<'_>) -> MetadataResult {
        let mut merged = MetadataResult::default();
        for provider in &self.providers {
            if !provider.supports(req.kind) {
                continue;
            }
            match provider.fetch(req).await {
                Ok(result) => merge_into(&mut merged, result),
                Err(err) => {
                    tracing::warn!(
                        provider = provider.name(),
                        path = %req.path.display(),
                        error = %err,
                        "metadata provider failed; skipping (scan continues)"
                    );
                }
            }
        }
        merged
    }
}

/// Fold `next` into `acc` per the merge rules. Scalars: keep the existing
/// (higher-priority) `Some`, else take `next`'s. Vecs: append the items
/// not already present (stable de-dupe).
fn merge_into(acc: &mut MetadataResult, next: MetadataResult) {
    let MetadataResult {
        title,
        overview,
        tagline,
        production_year,
        premiere_date,
        community_rating,
        critic_rating,
        official_rating,
        genres,
        studios,
        people,
        tags,
        collections,
        production_locations,
        trailers,
        provider_ids,
        artwork,
    } = next;

    fill(&mut acc.title, title);
    fill(&mut acc.overview, overview);
    fill(&mut acc.tagline, tagline);
    fill(&mut acc.production_year, production_year);
    fill(&mut acc.premiere_date, premiere_date);
    fill(&mut acc.community_rating, community_rating);
    fill(&mut acc.critic_rating, critic_rating);
    fill(&mut acc.official_rating, official_rating);

    merge_provider_ids(&mut acc.provider_ids, provider_ids);

    extend_dedup(&mut acc.genres, genres);
    extend_dedup(&mut acc.studios, studios);
    extend_dedup(&mut acc.tags, tags);
    extend_dedup(&mut acc.collections, collections);
    extend_dedup(&mut acc.production_locations, production_locations);
    extend_dedup(&mut acc.trailers, trailers);
    extend_people(&mut acc.people, people);
    extend_artwork(&mut acc.artwork, artwork);
}

/// First `Some` wins: only overwrite when the accumulator is still empty.
fn fill<T>(slot: &mut Option<T>, value: Option<T>) {
    if slot.is_none() {
        *slot = value;
    }
}

/// Per-slot first-`Some`-wins merge of provider ids.
fn merge_provider_ids(acc: &mut ProviderIds, next: ProviderIds) {
    fill(&mut acc.tmdb, next.tmdb);
    fill(&mut acc.tvdb, next.tvdb);
    fill(&mut acc.imdb, next.imdb);
    fill(&mut acc.mbid, next.mbid);
}

/// Append the strings from `next` not already in `acc` (stable de-dupe,
/// case-sensitive — genre/studio casing is meaningful on the wire).
fn extend_dedup(acc: &mut Vec<String>, next: Vec<String>) {
    let mut seen: HashSet<String> = acc.iter().cloned().collect();
    for s in next {
        if seen.insert(s.clone()) {
            acc.push(s);
        }
    }
}

/// De-dupe people on `(name, kind, character)` so the same actor credited
/// by two providers isn't doubled, while distinct characters survive.
fn extend_people(acc: &mut Vec<PersonRef>, next: Vec<PersonRef>) {
    for p in next {
        let dup = acc
            .iter()
            .any(|e| e.name == p.name && e.kind == p.kind && e.character == p.character);
        if !dup {
            acc.push(p);
        }
    }
}

/// De-dupe artwork on `(role, source)` so two providers offering the same
/// poster don't double it, while distinct backdrops survive.
fn extend_artwork(acc: &mut Vec<ArtworkRef>, next: Vec<ArtworkRef>) {
    for a in next {
        if !acc.iter().any(|e| e.role == a.role && e.source == a.source) {
            acc.push(a);
        }
    }
}

/// Object-safe shim over [`MetadataProvider`]. The trait's `fetch` returns
/// `impl Future` (RPITIT, not object-safe), so the resolver stores
/// `Box<dyn ErasedProvider>` whose `fetch` returns a boxed future. A
/// blanket impl adapts any [`MetadataProvider`].
pub trait ErasedProvider: Send + Sync {
    fn name(&self) -> &'static str;
    fn priority(&self) -> i32;
    fn supports(&self, kind: MediaKind) -> bool;
    fn fetch<'a>(
        &'a self,
        req: &'a MetadataRequest<'a>,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = pharos_core::DomainResult<MetadataResult>> + Send + 'a,
        >,
    >;
}

impl<T: MetadataProvider> ErasedProvider for T {
    fn name(&self) -> &'static str {
        MetadataProvider::name(self)
    }
    fn priority(&self) -> i32 {
        MetadataProvider::priority(self)
    }
    fn supports(&self, kind: MediaKind) -> bool {
        MetadataProvider::supports(self, kind)
    }
    fn fetch<'a>(
        &'a self,
        req: &'a MetadataRequest<'a>,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = pharos_core::DomainResult<MetadataResult>> + Send + 'a,
        >,
    > {
        Box::pin(MetadataProvider::fetch(self, req))
    }
}

#[cfg(test)]
mod tests;
