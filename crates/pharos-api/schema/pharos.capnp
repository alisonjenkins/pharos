# pharos native API — initial cut (017). Append-only: never renumber, retype or
# remove an ordinal (FR-009). Fields and params annotated $secret are redacted
# from every log, span and tap (V8, 017 FR-011).
#
# Conventions (data-model.md):
#  - every method takes `ctx :CallContext` first (FR-006, V173);
#  - every method returns a result union of ok and Error (FR-007);
#  - optional scalars use an explicit union, never a zero default.
@0xc54d2414cbb03e28;

annotation secret(field, param) :Void;

# Bumped when the schema changes; served as ServerInfo.schemaVersion so a client
# can tell which methods a server knows (FR-009).
const schemaVersion :UInt32 = 2;

# ---- ids and shared types -------------------------------------------------
struct ItemId {                # mirrors pharos_jellyfin_api::WireId
  union {
    real  @0 :UInt64;          # pharos_core::MediaId, a store row
    synth @1 :UInt64;          # synthesised group (series/season/album/artist)
  }
}
struct UserId        { uuid @0 :Data; }   # 16 bytes, pharos_core::auth::UserId(Uuid)
struct GroupId       { value @0 :Text; }
struct PlaySessionId { value @0 :Text; }
struct DeviceId      { value @0 :Text; }

struct OptU32 { union { none @0 :Void; some @1 :UInt32; } }
struct OptU16 { union { none @0 :Void; some @1 :UInt16; } }
struct StreamChoice { union { default @0 :Void; none @1 :Void; index @2 :UInt32; } }

struct CallContext {
  traceparent   @0 :Text;      # W3C trace context (016 FR-001)
  uiRoute       @1 :Text;      # bounded, validated server-side
  uiAction      @2 :Text;
  clientVersion @3 :Text;
  device        @4 :DeviceId;
  schemaVersion @5 :UInt32;    # the schemaVersion the client was built against; 0 = unset
}

struct Error {
  reason  @0 :Text;            # the cause value, never a bare class
  traceId @1 :Text;
  kind :union {
    notFound     @2 :Text;     # the id that was not found
    notPermitted @3 :Text;     # what was refused
    invalid      @4 :Invalid;
    limit        @5 :Limit;
    revoked      @6 :Void;
    unavailable  @7 :Text;     # the dependency
    other        @8 :Void;
  }
  struct Invalid { field @0 :Text; value @1 :Text; }
  struct Limit   { name @0 :Text; value @1 :UInt64; max @2 :UInt64; }
}

struct Done { union { ok @0 :Void; error @1 :Error; } }   # result for methods with no payload

enum Kind { movie @0; episode @1; audio @2; book @3; series @4; season @5; album @6; artist @7; person @8; collection @9; playlist @10; channel @11; }

struct ImageRef { kind @0 :Text; tag @1 :Text; url @2 :Text; blurhash @3 :Text; }

struct UserState { played @0 :Bool; favorite @1 :Bool; positionMs @2 :UInt64; playCount @3 :UInt32; }

struct ItemSummary {
  id @0 :ItemId; kind @1 :Kind; name @2 :Text; year @3 :OptU16;
  runtimeMs @4 :UInt64; images @5 :List(ImageRef); userState @6 :UserState;
  parentId @7 :ItemId; seriesName @8 :Text;
  indexNumber @9 :OptU32; parentIndexNumber @10 :OptU32;   # season 0 / episode 0 are real
}

struct ItemPage { items @0 :List(ItemSummary); total @1 :UInt32; start @2 :UInt32; }
struct ItemsResult { union { page @0 :ItemPage; error @1 :Error; } }

