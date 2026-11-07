//! Relative URI resolution against a playlist base URL.

/// Resolves `reference` against `base`, keeping `reference`'s query string and
/// dropping `base`'s query and fragment.
pub fn resolve(base: &str, reference: &str) -> String {
    if split_scheme(reference).is_some() {
        return reference.to_string();
    }

    let base_clean = strip_query_and_fragment(base);

    if reference.starts_with("//") {
        return match split_scheme(base_clean) {
            Some((scheme, _)) => format!("{scheme}:{reference}"),
            None => reference.to_string(),
        };
    }

    if reference.starts_with('/') {
        return match split_scheme(base_clean) {
            Some((scheme, rest)) => {
                let authority = rest.split_once('/').map_or(rest, |(a, _)| a);
                format!("{scheme}://{authority}{reference}")
            }
            None => reference.to_string(),
        };
    }

    let (origin, base_path) = match split_scheme(base_clean) {
        Some((scheme, rest)) => match rest.split_once('/') {
            Some((authority, path)) => (format!("{scheme}://{authority}"), format!("/{path}")),
            None => (format!("{scheme}://{rest}"), "/".to_string()),
        },
        None => (String::new(), base_clean.to_string()),
    };

    let (ref_path, ref_tail) = match reference.find(['?', '#']) {
        Some(idx) => (
            reference.get(..idx).unwrap_or(reference),
            reference.get(idx..).unwrap_or(""),
        ),
        None => (reference, ""),
    };

    if ref_path.is_empty() {
        return format!("{origin}{base_path}{ref_tail}");
    }

    let dir = match base_path.rsplit_once('/') {
        Some((prefix, _)) => format!("{prefix}/"),
        None => String::new(),
    };

    let joined = format!("{dir}{ref_path}");
    let normalized = normalize_path(&joined);
    format!("{origin}{normalized}{ref_tail}")
}

fn strip_query_and_fragment(uri: &str) -> &str {
    let no_frag = uri.split_once('#').map_or(uri, |(b, _)| b);
    no_frag.split_once('?').map_or(no_frag, |(b, _)| b)
}

fn split_scheme(uri: &str) -> Option<(&str, &str)> {
    let (scheme, rest) = uri.split_once("://")?;
    let mut chars = scheme.chars();
    let first = chars.next()?;
    if !first.is_ascii_alphabetic() {
        return None;
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')) {
        return None;
    }
    Some((scheme, rest))
}

fn normalize_path(path: &str) -> String {
    let (leading_slash, body) = match path.strip_prefix('/') {
        Some(rest) => (true, rest),
        None => (false, path),
    };
    let mut segments: Vec<&str> = Vec::new();
    let parts: Vec<&str> = body.split('/').collect();
    let last = parts.len().saturating_sub(1);
    for (i, seg) in parts.into_iter().enumerate() {
        match seg {
            "." | ".." if i == last => {
                if seg == ".." {
                    segments.pop();
                }
                segments.push("");
            }
            "." => {}
            ".." => {
                segments.pop();
            }
            s => segments.push(s),
        }
    }
    let mut out = String::with_capacity(path.len());
    if leading_slash {
        out.push('/');
    }
    out.push_str(&segments.join("/"));
    out
}

#[cfg(test)]
mod tests {
    // Panicking on unexpected values is the intended failure mode in unit tests.
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::resolve;

    #[test]
    fn absolute_scheme_reference_is_returned_unchanged() {
        let base = "http://pharos.local/videos/42/master.m3u8?PlaySessionId=p1&api_key=k";
        assert_eq!(
            resolve(base, "https://cdn.example.com/seg/0.m4s?token=abc"),
            "https://cdn.example.com/seg/0.m4s?token=abc"
        );
        assert_eq!(
            resolve(base, "http://other.local/main.m3u8"),
            "http://other.local/main.m3u8"
        );
    }

