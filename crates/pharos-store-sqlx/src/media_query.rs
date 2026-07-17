//! LIB-B1 — backend-agnostic SQL builder for [`MediaStore::query`].
//!
//! The WHERE-clause shape, the allowlisted ORDER BY column map, and the
//! ordered parameter list are IDENTICAL across sqlite + postgres; only the
//! placeholder token differs (`?` vs `$N`) and a handful of column casts.
//! This module assembles a [`BuiltQuery`] — the SQL fragments plus the
//! ordered [`Param`]s — once, parameterised by a placeholder formatter, so
//! each backend's `query()` is a thin bind-and-fetch wrapper.
//!
//! INJECTION SAFETY: every caller value reaches the SQL ONLY as a bound
//! [`Param`]. The single piece of interpolated text is the closed-set
//! [`SortKey`] → column expression map (`sort_key_column`), which can never
//! carry user input.

use pharos_core::{MediaFilters, MediaKind, MediaQuery, ParentFilter, SortDir, SortKey, UserId};

/// One bound parameter, in WHERE/ORDER appearance order. The backend binds
/// these positionally onto its `query_as` builder. The user-data join's
/// user id is NOT a `Param` — it binds first, in the FROM clause, directly
/// in each backend (sqlite BLOB / postgres BYTEA).
#[derive(Debug, Clone)]
pub(crate) enum Param {
    Text(String),
    Int(i64),
}

/// The assembled query: the `WHERE` body (without the `WHERE` keyword;
/// empty when no predicate), the `ORDER BY` body (always non-empty — at
/// minimum the `id` tiebreak), the optional `LIMIT`/`OFFSET` clause, and
/// the ordered params for all three in bind order.
pub(crate) struct BuiltQuery {
    pub where_sql: String,
    pub order_sql: String,
    pub limit_sql: String,
    pub params: Vec<Param>,
}

/// Map a [`SortKey`] to its fixed SQL column expression. CLOSED SET — the
/// only text interpolated into the query. Never accepts user strings.
fn sort_key_column(key: SortKey) -> &'static str {
    match key {
        // Unicode case-fold (Rust `to_lowercase` via `title_fold`), with an
        // ASCII fallback for pre-0028 rows, so accented / mixed-case titles
        // sort identically to the legacy in-memory `to_lowercase` order.
        SortKey::Name => "COALESCE(title_fold, LOWER(title))",
        SortKey::DateCreated => "created_at",
        SortKey::Runtime => "duration_ms",
        SortKey::PremiereDate => "premiere_date",
        SortKey::ProductionYear => "production_year",
        SortKey::CommunityRating => "community_rating",
        SortKey::Album => "LOWER(album)",
        SortKey::DiscNumber => "disc_number",
        SortKey::TrackNumber => "track_number",
        SortKey::AlbumArtist => "LOWER(album_artist)",
        SortKey::IndexNumber => "episode_number",
        SortKey::SeasonNumber => "season_number",
        SortKey::Id => "id",
    }
}