# ---- bootstrap and session -----------------------------------------------
# Browsers: POST /pharos/session sets the HttpOnly cookie (research R6), then
# the WebSocket upgrade carries it and `resume` with an empty token uses it.
# Native apps: `login` / `quickConnect` return a device token they store and
# later pass to `resume`.
interface Bootstrap {
  serverInfo @0 (ctx :CallContext) -> (result :ServerInfoResult);
  resume @1 (ctx :CallContext, deviceToken :Text $secret) -> (result :ResumeResult);
  login @2 (ctx :CallContext, name :Text, password :Text $secret) -> (result :LoginResult);
  quickConnectInitiate @3 (ctx :CallContext) -> (result :QuickConnectResult);
  quickConnectPoll @4 (ctx :CallContext, secret :Text $secret) -> (result :LoginResult);
  submitAnonymous @5 (ctx :CallContext, events :List(TelemetryEvent)) -> (result :Done);
  # 016 FR-005a; only offered when ServerInfo.anonymousErrors is true.
}
struct ServerInfo { name @0 :Text; version @1 :Text; schemaVersion @2 :UInt32; anonymousErrors @3 :Bool; }
struct ServerInfoResult { union { info @0 :ServerInfo; error @1 :Error; } }
# Not a union: capnp-rust generates no pipeline accessor for a capability
# inside one, and `resume → session.library() → library.home()` must be one
# flight (SC-001, research R1). A refused resume still returns a `session`, one
# whose every call fails `Error.kind.revoked`; `status` says why.
struct ResumeResult { session @0 :Session; status @1 :Done; }
struct LoginResult { union { ok @0 :LoginOk; error @1 :Error; } }
struct LoginOk { session @0 :Session; deviceToken @1 :Text $secret; }
struct QuickConnectResult { union { pending @0 :QuickConnectPending; error @1 :Error; } }
struct QuickConnectPending { code @0 :Text; secret @1 :Text $secret; }

struct Me { id @0 :UserId; name @1 :Text; isAdmin @2 :Bool; }
struct MeResult { union { me @0 :Me; error @1 :Error; } }

interface Session {
  me @0 (ctx :CallContext) -> (result :MeResult);
  library @1 (ctx :CallContext) -> (library :Library);
  userData @2 (ctx :CallContext) -> (userData :UserData);
  playback @3 (ctx :CallContext) -> (playback :Playback);
  events @4 (ctx :CallContext) -> (events :Events);
  prefs @5 (ctx :CallContext) -> (prefs :Prefs);
  telemetry @6 (ctx :CallContext) -> (telemetry :Telemetry);
  admin @7 (ctx :CallContext) -> (result :AdminResult);
  logout @8 (ctx :CallContext) -> (result :Done);
  # Capability getters return a capability, never fail; a revoked session makes
  # every call on the returned capability fail with Error.kind.revoked.
  # Group, Sessions, Queue, RemoteSources arrive with specs 013/014 (append-only).
}
struct AdminResult { union { admin @0 :Admin; error @1 :Error; } }

# ---- library --------------------------------------------------------------
struct ItemQuery {
  parentId @0 :ItemId;         # pointer field: absent means "no parent filter"
  kinds @1 :List(Kind); sortBy @2 :Text; descending @3 :Bool;
  nameStartsWith @4 :Text; filters @5 :List(Text); start @6 :UInt32; limit @7 :UInt32;
}

# Home is aggregated server-side: pipelining only follows capabilities, not
# data, so "views, then latest per view" cannot be one flight client-side.
struct HomeRail { view @0 :ItemSummary; latest @1 :List(ItemSummary); }
struct Home { rails @0 :List(HomeRail); resume @1 :List(ItemSummary); nextUp @2 :List(ItemSummary); }
struct HomeResult { union { home @0 :Home; error @1 :Error; } }

interface Library {
  views @0 (ctx :CallContext) -> (result :ItemsResult);
  latest @1 (ctx :CallContext, parentId :ItemId, limit :UInt32) -> (result :ItemsResult);
  resume @2 (ctx :CallContext, kinds :List(Kind), limit :UInt32) -> (result :ItemsResult);
  nextUp @3 (ctx :CallContext, limit :UInt32) -> (result :ItemsResult);
  query @4 (ctx :CallContext, q :ItemQuery) -> (result :ItemsResult);
  detail @5 (ctx :CallContext, id :ItemId) -> (result :DetailResult);
  search @6 (ctx :CallContext, text :Text, limit :UInt32) -> (result :ItemsResult);
  home @7 (ctx :CallContext, latestPerView :UInt32, resumeLimit :UInt32, nextUpLimit :UInt32) -> (result :HomeResult);
}
struct ItemDetail { summary @0 :ItemSummary; overview @1 :Text; genres @2 :List(Text); chapters @3 :List(Chapter); }
struct Chapter { name @0 :Text; startMs @1 :UInt64; }
struct DetailResult { union { detail @0 :ItemDetail; error @1 :Error; } }

interface UserData {
  setPlayed @0 (ctx :CallContext, id :ItemId, played :Bool) -> (result :UserStateResult);
  setFavorite @1 (ctx :CallContext, id :ItemId, favorite :Bool) -> (result :UserStateResult);
}
struct UserStateResult { union { state @0 :UserState; error @1 :Error; } }

