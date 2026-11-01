# Data model: 009-ui-foundation

Client-side state only; server data is 017's.

## Route (typed, `Routable`)

`Home` `/` · `Browse{view, sort?, filters?, letter?}` `/browse/:view` ·
`Title{id}` `/title/:id` · `Watch{id, t?}` `/watch/:id` · `Find{q}` `/find` ·
`Together{group?}` `/together[/:group]` · `Settings{tab}` `/settings/:tab` ·
`Manage{pane}` `/manage/:pane` · `SignIn{next?}` `/signin` · `NotFound`.
Validation: every first segment passes the R4 collision test. Query
parameters round-trip losslessly (`to_url(parse(url)) == url` property test).

## HistoryEntryState

`{ key: u64, scroll: Vec<(ZoneId, f32)>, focus: Option<FocusPath> }` stored by
`key` in session storage; `history.state` holds only `key`.

## QueryKey / QueryState (in `pharos-client`)

`QueryKey = (Method, ParamsHash, UserId, ServerId)`.
`QueryState<T> = Idle | Loading{stale: Option<T>} | Ready{value: T, fetched_at} | Failed{error, stale: Option<T>}`.
Invalidation rules: an `Event` (017 data model) maps to the set of keys it
dirties (e.g. `userData{id}` dirties `detail(id)`, `resume`, `nextUp`, and any
page containing `id`).

## DesignTokens

Typed struct (owned by 010's theme model, consumed here):
`colors: {role → Rgba}`, `type_scale`, `space`, `radii`, `elevation`,
`motion: {durations, easings}`, `effects: {shader: ShaderName, params:
BoundedParams}`. Emitted as CSS custom properties `--px-<group>-<name>` and as a
GPU uniform block, both from one `TokensResolved` value per frame.

## RendererMode

`WebGl2 | Fallback{reason: NoWebGl2 | UserReduced | ReducedMotion | ContextLost | BudgetExceeded}`.
Transitions: any mode can go to `Fallback(ContextLost)`; `ContextLost` returns
to `WebGl2` on `webglcontextrestored`. Every transition is a 016 decision event.

## Sprite

`{ id, node: NodeRef, texture: TextureKey, effect: Option<EffectRef>, z: i16 }`,
visible-set membership from `IntersectionObserver`.
