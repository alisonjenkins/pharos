//! The pharos REST calls this app makes, as plain values: URLs, bodies and
//! parsed replies. Nothing here touches the runtime, so all of it is tested
//! on the host.
//!
//! The Jellyfin-compatible API is used because a Cobalt app reaches the
//! network only through the runtime's HTTPS tasks (ADR-0136).

use kobo_json::{ObjectBuilder, Value};

/// The device id pharos keys this reader's session by.
pub const DEVICE_ID: &str = "pharos-kobo";
/// How many rows one request asks for. One panel page shows fewer, and the
/// rest are turned to without another round trip.
pub const PAGE: u32 = 50;

/// Ticks are 100 ns: what Jellyfin positions are counted in.
const TICKS_PER_MS: u64 = 10_000;

/// A signed-in account: what the app keeps across restarts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Account {
    pub user_id: String,
    pub token: String,
}

impl Account {
    /// Two lines: user id, then token. Neither can hold a newline.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        format!("{}\n{}", self.user_id, self.token).into_bytes()
    }

    #[must_use]
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let text = std::str::from_utf8(bytes).ok()?;
        let (user_id, token) = text.split_once('\n')?;
        (!user_id.is_empty() && !token.is_empty() && !token.contains('\n')).then(|| Self {
            user_id: user_id.to_owned(),
            token: token.to_owned(),
        })
    }
}

/// One row of a listing.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Entry {
    pub id: String,
    pub name: String,
    /// Artist for an album or a track; empty when pharos sent none.
    pub by: String,
    /// Length in milliseconds, when pharos sent one.
    pub runtime_ms: Option<u64>,
}

/// A page of a listing and how many there are in all.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Page {
    pub entries: Vec<Entry>,
    pub total: usize,
}

/// The value of `X-Emby-Authorization`: who is asking, and the token once
/// there is one. `Authorization` itself is the runtime's to set.
#[must_use]
pub fn client_header_value(version: &str, token: Option<&str>) -> String {
    let mut value = format!(
        "MediaBrowser Client=\"Pharos Kobo\", Device=\"Kobo\", DeviceId=\"{DEVICE_ID}\", \
         Version=\"{version}\""
    );
    if let Some(token) = token {
        value.push_str(&format!(", Token=\"{token}\""));
    }
    value
}

/// Keeps a value safe inside a URL query. Ids and secrets are hex already;
/// this is for the day one is not.
#[must_use]
pub fn query_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

#[must_use]
pub fn initiate_url(server: &str) -> String {
    format!("{server}/QuickConnect/Initiate")
}

#[must_use]
pub fn connect_url(server: &str, secret: &str) -> String {
    format!(
        "{server}/QuickConnect/Connect?secret={}",
        query_escape(secret)
    )
}

#[must_use]
pub fn finish_url(server: &str) -> String {
    format!("{server}/Users/AuthenticateWithQuickConnect")
}

#[must_use]
pub fn finish_body(secret: &str) -> String {
    ObjectBuilder::new().set("Secret", secret).build().to_json()
}

/// Albums in every music library the user can see, by name.
#[must_use]
pub fn albums_url(server: &str, account: &Account, start: usize) -> String {
    format!(
        "{server}/Users/{}/Items?IncludeItemTypes=MusicAlbum&Recursive=true\
         &SortBy=SortName&SortOrder=Ascending&StartIndex={start}&Limit={PAGE}",
        query_escape(&account.user_id)
    )
}

/// One album's tracks, in disc and track order.
#[must_use]
pub fn tracks_url(server: &str, account: &Account, album: &str, start: usize) -> String {
    format!(
        "{server}/Users/{}/Items?ParentId={}&IncludeItemTypes=Audio\
         &SortBy=ParentIndexNumber,IndexNumber,SortName&StartIndex={start}&Limit={PAGE}",
        query_escape(&account.user_id),
        query_escape(album)
    )
}

/// Every codec the reader's decoder plays, as pharos's probe names them.
/// A source in one of these is sent as stored, which pharos serves in byte
/// ranges; anything else comes back as a live MP3 encode, which has no
/// ranges and so must fit in one download (B1449).
pub const DECODES: &str = "mp3,aac,flac,opus,vorbis,pcm_s16le";

/// The track as the reader can decode it. The runtime fetches streams
/// without credentials, so the token rides in the query (ADR-0136).
#[must_use]
pub fn stream_url(server: &str, account: &Account, track: &str) -> String {
    format!(
        "{server}/Audio/{}/universal?AudioCodec={DECODES}&api_key={}",
        query_escape(track),
        query_escape(&account.token)
    )
}

