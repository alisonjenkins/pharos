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
const schemaVersion :UInt32 = 28;

# ---- ids and shared types -------------------------------------------------
struct ItemId {                # mirrors pharos_jellyfin_api::WireId
  union {
    real  @0 :UInt64;          # pharos_core::MediaId, a store row
    synth @1 :UInt64;          # synthesised group (series/season/album/artist)
  }
}
struct UserId        { uuid @0 :Data; }   # 16 bytes, pharos_core::auth::UserId(Uuid)
struct GroupId       { value @0 :Text; }
struct PlaySessionId { value @0 :Text $secret; }   # a grant's capability: debug dumps redact it
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

enum Kind { movie @0; episode @1; audio @2; book @3; series @4; season @5; album @6; artist @7; person @8; collection @9; playlist @10; channel @11; library @12; }
# What a `library` item holds; mirrors pharos_core::LibraryKind.
enum LibraryKind { mixed @0; movies @1; tvShows @2; music @3; books @4; }

# How a video packs two eyes into one frame; mirrors pharos_core::Video3DFormat.
# `none` is ordinary 2D video, and what an older server's zero reads as.
enum Stereo3d { none @0; halfSideBySide @1; fullSideBySide @2; halfTopAndBottom @3; fullTopAndBottom @4; mvc @5; }

struct ImageRef { kind @0 :Text; tag @1 :Text; url @2 :Text; blurhash @3 :Text; }

struct UserState { played @0 :Bool; favorite @1 :Bool; positionMs @2 :UInt64; playCount @3 :UInt32; }

struct ItemSummary {
  id @0 :ItemId; kind @1 :Kind; name @2 :Text; year @3 :OptU16;
  runtimeMs @4 :UInt64; images @5 :List(ImageRef); userState @6 :UserState;
  parentId @7 :ItemId; seriesName @8 :Text;
  indexNumber @9 :OptU32; parentIndexNumber @10 :OptU32;   # season 0 / episode 0 are real
  libraryKind @11 :LibraryKind;                            # kind == library only
  imdbRating @12 :OptU16;      # IMDb score in tenths: 78 is 7.8
  criticRating @13 :OptU16;    # Rotten Tomatoes Tomatometer, a percentage
  artist @14 :Text;            # a song's artists for its card, joined; empty for anything else. Since schemaVersion 25.
  stereo @15 :Stereo3d;        # the primary's packing, else the first 3D version's. Since schemaVersion 28.
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

struct Me {
  id @0 :UserId; name @1 :Text; isAdmin @2 :Bool;
  avatarTag @3 :Text;          # empty: no avatar; else GET /UserImage?userId=&tag=
}
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
  group @9 (ctx :CallContext) -> (group :Group);
  # The user's own password: `current` is checked through the login throttle,
  # an empty `new` is refused, and the user's other sessions are signed out.
  changePassword @10 (ctx :CallContext, current :Text $secret, new :Text $secret) -> (result :Done);
  # The user's own avatar: any PNG, JPEG, WebP or GIF up to 768 KiB, stored
  # cropped square and re-encoded by the server; answers the new tag.
  setAvatar @11 (ctx :CallContext, image :Data) -> (result :AvatarResult);
  removeAvatar @12 (ctx :CallContext) -> (result :Done);
  # Approves a pending Quick Connect code shown on another device, which then
  # signs in as this user; an unknown or expired code answers notFound.
  quickConnectAuthorize @13 (ctx :CallContext, code :Text) -> (result :Done);
  # Mint a narrow token for native image loaders and media players that
  # cannot send the session cookie (025, ADR 0058). Valid only on GET/HEAD
  # media and image endpoints; expires within server-configured TTL.
  mediaToken @14 (ctx :CallContext) -> (result :MediaTokenResult);
  # A short-lived token for the web UI's Manage pages, which call the admin
  # endpoints of the Jellyfin API with a header (ADR 0082). Admins only: any
  # other user is refused notPermitted. Full scope, issued as a child of this
  # session's token, so signing out ends it; lives one hour at most.
  manageToken @15 (ctx :CallContext) -> (result :MediaTokenResult);
  # Capability getters return a capability, never fail; a revoked session makes
  # every call on the returned capability fail with Error.kind.revoked.
  # Sessions, Queue, RemoteSources arrive with specs 013/014 (append-only).
}
struct AdminResult { union { admin @0 :Admin; error @1 :Error; } }
struct AvatarResult { union { tag @0 :Text; error @1 :Error; } }
struct MediaToken {
  token       @0 :Text $secret;
  expiresAtMs @1 :UInt64;
}
struct MediaTokenResult {
  union {
    token @0 :MediaToken;
    error @1 :Error;
  }
}