/// Build the query fragments + params for `q`. `ph` formats a 1-based bind
/// index into the backend's placeholder (`|_| "?"` for sqlite, `|n|
/// format!("${n}")` for postgres). `null_limit` is the backend's "no row
/// cap" token used when an OFFSET is present without a LIMIT (`-1` for
/// sqlite, `ALL` for postgres). When the user-data filter is active the
/// WHERE references `ud.*`, so the backend prepends `LEFT JOIN user_data
/// ud …` to the FROM clause (see [`needs_user_data_join`]).
pub(crate) fn build(
    q: &MediaQuery,
    mut ph: impl FnMut(usize) -> String,
    null_limit: &str,
) -> BuiltQuery {
    let mut params: Vec<Param> = Vec::new();
    let mut clauses: Vec<String> = Vec::new();

    // --- kind IN (…) ---
    if !q.kinds.is_empty() {
        let mut holes = Vec::with_capacity(q.kinds.len());
        for k in &q.kinds {
            params.push(Param::Text(kind_str(*k).to_string()));
            holes.push(ph(params.len()));
        }
        clauses.push(format!("kind IN ({})", holes.join(", ")));
    }

    // --- parent pivot ---
    if let Some(parent) = &q.parent {
        push_parent_clause(parent, &mut clauses, &mut params, &mut ph);
    }

    // --- search term (Unicode-case-insensitive substring on title) ---
    // Match against the Rust-folded `title_fold` (full Unicode lowercase),
    // falling back to ASCII `LOWER(title)` for rows scanned before the
    // 0028 column. The needle is lowercased in Rust to the same fold.
    if let Some(term) = q.search_term.as_deref() {
        let trimmed = term.trim();
        if !trimmed.is_empty() {
            let needle = format!("%{}%", like_escape(&trimmed.to_lowercase()));
            // Match the item title, the SERIES name (so searching a show name
            // like "Code Geass" finds its episodes — episode titles rarely
            // contain the series name), and artist/album for music. Each
            // column needs its own positional placeholder (`ph` is `?`).
            let cols = [
                "COALESCE(title_fold, LOWER(title))",
                "LOWER(COALESCE(series_name, ''))",
                "LOWER(COALESCE(artist, ''))",
                "LOWER(COALESCE(album, ''))",
            ];
            let mut ors = Vec::with_capacity(cols.len());
            for col in cols {
                params.push(Param::Text(needle.clone()));
                let p = ph(params.len());
                ors.push(format!("{col} LIKE {p} ESCAPE '\\'"));
            }
            clauses.push(format!("({})", ors.join(" OR ")));
        }
    }

    // --- stackable entity filters (EXISTS by wire_id) ---
    if let Some(w) = q.genre_wire_id.as_deref() {
        push_exists_entity(
            "item_genres",
            "genre_id",
            "genres",
            w,
            &mut clauses,
            &mut params,
            &mut ph,
        );
    }
    if let Some(w) = q.studio_wire_id.as_deref() {
        push_exists_entity(
            "item_studios",
            "studio_id",
            "studios",
            w,
            &mut clauses,
            &mut params,
            &mut ph,
        );
    }
    if let Some(w) = q.person_wire_id.as_deref() {
        push_exists_entity(
            "item_people",
            "person_id",
            "people",
            w,
            &mut clauses,
            &mut params,
            &mut ph,
        );
    }
    for w in &q.tag_wire_ids {
        // AND across tags — one EXISTS per requested tag (intersection).
        push_exists_entity(
            "item_tags",
            "tag_id",
            "tags",
            w,
            &mut clauses,
            &mut params,
            &mut ph,
        );
    }
    if let Some(w) = q.collection_wire_id.as_deref() {
        // Collection membership EXISTS (distinct from a Collection parent
        // pivot, which also orders by the curated sort_order).
        params.push(Param::Text(w.to_string()));
        let p = ph(params.len());
        clauses.push(format!(
            "EXISTS (SELECT 1 FROM collection_items ci JOIN collections c \
             ON c.id = ci.collection_id WHERE ci.item_id = media_items.id AND c.wire_id = {p})"
        ));
    }
    if let Some(w) = q.library_wire_id.as_deref() {
        params.push(Param::Text(w.to_string()));
        let p = ph(params.len());
        clauses.push(format!(
            "media_items.library_id IN (SELECT id FROM libraries WHERE wire_id = {p})"
        ));
    }
    // T68 — policy library restriction: intersect with the set of libraries
    // the user is allowed to browse (AND-composed with any pivot above).
    if !q.allowed_library_wire_ids.is_empty() {
        let placeholders: Vec<String> = q
            .allowed_library_wire_ids
            .iter()
            .map(|w| {
                params.push(Param::Text(w.clone()));
                ph(params.len())
            })
            .collect();
        clauses.push(format!(
            "media_items.library_id IN \
             (SELECT id FROM libraries WHERE wire_id IN ({}))",
            placeholders.join(", ")
        ));
    }
    // T68 — policy parental restriction. An item passes when its official
    // rating is within the user's max (its lowercased rating is in the allowed
    // set), OR it is unrated (NULL/empty) and unrated items aren't blocked. An
    // empty allowed set with block_unrated blocks everything rated/unrated
    // alike (max below the lowest rating).
    if let Some(parental) = &q.parental {
        let rating_ok = if parental.allowed_ratings_lc.is_empty() {
            "0 = 1".to_string()
        } else {
            let placeholders: Vec<String> = parental
                .allowed_ratings_lc
                .iter()
                .map(|r| {
                    params.push(Param::Text(r.clone()));
                    ph(params.len())
                })
                .collect();
            format!(
                "(media_items.official_rating IS NOT NULL AND media_items.official_rating <> '' \
                 AND LOWER(media_items.official_rating) IN ({}))",
                placeholders.join(", ")
            )
        };
        let unrated_ok = if parental.block_unrated {
            "0 = 1"
        } else {
            "(media_items.official_rating IS NULL OR media_items.official_rating = '')"
        };
        clauses.push(format!("({rating_ok} OR {unrated_ok})"));
    }

    // --- residual chip filters (LIB-B2) ---
    if q.filters.is_active() {
        push_residual_clauses(&q.filters, &mut clauses, &mut params, &mut ph);
    }

    // --- user-data predicates (ud.* references; join added by backend) ---
    if q.user_data.is_active() {
        push_user_data_clauses(&q.user_data, &mut clauses, &mut params, &mut ph);
    }

    let where_sql = clauses.join(" AND ");
    // ORDER BY must bind BEFORE LIMIT/OFFSET in positional-placeholder
    // backends, so build_order pushes its params ahead of build_limit's —
    // the call order here enforces that.
    let order_sql = build_order(q, &mut params, &mut ph);
    let limit_sql = build_limit(q, &mut params, &mut ph, null_limit);

    BuiltQuery {
        where_sql,
        order_sql,
        limit_sql,
        params,
    }
}