    #[test]
    fn scheme_relative_reference_inherits_base_scheme() {
        assert_eq!(
            resolve(
                "https://pharos.local/videos/42/master.m3u8?PlaySessionId=p1",
                "//cdn.pharos.local/videos/42/0.m4s?e=1"
            ),
            "https://cdn.pharos.local/videos/42/0.m4s?e=1"
        );
        assert_eq!(
            resolve("/videos/42/master.m3u8", "//cdn.pharos.local/0.m4s"),
            "//cdn.pharos.local/0.m4s"
        );
    }

    #[test]
    fn root_relative_reference_uses_base_authority_and_drops_base_query() {
        let base = "http://pharos.local:8096/videos/42/master.m3u8?PlaySessionId=p1&api_key=k";
        assert_eq!(
            resolve(
                base,
                "/videos/42/h264cmaf/main.m3u8?PlaySessionId=p1&api_key=k"
            ),
            "http://pharos.local:8096/videos/42/h264cmaf/main.m3u8?PlaySessionId=p1&api_key=k"
        );
        assert_eq!(
            resolve(base, "/videos/42/vp9/init.mp4"),
            "http://pharos.local:8096/videos/42/vp9/init.mp4"
        );
    }

    #[test]
    fn relative_reference_joins_base_directory_and_normalises_dot_segments() {
        let base = "http://pharos.local/videos/42/vp9/main.m3u8?PlaySessionId=p1&api_key=k#frag";
        assert_eq!(
            resolve(base, "0.m4s?PlaySessionId=p1&api_key=k&e=3"),
            "http://pharos.local/videos/42/vp9/0.m4s?PlaySessionId=p1&api_key=k&e=3"
        );
        assert_eq!(
            resolve(base, "./sub/../audio/init.mp4?PlaySessionId=p1"),
            "http://pharos.local/videos/42/vp9/audio/init.mp4?PlaySessionId=p1"
        );
        assert_eq!(
            resolve(base, "../h264cmaf/main.m3u8?qs=1"),
            "http://pharos.local/videos/42/h264cmaf/main.m3u8?qs=1"
        );
        assert_eq!(
            resolve(base, "../../../../../above-root.m4s"),
            "http://pharos.local/above-root.m4s"
        );
    }

    #[test]
    fn path_only_base_resolves_root_and_relative_references() {
        let base = "/videos/1/master.m3u8?PlaySessionId=p1&api_key=k";
        assert_eq!(
            resolve(base, "/videos/1/vp9/main.m3u8?PlaySessionId=p1&api_key=k"),
            "/videos/1/vp9/main.m3u8?PlaySessionId=p1&api_key=k"
        );
        assert_eq!(
            resolve(base, "vp9/main.m3u8?PlaySessionId=p1&api_key=k"),
            "/videos/1/vp9/main.m3u8?PlaySessionId=p1&api_key=k"
        );
        assert_eq!(
            resolve(base, "./vp9/../h264cmaf/0.m4s"),
            "/videos/1/h264cmaf/0.m4s"
        );
    }

    /// RFC 3986 5.2.2: a query-only reference keeps the base path.
    #[test]
    fn query_only_reference_keeps_the_base_file() {
        assert_eq!(
            resolve("http://pharos.local/videos/42/master.m3u8?a=1", "?x=1"),
            "http://pharos.local/videos/42/master.m3u8?x=1"
        );
        assert_eq!(
            resolve("/videos/42/master.m3u8?a=1", "#t"),
            "/videos/42/master.m3u8#t"
        );
    }

    /// RFC 3986 remove_dot_segments keeps empty segments.
    #[test]
    fn empty_path_segments_are_kept() {
        assert_eq!(
            resolve("http://h/v/main.m3u8", "seg//0.m4s"),
            "http://h/v/seg//0.m4s"
        );
        assert_eq!(resolve("http://h/v/main.m3u8", "a/.."), "http://h/v/");
        assert_eq!(resolve("http://h/v/main.m3u8", "./"), "http://h/v/");
    }
}