# ---- library --------------------------------------------------------------
# `kinds` naming exactly one kind pharos only synthesises (series, season,
# album, artist) lists those groups under `parentId`, folded out of the
# stored rows there; seasons need a series parent (V230). Any other list
# filters stored rows by kind.
struct ItemQuery {
  parentId @0 :ItemId;         # pointer field: absent means "no parent filter"
  kinds @1 :List(Kind); sortBy @2 :Text; descending @3 :Bool;
  nameStartsWith @4 :Text; filters @5 :List(Text); start @6 :UInt32; limit @7 :UInt32;
  genre @8 :Text;              # one genre's name; empty means every genre
  stereo @9 :StereoFilter;     # 3D titles only, 2D only, or both. Since schemaVersion 28.
}
enum StereoFilter { any @0; only3d @1; only2d @2; }

# Where each section starts in a listing, in the listing's own order: a
# section's `start` is the offset of its first item, so a client can page
# straight to it. In name order a section is a first letter, `#` gathering
# names that do not start with a letter A to Z; in a date order (the first
# `sortBy` key a date) it is a four-figure year, `#` gathering items with no
# such date. Only sections the listing holds appear. `genres` are the genres found under
# the query's parent and kind, ignoring its own genre and filters.
struct LetterStart { letter @0 :Text; start @1 :UInt32; }
struct ListIndex { letters @0 :List(LetterStart); genres @1 :List(Text); }
struct IndexResult { union { index @0 :ListIndex; error @1 :Error; } }

# Home is aggregated server-side: pipelining only follows capabilities, not
# data, so "views, then latest per view" cannot be one flight client-side.
struct HomeRail { view @0 :ItemSummary; latest @1 :List(ItemSummary); }
# `resume` holds films and episodes, `listening` music; `resumeLimit` sizes both.
struct Home { rails @0 :List(HomeRail); resume @1 :List(ItemSummary); nextUp @2 :List(ItemSummary); listening @3 :List(ItemSummary); }
struct HomeResult { union { home @0 :Home; error @1 :Error; } }