/// Whether the query references the `user_data` join (so the backend adds
/// the `LEFT JOIN`). Exposed so the backend builds the FROM clause without
/// re-deriving it.
pub(crate) fn needs_user_data_join(q: &MediaQuery) -> bool {
    q.user_data.is_active()
}

/// The user id whose `user_data` the join keys on, when active.
pub(crate) fn user_data_user(q: &MediaQuery) -> Option<UserId> {
    if q.user_data.is_active() {
        q.user_data.user
    } else {
        None
    }
}

fn build_order(
    q: &MediaQuery,
    params: &mut Vec<Param>,
    ph: &mut impl FnMut(usize) -> String,
) -> String {
    // A Collection parent pivot renders members in the join's curated
    // sort_order FIRST (the box-set browse order), then any caller sort,
    // then the id tiebreak. To ORDER BY the membership sort_order we
    // correlate a scalar subquery back to collection_items (binding the
    // pivot wire_id again — it appears once in WHERE, once here).
    let mut terms: Vec<String> = Vec::new();
    if let Some(ParentFilter::Collection { wire_id }) = &q.parent {
        params.push(Param::Text(wire_id.clone()));
        let p = ph(params.len());
        terms.push(format!(
            "(SELECT MIN(ci.sort_order) FROM collection_items ci JOIN collections c \
             ON c.id = ci.collection_id WHERE ci.item_id = media_items.id \
             AND c.wire_id = {p}) ASC"
        ));
    }
    for (key, dir) in &q.sort {
        let col = sort_key_column(*key);
        let d = match dir {
            SortDir::Asc => "ASC",
            SortDir::Desc => "DESC",
        };
        terms.push(format!("{col} {d}"));
    }
    // Always end on the stable id tiebreak so pagination never duplicates
    // or drops a row across pages.
    terms.push("id ASC".to_string());
    terms.join(", ")
}

fn build_limit(
    q: &MediaQuery,
    params: &mut Vec<Param>,
    ph: &mut impl FnMut(usize) -> String,
    null_limit: &str,
) -> String {
    match q.limit {
        Some(limit) => {
            params.push(Param::Int(i64::from(limit)));
            let lp = ph(params.len());
            params.push(Param::Int(i64::try_from(q.start_index).unwrap_or(i64::MAX)));
            let op = ph(params.len());
            format!("LIMIT {lp} OFFSET {op}")
        }
        None if q.start_index > 0 => {
            // No LIMIT but a non-zero offset. Both backends require a LIMIT
            // before OFFSET; emit the backend's "no cap" token (`-1` for
            // sqlite, `ALL` for postgres) so every matching row past the
            // offset is returned.
            params.push(Param::Int(i64::try_from(q.start_index).unwrap_or(i64::MAX)));
            let op = ph(params.len());
            format!("LIMIT {null_limit} OFFSET {op}")
        }
        None => String::new(),
    }
}