/// Which playing-session call a report is.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Report {
    Started,
    Progress,
    Stopped,
}

impl Report {
    #[must_use]
    pub fn url(self, server: &str) -> String {
        let tail = match self {
            Self::Started => "",
            Self::Progress => "/Progress",
            Self::Stopped => "/Stopped",
        };
        format!("{server}/Sessions/Playing{tail}")
    }

    /// A stable name for logs.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Started => "started",
            Self::Progress => "progress",
            Self::Stopped => "stopped",
        }
    }
}

#[must_use]
pub fn report_body(track: &str, position_ms: u64, paused: bool) -> String {
    // Built by hand: kobo-json numbers stop at 32 bits, and ten minutes is
    // already six billion ticks.
    let ticks = position_ms.saturating_mul(TICKS_PER_MS);
    let mut item = String::new();
    kobo_json::escape_into(track, &mut item);
    format!(
        "{{\"ItemId\":{item},\"PositionTicks\":{ticks},\"IsPaused\":{paused},\
         \"PlayMethod\":\"Transcode\"}}"
    )
}

fn parse(bytes: &[u8]) -> Option<Value> {
    kobo_json::parse(std::str::from_utf8(bytes).ok()?).ok()
}

fn text<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

/// `(code to show, secret to poll with)` from `QuickConnect/Initiate`.
#[must_use]
pub fn parse_initiate(bytes: &[u8]) -> Option<(String, String)> {
    let reply = parse(bytes)?;
    Some((
        text(&reply, "Code")?.to_owned(),
        text(&reply, "Secret")?.to_owned(),
    ))
}

/// Whether `QuickConnect/Connect` says the code was approved.
#[must_use]
pub fn parse_connected(bytes: &[u8]) -> bool {
    parse(bytes)
        .and_then(|reply| reply.get("Authenticated").and_then(Value::as_bool))
        .unwrap_or(false)
}

/// The account from `AuthenticateWithQuickConnect`.
#[must_use]
pub fn parse_account(bytes: &[u8]) -> Option<Account> {
    let reply = parse(bytes)?;
    let token = text(&reply, "AccessToken")?;
    let user_id = reply.get("User").and_then(|user| text(user, "Id"))?;
    let account = Account {
        user_id: user_id.to_owned(),
        token: token.to_owned(),
    };
    // Round-trips through the store, or it is not an account worth keeping.
    Account::decode(&account.encode())
}

/// A page of `Items`. Rows without an id are dropped; a missing name reads
/// as "Untitled" so a row is never blank.
#[must_use]
pub fn parse_page(bytes: &[u8]) -> Option<Page> {
    let reply = parse(bytes)?;
    let items = reply.get("Items").and_then(Value::as_array)?;
    let entries: Vec<Entry> = items
        .iter()
        .filter_map(|item| {
            let by = text(item, "AlbumArtist")
                .or_else(|| {
                    item.get("Artists")
                        .and_then(Value::as_array)
                        .and_then(|artists| artists.first())
                        .and_then(Value::as_str)
                })
                .unwrap_or_default();
            let runtime_ms = item
                .get("RunTimeTicks")
                .and_then(Value::as_i64)
                .and_then(|ticks| u64::try_from(ticks).ok())
                .map(|ticks| ticks / TICKS_PER_MS);
            Some(Entry {
                id: text(item, "Id")?.to_owned(),
                name: text(item, "Name").unwrap_or("Untitled").to_owned(),
                by: by.to_owned(),
                runtime_ms,
            })
        })
        .collect();
    let total = reply
        .get("TotalRecordCount")
        .and_then(Value::as_i64)
        .and_then(|total| usize::try_from(total).ok())
        .unwrap_or(entries.len());
    Some(Page { entries, total })
}

