//! Which kind of client sent a request, from its `User-Agent`.
//!
//! Every client reaches pharos from one NAT address, so the only thing that
//! tells a slow Android TV WebView from a desktop browser is the user agent.
//! [`ClientClass`] is the bounded `client` label for metrics and logs: a raw
//! user agent is attacker-controlled and would mint a series each.

use actix_web::HttpRequest;

/// Bounded client classes. Add a variant only with a distinct label and an
/// entry in [`ClientClass::ALL`], so the dashboard contract stays asserted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientClass {
    /// The Pharos Android TV app: its own `Pharos Android TV` agent, or the
    /// Android System WebView (`; wv)`) that hosts its UI.
    PharosAndroid,
    /// Any other `Mozilla/` agent: desktop and mobile browsers, including
    /// jellyfin-web, which sends a stock browser agent and cannot be told apart.
    Browser,
    /// The Jellyfin Android / Android TV apps and their media stack
    /// (`okhttp`, `jellyfin-sdk-kotlin`, ExoPlayer, stagefright).
    JellyfinAndroid,
    /// No `User-Agent`, or one none of the above match.
    Other,
}

impl ClientClass {
    pub const ALL: [Self; 4] = [
        Self::PharosAndroid,
        Self::Browser,
        Self::JellyfinAndroid,
        Self::Other,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            Self::PharosAndroid => "pharos_android",
            Self::Browser => "browser",
            Self::JellyfinAndroid => "jellyfin_android",
            Self::Other => "other",
        }
    }

    pub fn classify(user_agent: Option<&str>) -> Self {
        let Some(ua) = user_agent else {
            return Self::Other;
        };
        let ua = ua.to_ascii_lowercase();
        if ua.starts_with("pharos") {
            Self::PharosAndroid
        } else if ua.contains("okhttp")
            || ua.contains("jellyfin android")
            || ua.contains("jellyfin-sdk")
            || ua.contains("exoplayer")
            || ua.contains("stagefright")
        {
            Self::JellyfinAndroid
        } else if ua.starts_with("mozilla/") {
            if ua.contains("android") && ua.contains("; wv)") {
                Self::PharosAndroid
            } else {
                Self::Browser
            }
        } else {
            Self::Other
        }
    }

    pub fn of(req: &HttpRequest) -> Self {
        Self::classify(
            req.headers()
                .get(actix_web::http::header::USER_AGENT)
                .and_then(|v| v.to_str().ok()),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[::core::prelude::v1::test]
    fn user_agents_seen_in_the_field_map_to_their_class() {
        let cases = [
            (Some("Pharos Android TV"), ClientClass::PharosAndroid),
            (
                Some(
                    "Mozilla/5.0 (Linux; Android 11; SHIELD Android TV Build/RQ1A; wv) \
                     AppleWebKit/537.36 (KHTML, like Gecko) Version/4.0 Chrome/120.0 Safari/537.36",
                ),
                ClientClass::PharosAndroid,
            ),
            (
                Some("Mozilla/5.0 (X11; Linux x86_64; rv:144.0) Gecko/20100101 Firefox/144.0"),
                ClientClass::Browser,
            ),
            (
                Some(
                    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                     (KHTML, like Gecko) Chrome/120.0 Safari/537.36",
                ),
                ClientClass::Browser,
            ),
            (Some("okhttp/4.12.0"), ClientClass::JellyfinAndroid),
            (
                Some("Jellyfin Android TV/0.19 via jellyfin-sdk-kotlin (OkHttp/4.12.0)"),
                ClientClass::JellyfinAndroid,
            ),
            (
                Some("ExoPlayerLib/2.18.7 (Linux;Android 11)"),
                ClientClass::JellyfinAndroid,
            ),
            (Some("curl/8.5.0"), ClientClass::Other),
            (Some(""), ClientClass::Other),
            (None, ClientClass::Other),
        ];
        for (ua, want) in cases {
            assert_eq!(ClientClass::classify(ua), want, "user agent {ua:?}");
        }
    }

    #[::core::prelude::v1::test]
    fn labels_are_distinct_and_stable() {
        let labels: std::collections::BTreeSet<_> =
            ClientClass::ALL.iter().map(|c| c.label()).collect();
        assert_eq!(labels.len(), ClientClass::ALL.len(), "labels collide");
        assert_eq!(
            labels.into_iter().collect::<Vec<_>>(),
            ["browser", "jellyfin_android", "other", "pharos_android"]
        );
    }
}