fn push_parent_clause(
    parent: &ParentFilter,
    clauses: &mut Vec<String>,
    params: &mut Vec<Param>,
    ph: &mut impl FnMut(usize) -> String,
) {
    match parent {
        ParentFilter::Library { wire_id } => {
            params.push(Param::Text(wire_id.clone()));
            let p = ph(params.len());
            clauses.push(format!(
                "media_items.library_id IN (SELECT id FROM libraries WHERE wire_id = {p})"
            ));
        }
        ParentFilter::Series { folder, name } => {
            push_series_clause(folder.as_deref(), name, None, clauses, params, ph);
        }
        ParentFilter::Season {
            folder,
            name,
            season,
        } => {
            push_series_clause(folder.as_deref(), name, Some(*season), clauses, params, ph);
        }
        ParentFilter::Artist { name } => {
            params.push(Param::Text(name.clone()));
            let a = ph(params.len());
            params.push(Param::Text(name.clone()));
            let aa = ph(params.len());
            clauses.push(format!("(artist = {a} OR album_artist = {aa})"));
        }
        ParentFilter::Album { name } => {
            params.push(Param::Text(name.clone()));
            let p = ph(params.len());
            clauses.push(format!("album = {p}"));
        }
        ParentFilter::Genre { wire_id } => push_exists_entity(
            "item_genres",
            "genre_id",
            "genres",
            wire_id,
            clauses,
            params,
            ph,
        ),
        ParentFilter::Studio { wire_id } => push_exists_entity(
            "item_studios",
            "studio_id",
            "studios",
            wire_id,
            clauses,
            params,
            ph,
        ),
        ParentFilter::Person { wire_id } => push_exists_entity(
            "item_people",
            "person_id",
            "people",
            wire_id,
            clauses,
            params,
            ph,
        ),
        ParentFilter::Tag { wire_id } => {
            push_exists_entity("item_tags", "tag_id", "tags", wire_id, clauses, params, ph)
        }
        ParentFilter::Collection { wire_id } => {
            params.push(Param::Text(wire_id.clone()));
            let p = ph(params.len());
            clauses.push(format!(
                "EXISTS (SELECT 1 FROM collection_items ci JOIN collections c \
                 ON c.id = ci.collection_id WHERE ci.item_id = media_items.id AND c.wire_id = {p})"
            ));
        }
    }
}

/// LIB-C11 — the folder-keyed Series / Season predicate. Prefers the
/// canonical `series_folder` when the API resolved one; otherwise matches
/// the bare `series_name` (legacy rows). `season` restricts to one season.
fn push_series_clause(
    folder: Option<&str>,
    name: &str,
    season: Option<u32>,
    clauses: &mut Vec<String>,
    params: &mut Vec<Param>,
    ph: &mut impl FnMut(usize) -> String,
) {
    match folder {
        Some(f) => {
            params.push(Param::Text(f.to_string()));
            let p = ph(params.len());
            clauses.push(format!("series_folder = {p}"));
        }
        None => {
            // Legacy: no folder recorded. Match by name, and only rows that
            // likewise lack a folder so a folder-keyed sibling isn't pulled
            // in under the bare-name series.
            params.push(Param::Text(name.to_string()));
            let p = ph(params.len());
            clauses.push(format!("(series_name = {p} AND series_folder IS NULL)"));
        }
    }
    if let Some(s) = season {
        params.push(Param::Int(i64::from(s)));
        let p = ph(params.len());
        clauses.push(format!("season_number = {p}"));
    }
}

/// `EXISTS (SELECT 1 FROM <join> j JOIN <entity> e ON e.id = j.<col> WHERE
/// j.item_id = media_items.id AND e.wire_id = ?)` — the indexed entity
/// pivot, shared by every name-aggregate entity (genres / studios / people
/// / tags).
fn push_exists_entity(
    join_table: &str,
    join_col: &str,
    entity_table: &str,
    wire_id: &str,
    clauses: &mut Vec<String>,
    params: &mut Vec<Param>,
    ph: &mut impl FnMut(usize) -> String,
) {
    params.push(Param::Text(wire_id.to_string()));
    let p = ph(params.len());
    clauses.push(format!(
        "EXISTS (SELECT 1 FROM {join_table} j JOIN {entity_table} e ON e.id = j.{join_col} \
         WHERE j.item_id = media_items.id AND e.wire_id = {p})"
    ));
}