interface Library {
  views @0 (ctx :CallContext) -> (result :ItemsResult);
  latest @1 (ctx :CallContext, parentId :ItemId, limit :UInt32) -> (result :ItemsResult);
  resume @2 (ctx :CallContext, kinds :List(Kind), limit :UInt32) -> (result :ItemsResult);
  nextUp @3 (ctx :CallContext, limit :UInt32) -> (result :ItemsResult);
  query @4 (ctx :CallContext, q :ItemQuery) -> (result :ItemsResult);
  detail @5 (ctx :CallContext, id :ItemId, version :Text) -> (result :DetailResult);
  # `version`: a Version.id; its tracks, fonts and subtitle URLs are that file's. Empty is the primary.
  search @6 (ctx :CallContext, text :Text, limit :UInt32) -> (result :ItemsResult);
  home @7 (ctx :CallContext, latestPerView :UInt32, resumeLimit :UInt32, nextUpLimit :UInt32) -> (result :HomeResult);
  index @8 (ctx :CallContext, q :ItemQuery) -> (result :IndexResult);   # `q.start` and `q.limit` are ignored
  # wantWords is set by a client showing karaoke; it lets the server ask the aligner for word times.
  lyrics @9 (ctx :CallContext, id :ItemId, wantWords :Bool) -> (result :LyricsResult);
  mix @10 (ctx :CallContext, spec :MixSpec, recent :List(ItemId), count :UInt32) -> (result :MixResult);
  # Someone in a cast, and what they appear in that the viewer may see. Since schemaVersion 28.
  person @11 (ctx :CallContext, id :ItemId) -> (result :PersonResult);
}
struct ItemDetail {
  summary @0 :ItemSummary; overview @1 :Text; genres @2 :List(Text); chapters @3 :List(Chapter);
  audioTracks @4 :List(Track);
  subtitleTracks @5 :List(Track);
  trickplay @6 :List(Trickplay);    # one per width the server renders; empty when off or unprobed
  segments @7 :List(Segment);
  fonts @8 :List(Font);             # empty when the file embeds none; an older client ignores it
  artists @9 :List(Text);           # a song's track artists, in display order; empty for anything else
  album @10 :Text;                  # a song's album name; empty when it has none
  cast @11 :List(Credit);           # billed order; a series or season ranks its regulars across episodes
  versions @12 :List(Version);      # a film or episode's files, the primary first; empty for anything else. Since schemaVersion 28.
}
# Someone who appears in the item. `imageUrl` is the person's photo on this
# server, empty when there is none.
struct Credit {
  name @0 :Text; character @1 :Text; kind @2 :CreditKind; imageUrl @3 :Text;
  personId @4 :ItemId;              # what Library.person takes. Since schemaVersion 28.
}
enum CreditKind { actor @0; guestStar @1; }
# A person's page. `films` are newest first. `shows` hold one entry per show,
# the show that gives them the most episodes first, each with its episodes in
# season order. Only items the viewer may see are listed.
struct Person {
  id @0 :ItemId; name @1 :Text;
  imageUrl @2 :Text;                # the person's photo on this server, empty when there is none
  films @3 :List(PersonCredit);
  shows @4 :List(PersonShow);
  # What a metadata provider says about them; each empty when unknown.
  biography @5 :Text;
  birthDate @6 :Text;               # YYYY-MM-DD
  deathDate @7 :Text;               # YYYY-MM-DD
  birthPlace @8 :Text;
  imdbId @9 :Text;                  # nm and digits
  wikidataId @10 :Text;             # Q and digits
}
# One item a person appears in, and who they play there (empty when the credit names no one).
struct PersonCredit { item @0 :ItemSummary; character @1 :Text; kind @2 :CreditKind; }
struct PersonShow {
  series @0 :ItemSummary;
  characters @1 :List(Text);        # who they play in it, most episodes first
  episodes @2 :List(PersonCredit);
}
struct PersonResult { union { person @0 :Person; error @1 :Error; } }
# One playable file of an item: its primary, a grouped edition (a cut, a 3D
# rip) or another copy of the same episode. `id` is what Library.detail and
# Playback.negotiate take as `version`; the primary's is empty. Probe facts
# are 0 or empty when unknown. `path` is the file on the server's disk, sent
# to admins only (else empty).
struct Version {
  id @0 :Text; name @1 :Text; stereo @2 :Stereo3d; primary @3 :Bool;
  width @4 :UInt32; height @5 :UInt32;
  videoCodec @6 :Text;
  hdr @7 :Text;                     # HDR10 or HLG; empty is SDR
  audio @8 :List(VersionAudio);
  sizeBytes @9 :UInt64;
  container @10 :Text;
  path @11 :Text;
}
struct VersionAudio { language @0 :Text; codec @1 :Text; channels @2 :UInt32; }
# A font the file embeds, for a renderer drawing an ASS subtitle (spec 026).
# url holds `{subtitle}`, the index of the subtitle track the font is fetched
# for: the server cuts the font to the glyphs that track draws. A track that
# draws no ASS has no use for any of them.
struct Font { index @0 :UInt32; filename @1 :Text; mime @2 :Text; url @3 :Text; }
struct Chapter { name @0 :Text; startMs @1 :UInt64; }
# An audio or subtitle stream. `index` is what Playback.negotiate takes.
struct Track {
  index @0 :UInt32; language @1 :Text; title @2 :Text; codec @3 :Text;
  isDefault @4 :Bool; isForced @5 :Bool; isHearingImpaired @6 :Bool;
  delivery @7 :SubtitleDelivery;    # subtitles only
  url @8 :Text;                     # WebVTT for `external`, the PNG manifest for `image` (HTTP, 017 §Scope), else empty
  styledUrl @9 :Text;               # the generated Pharos-styled ASS script (spec 028), fetched with ?tier=&effects=; empty for every other track
  channels @10 :UInt16;             # audio only; 0 = unknown. Since schemaVersion 26.
}
# `external`: the player renders the WebVTT at Track.url itself. `burn`: an
# image subtitle the server draws into the video, so choosing it renegotiates.
# `image`: an image subtitle the player draws from the timed PNG manifest at
# Track.url (spec 019). Library.detail sends `image`, and its PNG manifest
# Track.url, only to a client whose CallContext.schemaVersion is 16 or later;
# an older one gets `burn` for an image subtitle. `external` WebVTT tracks and
# their URLs reach every client.
enum SubtitleDelivery { external @0; burn @1; image @2; }
# A trickplay sprite set: tiles of grid × grid thumbnails, one every intervalMs.
# tileUrl holds `{tile}`, the 0-based tile number. A tile the server has not
# generated yet answers 404.
struct Trickplay {
  width @0 :UInt32; height @1 :UInt32; intervalMs @2 :UInt32;
  thumbCount @3 :UInt32; tileCount @4 :UInt32; grid @5 :UInt32; tileUrl @6 :Text;
}
enum SegmentKind { intro @0; outro @1; recap @2; preview @3; commercial @4; }
struct Segment { kind @0 :SegmentKind; startMs @1 :UInt64; endMs @2 :UInt64; }
struct DetailResult { union { detail @0 :ItemDetail; error @1 :Error; } }
# A song's lyrics. `synced`: every line has a `startMs` to follow along by;
# otherwise `startMs` is 0 and the lines are plain text, a blank line between
# verses. No lines means the song has none. `source` names where they came
# from: `sidecar_lrc`, `embedded_synced`, `embedded_text`, `sidecar_text` or
# `online`, empty when there are none.
# A word timestamp inside a `LyricLine`. `at` is the char (Unicode scalar)
# offset in the line's text where the stamp falls; a stamp at the text's length
# marks the end of the last word.
struct LyricWord { at @0 :UInt32; startMs @1 :UInt64; }
# `words` is empty unless the source timed words.
struct LyricLine { startMs @0 :UInt64; text @1 :Text; words @2 :List(LyricWord); }
# wordsPending: word times are being worked out; ask again in a little while.
struct Lyrics { synced @0 :Bool; lines @1 :List(LyricLine); source @2 :Text; wordsPending @3 :Bool; }
struct LyricsResult { union { lyrics @0 :Lyrics; error @1 :Error; } }

