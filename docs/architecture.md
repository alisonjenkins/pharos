# pharos architecture

Brief, technical. For deeper Jellyfin-mapping rationale see [`jellyfin-mapping.md`](jellyfin-mapping.md).

## 1. Component overview

```mermaid
flowchart LR
    subgraph Clients
        JC[Jellyfin clients<br/>Finamp/Infuse/web]
        PC[Plex clients<br/>Plexamp/web]
        DUI[Dioxus web UI<br/>WASM]
    end

    subgraph pharos-server
        HTTP[actix-web router]
        JAPI[Jellyfin API scope]
        PAPI[Plex API scope]
        WS[group-sync WS hub]
        OBS[obs<br/>tracing+Prom+OTLP]
        HEALTH[health-api<br/>/healthz /readyz /info]
    end

    subgraph Adapters
        STORE[pharos-store-sqlx<br/>MediaStore impl]
        SCAN[Scanner impl]
        TRANS[Transcoder impl<br/>ffmpeg subprocess]
    end

    CORE[pharos-core<br/>domain traits + types]

    DB[(SQLite / Postgres)]
    FS[(media filesystem)]
    FFM[[ffmpeg binary]]
    OTEL[(OTLP collector)]

    JC --> HTTP
    PC --> HTTP
    DUI --> HTTP
    HTTP --> JAPI
    HTTP --> PAPI
    HTTP --> WS
    HTTP --> HEALTH
    HTTP --> OBS

    JAPI --> CORE
    PAPI --> CORE
    WS --> CORE
    HEALTH --> STORE

    STORE -. impl .-> CORE
    SCAN -. impl .-> CORE
    TRANS -. impl .-> CORE

    STORE --> DB
    SCAN --> FS
    TRANS --> FFM
    OBS --> OTEL
```

Solid arrows = runtime data path. Dashed = trait impl-of relationship. All adapters depend on `pharos-core` traits only (V12).

## 2. Crate graph

```mermaid
flowchart TB
    core[pharos-core<br/>traits + domain types<br/>no IO deps]
    sqlx[pharos-store-sqlx<br/>SqliteStore, PostgresStore<br/>sqlx + migrations]
    server[pharos-server<br/>actix-web, CLI, config, obs<br/>wires impls into routes]
    ui[pharos-ui<br/>Dioxus WASM<br/>future T24]

    sqlx --> core
    server --> core
    server --> sqlx
    ui --> core
```

Direction = `depends-on`. `pharos-core` has zero IO deps so domain logic is testable without DB/fs/network.

## 3. Request flow — Jellyfin `GET /Items/{id}`

```mermaid
sequenceDiagram
    autonumber
    actor C as Client
    participant R as actix router
    participant TL as TracingLogger<br/>middleware
    participant H as items handler
    participant S as MediaStore<br/>(SqliteStore)
    participant DB as SQLite

    C->>R: GET /Items/123<br/>X-Emby-Token: ...
    R->>TL: span(http.request)
    TL->>H: dispatch
    H->>H: auth check (token → user)
    H->>S: get(123)
    S->>DB: SELECT … WHERE id=?
    DB-->>S: row
    S-->>H: MediaItem
    H-->>R: 200 JSON (Jellyfin schema)
    R-->>C: response
    TL-->>OBS: emit span + metrics
```

Per V13: every inbound request gets a trace span; every store call gets a child span. Per V7: response shape matches Jellyfin schema byte-equivalent.

## 4. Concurrency model

```mermaid
flowchart LR
    subgraph tokio_runtime[tokio multi-thread runtime]
        direction TB
        H1[handler task]
        H2[handler task]
        H3[handler task]

        subgraph Actors
            SA[SessionActor<br/>owns Sessions]
            DR[DeviceRegistry<br/>owns Devices]
            SY[SyncSession actors<br/>one per group]
            SC[Scanner actor]
        end

        POOL[sqlx pool<br/>internal concurrency]
    end

    H1 -.mpsc.-> SA
    H2 -.mpsc.-> SY
    H3 -.mpsc.-> DR
    SC -.mpsc.-> SA

    H1 --> POOL
    H2 --> POOL
    H3 --> POOL
```

Rules (V18):
- Mutable runtime state owned by exactly one task. Handlers send `mpsc::Sender<Msg>` messages — never lock shared state.
- `sqlx::Pool` is the exception — it's lock-free internally and acts as its own concurrency primitive.
- One-shot init (obs, config) uses `OnceLock` / `Once`. No `Mutex` on request path.

## 5. Data flow — scan → store → serve

```mermaid
flowchart LR
    FS[(media roots)] -->|walk| SCAN[Scanner task]
    SCAN -->|ffprobe| FFM[[ffmpeg]]
    SCAN -->|MediaItem stream| STORE[MediaStore.put]
    STORE --> DB[(sqlite)]

    DB --> RH[API handler<br/>MediaStore.list/get]
    RH --> NET((client))

    classDef bg fill:#eef,stroke:#88a;
    class FS,DB,FFM,NET bg;
```

Per V5: scan runs in dedicated task pool; never blocks handler tasks. Per V10: each `put` is atomic — readers never see partial entries.

## 6. Boundary summary

| Boundary | Mechanism | Invariant |
|---|---|---|
| HTTP ingress | actix scope + TracingLogger | V4 (no panic), V13 (trace) |
| Domain ↔ IO | `pharos-core` traits, adapter crates | V12 |
| Cross-task state | tokio mpsc, actor pattern | V18 |
| Process ↔ ffmpeg | subprocess + structured stdout/stderr | V6 (no crash propagation) |
| Process ↔ logs | `tracing` crate only | V15 |
| Process ↔ metrics | `metrics` + Prometheus exporter | V14 |
| Filesystem ↔ HTTP | path canonicalization + auth gate | V9 (no traversal) |

Read this table alongside SPEC.md §V when reviewing a change — the table tells you which invariants any given boundary must preserve.