/// LIB-B2 — emit the residual chip-filter clauses, mirroring the legacy
/// in-memory `filter_and_sort` semantics. Every value binds as a parameter;
/// the column names are fixed text. Width / resolution predicates treat a
/// NULL width as "no match" exactly like `width.map(..) == Some(want)`.
fn push_residual_clauses(
    f: &MediaFilters,
    clauses: &mut Vec<String>,
    params: &mut Vec<Param>,
    ph: &mut impl FnMut(usize) -> String,
) {
    // Ids= present: an empty parsed set matches nothing; a non-empty set is
    // `id IN (…)`. Checked first so an empty-but-present Ids short-circuits.
    if f.ids_present {
        if f.ids.is_empty() {
            // "you asked for nothing, you get nothing" — a never-true clause.
            clauses.push("1 = 0".to_string());
        } else {
            let mut holes = Vec::with_capacity(f.ids.len());
            for id in &f.ids {
                params.push(Param::Int(i64::try_from(*id).unwrap_or(i64::MAX)));
                holes.push(ph(params.len()));
            }
            clauses.push(format!("id IN ({})", holes.join(", ")));
        }
    }

    // IncludeItemTypes is `MediaQuery.kinds`; ExcludeItemTypes is here.
    if !f.exclude_kinds.is_empty() {
        let mut holes = Vec::with_capacity(f.exclude_kinds.len());
        for k in &f.exclude_kinds {
            params.push(Param::Text(kind_str(*k).to_string()));
            holes.push(ph(params.len()));
        }
        clauses.push(format!("kind NOT IN ({})", holes.join(", ")));
    }

    // MediaTypes projection → kind IN (…). Stacks with kinds.
    if !f.media_type_kinds.is_empty() {
        let mut holes = Vec::with_capacity(f.media_type_kinds.len());
        for k in &f.media_type_kinds {
            params.push(Param::Text(kind_str(*k).to_string()));
            holes.push(ph(params.len()));
        }
        clauses.push(format!("kind IN ({})", holes.join(", ")));
    }

    // HasSubtitles — the JSON column is NULL when there are no embedded
    // tracks (the store encodes an empty Vec as NULL), so non-empty ==
    // NOT NULL. `Some(true)` keeps non-empty; `Some(false)` keeps empty.
    if let Some(want) = f.has_subtitles {
        if want {
            clauses.push("subtitle_tracks_json IS NOT NULL".to_string());
        } else {
            clauses.push("subtitle_tracks_json IS NULL".to_string());
        }
    }

    // Resolution / width — NULL width never matches a Some(want) (mirrors
    // `width.map(..) == Some(want)`), so each clause requires width NOT NULL.
    if let Some(want) = f.is_4k {
        params.push(Param::Int(3840));
        let p = ph(params.len());
        if want {
            clauses.push(format!("(width IS NOT NULL AND width >= {p})"));
        } else {
            clauses.push(format!("(width IS NOT NULL AND width < {p})"));
        }
    }
    if let Some(want) = f.is_hd {
        params.push(Param::Int(1280));
        let lo = ph(params.len());
        params.push(Param::Int(3840));
        let hi = ph(params.len());
        if want {
            clauses.push(format!(
                "(width IS NOT NULL AND width >= {lo} AND width < {hi})"
            ));
        } else {
            clauses.push(format!(
                "(width IS NOT NULL AND (width < {lo} OR width >= {hi}))"
            ));
        }
    }
    if let Some(want) = f.is_3d {
        // No 3D detection: true → nothing, false → everything.
        if want {
            clauses.push("1 = 0".to_string());
        }
        // want == false adds no clause (everything passes).
    }
    if let Some(min) = f.min_width {
        params.push(Param::Int(i64::from(min)));
        let p = ph(params.len());
        clauses.push(format!("(width IS NOT NULL AND width >= {p})"));
    }
    if let Some(max) = f.max_width {
        params.push(Param::Int(i64::from(max)));
        let p = ph(params.len());
        clauses.push(format!("(width IS NOT NULL AND width <= {p})"));
    }

    // Episode index bounds — NULL episode_number never matches (mirrors
    // `.map(..).unwrap_or(false)`).
    if let Some(min) = f.min_index_number {
        params.push(Param::Int(i64::from(min)));
        let p = ph(params.len());
        clauses.push(format!(
            "(episode_number IS NOT NULL AND episode_number >= {p})"
        ));
    }
    if let Some(max) = f.max_index_number {
        params.push(Param::Int(i64::from(max)));
        let p = ph(params.len());
        clauses.push(format!(
            "(episode_number IS NOT NULL AND episode_number <= {p})"
        ));
    }

    // NameStartsWith — Unicode-case-insensitive title prefix (title_fold).
    if let Some(prefix) = f.name_starts_with.as_deref() {
        if !prefix.is_empty() {
            params.push(Param::Text(format!(
                "{}%",
                like_escape(&prefix.to_lowercase())
            )));
            let p = ph(params.len());
            clauses.push(format!(
                "COALESCE(title_fold, LOWER(title)) LIKE {p} ESCAPE '\\'"
            ));
        }
    }

    // NameLessThan — strict Unicode-case-insensitive upper bound on the title.
    if let Some(bound) = f.name_less_than.as_deref() {
        if !bound.is_empty() {
            params.push(Param::Text(bound.to_lowercase()));
            let p = ph(params.len());
            clauses.push(format!("COALESCE(title_fold, LOWER(title)) < {p}"));
        }
    }

    // Component-boundary path-prefix scope (media-root ParentId fallback,
    // no typed-library entity). `path = p OR path LIKE p || '/%'` so a
    // sibling like `/media/movies-4k` never matches `/media/movies`.
    if let Some(prefix) = f.path_prefix.as_deref() {
        params.push(Param::Text(prefix.to_string()));
        let exact = ph(params.len());
        params.push(Param::Text(format!("{}/%", like_escape(prefix))));
        let under = ph(params.len());
        clauses.push(format!("(path = {exact} OR path LIKE {under} ESCAPE '\\')"));
    }

    // LEGACY ParentId=genre fallback: token-membership in the `|`/`,`-split
    // probe.genre field (case-folded, surrounding spaces stripped to mirror
    // `split_genre_field`'s trim). Normalize the column to ",tok,tok," and
    // test for ",<token>,".
    if let Some(token) = f.genre_probe_token.as_deref() {
        params.push(Param::Text(format!(
            "%,{},%",
            like_escape(&token.to_lowercase())
        )));
        let p = ph(params.len());
        clauses.push(format!(
            "(genre IS NOT NULL AND \
             (',' || REPLACE(REPLACE(REPLACE(LOWER(genre), '|', ','), ', ', ','), ' ,', ',') || ',') \
             LIKE {p} ESCAPE '\\')"
        ));
    }

    // Genres= — the LEGACY probe-column filter: whole probe.genre string
    // (case-folded) must be in the requested set.
    if !f.genre_probe_names.is_empty() {
        let mut holes = Vec::with_capacity(f.genre_probe_names.len());
        for name in &f.genre_probe_names {
            params.push(Param::Text(name.to_lowercase()));
            holes.push(ph(params.len()));
        }
        clauses.push(format!(
            "(genre IS NOT NULL AND LOWER(genre) IN ({}))",
            holes.join(", ")
        ));
    }
}