/// "3:07", "1:02:05": a length as a listener reads it.
#[must_use]
pub fn clock(ms: u64) -> String {
    let seconds = ms / 1000;
    let (hours, minutes, seconds) = (seconds / 3600, seconds / 60 % 60, seconds % 60);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account() -> Account {
        Account {
            user_id: "u1".to_owned(),
            token: "t0k".to_owned(),
        }
    }

    #[test]
    fn an_account_survives_the_store_and_junk_does_not() {
        let account = account();
        assert_eq!(Account::decode(&account.encode()), Some(account));
        assert_eq!(Account::decode(b"only-one-line"), None);
        assert_eq!(Account::decode(b"\ntoken"), None);
        assert_eq!(Account::decode(b"user\n"), None);
        assert_eq!(Account::decode(b"user\ntok\nextra"), None);
    }

    #[test]
    fn the_client_header_names_the_device_and_adds_the_token_once_signed_in() {
        let anonymous = client_header_value("0.1.0", None);
        assert!(anonymous.contains("DeviceId=\"pharos-kobo\""));
        assert!(!anonymous.contains("Token"));
        assert!(client_header_value("0.1.0", Some("abc")).ends_with(", Token=\"abc\""));
    }

    #[test]
    fn query_values_are_escaped() {
        assert_eq!(query_escape("ab-12_.~"), "ab-12_.~");
        assert_eq!(query_escape("a b&c=d"), "a%20b%26c%3Dd");
    }

    #[test]
    fn the_stream_asks_for_every_codec_the_reader_decodes_and_carries_the_token() {
        let url = stream_url("https://p.test", &account(), "id9");
        assert_eq!(
            url,
            "https://p.test/Audio/id9/universal\
             ?AudioCodec=mp3,aac,flac,opus,vorbis,pcm_s16le&api_key=t0k"
        );
    }

    #[test]
    fn an_audiobook_codec_is_taken_as_stored() {
        // An M4B is AAC: asking for MP3 alone made pharos encode it live,
        // and a live encode cannot be fetched in ranges (B1449).
        assert!(DECODES.split(',').any(|codec| codec == "aac"));
    }

    #[test]
    fn listings_page_and_sort() {
        let albums = albums_url("https://p.test", &account(), 50);
        assert!(albums.contains("IncludeItemTypes=MusicAlbum"));
        assert!(albums.contains("StartIndex=50&Limit=50"));
        let tracks = tracks_url("https://p.test", &account(), "al1", 50);
        assert!(tracks.contains("ParentId=al1"));
        assert!(tracks.contains("StartIndex=50&Limit=50"));
        assert!(tracks.contains("SortBy=ParentIndexNumber,IndexNumber"));
    }

    #[test]
    fn quick_connect_replies_parse() {
        let initiate = br#"{"Code":"123456","Secret":"s3","Authenticated":false}"#;
        assert_eq!(
            parse_initiate(initiate),
            Some(("123456".to_owned(), "s3".to_owned()))
        );
        assert!(!parse_connected(initiate));
        assert!(parse_connected(br#"{"Authenticated":true}"#));
        assert!(!parse_connected(b"not json"));
        let finished = br#"{"AccessToken":"t0k","User":{"Id":"u1","Name":"x"}}"#;
        assert_eq!(parse_account(finished), Some(account()));
        assert_eq!(parse_account(br#"{"AccessToken":"t0k"}"#), None);
    }

    #[test]
    fn a_page_keeps_rows_with_ids_and_reads_lengths_and_artists() {
        let bytes = br#"{"Items":[
            {"Id":"a","Name":"One","AlbumArtist":"Band","RunTimeTicks":1850000000},
            {"Id":"b","Artists":["Solo"]},
            {"Name":"no id"}
        ],"TotalRecordCount":120}"#;
        let page = parse_page(bytes).unwrap_or_default();
        assert_eq!(page.total, 120);
        assert_eq!(page.entries.len(), 2);
        assert_eq!(page.entries[0].by, "Band");
        assert_eq!(page.entries[0].runtime_ms, Some(185_000));
        assert_eq!(page.entries[1].name, "Untitled");
        assert_eq!(page.entries[1].by, "Solo");
    }

    #[test]
    fn reports_carry_ticks_and_go_to_the_session_endpoints() {
        let body = report_body("id9", 1_500, true);
        assert!(body.contains("\"PositionTicks\":15000000"));
        assert!(body.contains("\"IsPaused\":true"));
        assert!(body.starts_with("{\"ItemId\":\"id9\","), "{body}");
        assert!(report_body("x", 600_000, false).contains(":6000000000,"));
        assert_eq!(
            Report::Started.url("https://p.test"),
            "https://p.test/Sessions/Playing"
        );
        assert_eq!(
            Report::Stopped.url("https://p.test"),
            "https://p.test/Sessions/Playing/Stopped"
        );
        let labels = [Report::Started, Report::Progress, Report::Stopped].map(Report::label);
        assert_eq!(labels, ["started", "progress", "stopped"]);
    }

    #[test]
    fn lengths_read_as_clocks() {
        assert_eq!(clock(187_000), "3:07");
        assert_eq!(clock(3_725_000), "1:02:05");
        assert_eq!(clock(0), "0:00");
    }
}