# ---- playback -------------------------------------------------------------
struct Capabilities { videoCodecs @0 :List(Text); audioCodecs @1 :List(Text); containers @2 :List(Text); maxBitrate @3 :UInt64; hdr @4 :Bool; }
struct Decision {
  method @0 :Text;             # direct_play | direct_stream | transcode
  directPlayBlock @1 :Text;    # same label values as pharos_playback_decision_total
  downgrade @2 :Text;
  streamUrl @3 :Text;          # HLS master or direct stream (HTTP, 017 §Scope)
  playSession @4 :PlaySessionId;
}
struct DecisionResult { union { decision @0 :Decision; error @1 :Error; } }
interface PlaybackGrant {
  decision @0 (ctx :CallContext) -> (result :DecisionResult);
  report @1 (ctx :CallContext, positionMs :UInt64, paused :Bool) -> (result :Done);
  stop @2 (ctx :CallContext, positionMs :UInt64) -> (result :Done);
}
struct NegotiateResult { union { grant @0 :PlaybackGrant; error @1 :Error; } }
interface Playback {
  negotiate @0 (ctx :CallContext, id :ItemId, caps :Capabilities, audio :StreamChoice, subtitle :StreamChoice) -> (result :NegotiateResult);
}

# ---- push -----------------------------------------------------------------
# seq is per USER event stream, monotonic across connections and replicas
# (data-model.md §Events). The server keeps a bounded replay window; resuming
# from a seq older than the window yields one `resync` event.
struct Event {
  seq @0 :UInt64;
  union {
    userData @1 :UserDataChanged;
    libraryChanged @2 :LibraryChanged;
    task @3 :TaskProgress;
    resync @4 :Void;           # replay window exceeded: refetch everything cached
  }
  struct UserDataChanged { id @0 :ItemId; state @1 :UserState; }
  struct LibraryChanged { added @0 :List(ItemId); updated @1 :List(ItemId); removed @2 :List(ItemId); }
  struct TaskProgress { id @0 :Text; progress @1 :Float32; state @2 :Text; }
}
interface EventSink { push @0 (ctx :CallContext, events :List(Event)) -> (result :Done); }
interface Subscription { }      # dropping it unsubscribes
struct SubscribeResult { union { subscription @0 :Subscription; error @1 :Error; } }
interface Events {
  subscribe @0 (ctx :CallContext, sink :EventSink, topics :List(Text), afterSeq :UInt64) -> (result :SubscribeResult);
}

# ---- prefs, telemetry, admin (bodies grow with 010/011/015/016) -----------
struct PrefResult { union { value @0 :Data; notSet @1 :Void; error @2 :Error; } }
interface Prefs {
  get @0 (ctx :CallContext, key :Text) -> (result :PrefResult);
  set @1 (ctx :CallContext, key :Text, value :Data) -> (result :Done);
}

enum EventKind {               # closed catalogue, 016 contracts/events.md
  panic @0; jsError @1; rpcFailed @2; httpFailed @3; wsClosed @4; rendererMode @5;
  webglContextLost @6; longFrame @7; routePaint @8; slowInteraction @9;
  playbackDecision @10; playbackStall @11; playbackError @12; qualitySwitch @13;
  seek @14; themeFallback @15; focusRecovered @16; cacheServed @17; cspViolation @18;
}
enum Level { error @0; warn @1; info @2; }
struct TelemetryEvent {
  kind @0 :EventKind; level @1 :Level; monotonicMs @2 :UInt64; wallMs @3 :UInt64;
  traceId @4 :Text; spanId @5 :Text; route @6 :Text; action @7 :Text;
  item @8 :ItemId; playSession @9 :PlaySessionId; group @10 :GroupId;   # pointer fields: absent when n/a
  repeat @11 :UInt32; fields @12 :List(Field);
  struct Field { key @0 :Text; value @1 :Text; }
}
struct ClientSpan {
  traceId @0 :Text; spanId @1 :Text; parentSpanId @2 :Text; name @3 :Text;
  startWallMs @4 :UInt64; durationMs @5 :UInt64; ok @6 :Bool; message @7 :Text;
  attrs @8 :List(TelemetryEvent.Field);
}
struct ReportResult { union { reportId @0 :Text; error @1 :Error; } }
interface Telemetry {
  submit @0 (ctx :CallContext, events :List(TelemetryEvent), spans :List(ClientSpan)) -> (result :Done);
  report @1 (ctx :CallContext, note :Text, bundle :Data) -> (result :ReportResult);
}
struct CountResult { union { count @0 :UInt32; error @1 :Error; } }
interface Admin {
  sessions @0 (ctx :CallContext) -> (result :CountResult);
}