fn push_user_data_clauses(
    ud: &pharos_core::UserDataQuery,
    clauses: &mut Vec<String>,
    params: &mut Vec<Param>,
    ph: &mut impl FnMut(usize) -> String,
) {
    // A missing user_data row → defaults (unplayed, not favourite, pos 0).
    // The backend LEFT JOINs `user_data ud`, so COALESCE the NULLs.
    if let Some(want) = ud.is_favorite {
        params.push(Param::Int(i64::from(want)));
        let p = ph(params.len());
        clauses.push(format!("COALESCE(ud.is_favorite, 0) = {p}"));
    }
    if let Some(want) = ud.is_played {
        params.push(Param::Int(i64::from(want)));
        let p = ph(params.len());
        clauses.push(format!("COALESCE(ud.played, 0) = {p}"));
    }
    if ud.is_resumable {
        clauses.push(
            "(COALESCE(ud.played, 0) = 0 AND COALESCE(ud.last_played_position_ticks, 0) > 0)"
                .to_string(),
        );
    }
}

/// Escape LIKE wildcards in a user search term so `%`/`_` are literal. The
/// query MUST use `ESCAPE '\'`.
pub(crate) fn like_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '\\' | '%' | '_') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

fn kind_str(k: MediaKind) -> &'static str {
    k.as_str()
}