# A dynamic mix (spec 024): rules the next songs must, should or should not
# meet. The call is stateless: the client sends the spec, what was heard or
# queued (`recent`, oldest first, at most 200 read) and how many songs it
# wants (1 to 50), and gets that many back with the rules each matched.
# Hard rules (require, exclude) never bend; a mix that runs out of songs
# answers short with `exhausted`. Freshness, shares and spacing bend only
# when they would leave nothing, and each bend is named in `relaxed`
# (`repeat`, `share`, `spacing`).
enum MixWeight { require @0; prefer @1; avoid @2; exclude @3; }
struct MixRange { from @0 :UInt32; to @1 :UInt32; }   # inclusive both ends
struct MixRule {
  weight @0 :MixWeight;
  share @1 :UInt8;   # 1-100: aim for about this % of songs to match (weight must be prefer); 0 = no share
  test :union {
    genre @2 :Text;
    artist @3 :Text;            # the track or the album artist
    album @4 :Text;
    years @5 :MixRange;         # release year
    favorite @6 :Void;
    neverPlayed @7 :Void;
    playCount @8 :MixRange;
    notPlayedForDays @9 :UInt32;
    playedWithinDays @10 :UInt32;
    addedWithinDays @11 :UInt32;
    lengthSecs @12 :MixRange;
    likeSeed @13 :Void;         # same artist, or near in genre, era, tempo and energy; a prefer weights by how near
    tempoBpm @14 :MixRange;     # beats per minute (the file's tag, else analysis); untempoed songs never match
    energy @15 :MixRange;       # percent, 0 calm to 100 intense (analysis); unanalysed songs never match
  }
}
struct MixSpec {
  rules @0 :List(MixRule);      # at most 32
  seed :union {
    none @1 :Void;              # a likeSeed rule is refused
    song @2 :ItemId;            # like this song
    follow @3 :Void;            # like the last song in `recent`, then each pick like the one before: the mix walks
  }
  artistGap @4 :UInt8;          # no artist again within this many songs
  albumGap @5 :UInt8;           # no album again within this many songs
}
struct MixPick { item @0 :ItemSummary; reasons @1 :List(Text); }   # reasons: the soft rules it matched, in words
struct MixBatch { picks @0 :List(MixPick); pool @1 :UInt32; eligible @2 :UInt32; exhausted @3 :Bool; relaxed @4 :List(Text); }
struct MixResult { union { batch @0 :MixBatch; error @1 :Error; } }
# A user's saved mixes, stored whole as one Prefs value under `mix.saved`
# (a capnp message of this struct), so every first-party client shares them.
struct SavedMix { name @0 :Text; spec @1 :MixSpec; }
struct SavedMixes { mixes @0 :List(SavedMix); }

interface UserData {
  setPlayed @0 (ctx :CallContext, id :ItemId, played :Bool) -> (result :UserStateResult);
  setFavorite @1 (ctx :CallContext, id :ItemId, favorite :Bool) -> (result :UserStateResult);
}
struct UserStateResult { union { state @0 :UserState; error @1 :Error; } }

# ---- playback -------------------------------------------------------------
struct Capabilities { videoCodecs @0 :List(Text); audioCodecs @1 :List(Text); containers @2 :List(Text); maxBitrate @3 :UInt64; hdr @4 :Bool; imageSubtitles @5 :List(Text); }
struct Decision {
  method @0 :Text;             # direct_play | audio_remux | video_remux | transcode (pharos_playback_decision_total's decision)
  directPlayBlock @1 :Text;    # same label values as pharos_playback_decision_total
  downgrade @2 :Text;
  streamUrl @3 :Text;          # HLS master or direct stream (HTTP, 017 §Scope)
  playSession @4 :PlaySessionId;
  audio @5 :OptU32;            # the audio track this playback starts on
  subtitle @6 :OptU32;         # the subtitle it starts with; none = off
}
struct DecisionResult { union { decision @0 :Decision; error @1 :Error; } }
interface PlaybackGrant {
  decision @0 (ctx :CallContext) -> (result :DecisionResult);
  # audio/subtitle: the tracks playing now, after any switch the player made
  # without renegotiating; `default` says nothing and keeps the stored choice.
  report @1 (ctx :CallContext, positionMs :UInt64, paused :Bool, audio :StreamChoice, subtitle :StreamChoice) -> (result :Done);
  stop @2 (ctx :CallContext, positionMs :UInt64) -> (result :Done);
}
struct NegotiateResult { union { grant @0 :PlaybackGrant; error @1 :Error; } }
# The tracks a negotiate naming `default` for both would start on: the user's
# remembered pick for the item, else their language rules; none = off.
# Since schemaVersion 26.
struct StartTracks { audio @0 :OptU32; subtitle @1 :OptU32; }
struct StartTracksResult { union { tracks @0 :StartTracks; error @1 :Error; } }
interface Playback {
  negotiate @0 (ctx :CallContext, id :ItemId, caps :Capabilities, audio :StreamChoice, subtitle :StreamChoice, version :Text) -> (result :NegotiateResult);
  # `version`: a Version.id from ItemDetail.versions; empty plays the primary.
  # An id the item does not have is notFound; an edition the server cannot
  # serve yet (no stored probe) is unavailable.
  # What a title page shows in its track pickers before playback; opens no session.
  # Since schemaVersion 26.
  startTracks @1 (ctx :CallContext, id :ItemId, caps :Capabilities) -> (result :StartTracksResult);
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
  groupDrift @19; groupHardSeek @20; groupWait @21; playbackResume @22;
  playbackStart @23; playbackSummary @24;
}
enum Level { error @0; warn @1; info @2; }
struct TelemetryEvent {
  kind @0 :EventKind; level @1 :Level; monotonicMs @2 :UInt64; wallMs @3 :UInt64;
  traceId @4 :Text; spanId @5 :Text; route @6 :Text; action @7 :Text;
  item @8 :ItemId; playSession @9 :PlaySessionId; group @10 :GroupId;   # pointer fields: absent when n/a
  repeat @11 :UInt32; fields @12 :List(Field);
  struct Field { key @0 :Text; value @1 :Text $secret; }   # free text from the client: never in a debug dump
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

# ---- group watch (014) ----------------------------------------------------
# The same SyncPlay groups jellyfin-web uses (pharos-sync's GroupRegistry), so a
# native member and a Jellyfin member can share one group. A member is the
# call's `ctx.device` (per user): a call with no device is refused
# `invalid{field "ctx.device"}`. Dropping a Membership is a socket loss (the
# member's TTL applies), not a leave; `leave` is explicit.
# `ownTracks`: the member is not following the group's audio and subtitles (US7).
struct GroupMember { id @0 :Text; name @1 :Text; buffering @2 :Bool; ownTracks @3 :Bool; }
# A stream index of the playing item. `set` false means no choice yet (each
# member keeps the player's default); `off` is subtitles off.
struct TrackChoice { set @0 :Bool; index @1 :UInt32; off @2 :Bool; }
# The group's audio and subtitle choice; `by` is the member id that made it.
struct GroupTracks { audio @0 :TrackChoice; subtitle @1 :TrackChoice; by @2 :Text; }
# `tracks` is what a late joiner starts on. `you` is the receiving member's own
# id, set only in the `joined` update ("" elsewhere).
struct GroupSummary { id @0 :GroupId; name @1 :Text; item @2 :ItemId; members @3 :List(GroupMember); tracks @4 :GroupTracks; you @5 :Text; }
struct GroupsResult { union { groups @0 :List(GroupSummary); error @1 :Error; } }
# Not a union, like ResumeResult, so a call on `membership` pipelines. A refused
# create or join still returns a membership whose every call fails with the
# same error; `status` says why.
struct MembershipResult { membership @0 :Membership; status @1 :Done; }
# NTP-style: the server's receive and send times (T2, T3) for the client's
# `clientMs` (T1). The client closes the loop with Membership.clockReport.
struct ClockSample { clientMsEcho @0 :UInt64; recvMs @1 :UInt64; sendMs @2 :UInt64; }
struct ClockResult { union { sample @0 :ClockSample; error @1 :Error; } }

interface Group {
  list   @0 (ctx :CallContext) -> (result :GroupsResult);
  create @1 (ctx :CallContext, item :ItemId, positionMs :UInt64, sink :GroupSink) -> (result :MembershipResult);
  join   @2 (ctx :CallContext, id :GroupId, sink :GroupSink) -> (result :MembershipResult);
  clock  @3 (ctx :CallContext, clientMs :UInt64) -> (result :ClockResult);
}
interface Membership {
  unpause   @0 (ctx :CallContext) -> (result :Done);
  pause     @1 (ctx :CallContext) -> (result :Done);
  seek      @2 (ctx :CallContext, positionMs :UInt64) -> (result :Done);
  # `playlistItemId` is the queue entry the report is FOR (the item update
  # carries it); the group drops a stale report racing a queue change (B37).
  buffering @3 (ctx :CallContext, positionMs :UInt64, buffering :Bool, playlistItemId :Text) -> (result :Done);
  ready     @4 (ctx :CallContext, positionMs :UInt64, playing :Bool, playlistItemId :Text) -> (result :Done);
  leave     @5 (ctx :CallContext) -> (result :Done);
  # Liveness beacon, every 30 s: keeps a member that is only watching clear of
  # the ghost prune (T83).
  ping      @6 (ctx :CallContext) -> (result :Done);
  clockReport @7 (ctx :CallContext, t1 :UInt64, t2 :UInt64, t3 :UInt64, t4 :UInt64) -> (result :Done);
  # What each member of THIS group has watched of `ids` (US6, ADR-0054). At most
  # 200 ids, else `limit`. An id the caller may not see is left out of every
  # member's `items`; a member with no recorded user is listed with no items.
  seen      @8 (ctx :CallContext, ids :List(ItemId)) -> (result :SeenResult);
  # Change the group's audio and/or subtitle (US7); a `set` false choice leaves
  # that one as it is. Advisory: each following member applies it themselves.
  # `playlistItemId` names the queue entry the pick was made on: the group drops
  # one that is no longer the playing entry, since stream indexes belong to one
  # item. Empty skips that check.
  setTracks @9 (ctx :CallContext, audio :TrackChoice, subtitle :TrackChoice, playlistItemId :Text) -> (result :Done);
  # false = "Just me" (own tracks), true = back on the group's tracks.
  followTracks @10 (ctx :CallContext, follow :Bool) -> (result :Done);
  # Move the group to `item` from `positionMs` (014 US9): the next or previous
  # episode, or another title. Every member must be able to see it, else
  # `notPermitted{what "queueing"}`. `playlistItemId` names the entry the caller
  # is moving FROM: the group drops a request whose entry is no longer the
  # playing one, so two members' autoplay at an episode's end moves it once.
  # Empty skips that check. Since schemaVersion 23.
  setItem   @11 (ctx :CallContext, item :ItemId, positionMs :UInt64, playlistItemId :Text) -> (result :Done);
}
# For a film or episode `played` and `positionPct` (0-100, the resume point)
# apply. For a series or season `playedCount` of `totalCount` episodes (the ones
# the caller may see) have been played; `played` is true when all are.
struct ItemSeen { id @0 :ItemId; played @1 :Bool; positionPct @2 :UInt8; playedCount @3 :UInt32; totalCount @4 :UInt32; }
struct MemberSeen { member @0 :GroupMember; items @1 :List(ItemSeen); }
struct SeenResult { union { members @0 :List(MemberSeen); error @1 :Error; } }
enum EndReason { left @0; notInGroup @1; }
struct GroupUpdate {
  union {
    joined  @0 :GroupSummary;
    play    @1 :At;
    pause   @2 :At;
    seek    @3 :At;
    member  @4 :GroupMember;           # joined, or its buffering changed
    left    @5 :Text;                  # member id
    item    @6 :ItemAt;                # the group moved to another item
    ended   @7 :EndReason;
    tracks  @8 :GroupTracks;           # the group's audio or subtitle changed
  }
  # `by` is the member id that caused it; "" when the group did (all ready).
  struct At { atServerMs @0 :UInt64; positionMs @1 :UInt64; by @2 :Text; }
  struct ItemAt { item @0 :ItemId; playlistItemId @1 :Text; positionMs @2 :UInt64; playing @3 :Bool; }
}
interface GroupSink { update @0 (ctx :CallContext, update :GroupUpdate) -> (result :Done); }
