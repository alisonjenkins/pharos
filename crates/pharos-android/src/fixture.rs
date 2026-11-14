//! The fixed test catalogue: a Test folder of generated tones and one
//! stream, for the host JVM check (`crates/pharos-classgen/jvm-check`). It
//! is built only with the `test-catalogue` feature. Without it every call
//! here answers empty, so a shipped app shows the library alone.

#[cfg(feature = "test-catalogue")]
mod imp {
    use super::super::*;

    const TEST: &str = "test";

    /// One entry in the fixed test catalogue.
    struct Entry {
        id: &'static str,
        parent: &'static str,
        title: &'static str,
        subtitle: &'static str,
        playable: bool,
        /// Tone frequency for playable entries without a `url`.
        hz: u32,
        /// An http(s) stream; when set it plays instead of the tone.
        url: &'static str,
        /// An http(s) image shown as the item's icon and album art.
        art: &'static str,
    }

    const CATALOGUE: &[Entry] = &[
        Entry {
            id: TEST,
            parent: ROOT,
            title: "Test",
            subtitle: "Spike 044",
            playable: false,
            hz: 0,
            url: "",
            art: "",
        },
        Entry {
            id: "tone:440",
            parent: TEST,
            title: "Test tone A",
            subtitle: "Spike 044",
            playable: true,
            hz: 440,
            url: "",
            art: "",
        },
        Entry {
            id: "tone:523",
            parent: TEST,
            title: "Test tone C",
            subtitle: "Spike 044",
            playable: true,
            hz: 523,
            url: "",
            art: "",
        },
        // A public-domain clip on Wikimedia Commons, to prove streaming.
        Entry {
            id: "stream:example",
            parent: TEST,
            title: "Test stream",
            subtitle: "Spike 044",
            playable: true,
            hz: 0,
            url: "https://upload.wikimedia.org/wikipedia/commons/c/c8/Example.ogg",
            art: "https://upload.wikimedia.org/wikipedia/commons/a/a9/Example.jpg",
        },
        Entry {
            id: "tone:330",
            parent: TEST,
            title: "Test chapter 1",
            subtitle: "Spike 044",
            playable: true,
            hz: 330,
            url: "",
            art: "",
        },
    ];

    fn entry(id: &str) -> Option<&'static Entry> {
        CATALOGUE.iter().find(|e| e.id == id)
    }

    fn row(e: &Entry) -> Item {
        Item {
            id: e.id.to_owned(),
            title: e.title.to_owned(),
            subtitle: e.subtitle.to_owned(),
            browsable: !e.playable,
            playable: e.playable,
            art: e.art.to_owned(),
            duration_ms: if e.playable && e.url.is_empty() {
                TONE_MS
            } else {
                0
            },
            album: String::new(),
        }
    }

    /// The rows of fixture folder `parent`; `None` when it is not one.
    pub(crate) fn folder(parent: &str) -> Option<Vec<Item>> {
        (parent != ROOT && entry(parent).is_some()).then(|| {
            CATALOGUE
                .iter()
                .filter(|e| e.parent == parent)
                .map(row)
                .collect()
        })
    }

    /// The fixture rows shown beside the library at the root.
    pub(crate) fn root_rows() -> Vec<Item> {
        CATALOGUE
            .iter()
            .filter(|e| e.parent == ROOT)
            .map(row)
            .collect()
    }

    pub(crate) fn playables() -> Vec<Item> {
        CATALOGUE.iter().filter(|e| e.playable).map(row).collect()
    }

    pub(crate) fn playable(id: &str) -> Option<Item> {
        entry(id).filter(|e| e.playable).map(row)
    }

    pub(crate) fn is_fixture(id: &str) -> bool {
        entry(id).is_some()
    }

    /// Playable entries whose title holds `query`; none for an empty query.
    pub(crate) fn matches(query: &str) -> Vec<Item> {
        let q = query.to_lowercase();
        if q.trim().is_empty() {
            return Vec::new();
        }
        CATALOGUE
            .iter()
            .filter(|e| e.playable && e.title.to_lowercase().contains(&q))
            .map(row)
            .collect()
    }

    /// A 10 s, 16-bit mono, 22.05 kHz sine WAV at low volume.
    fn tone_wav(hz: u32) -> Vec<u8> {
        const RATE: u32 = 22_050;
        let samples = RATE * 10;
        let data_len = samples * 2;
        let mut w = Vec::with_capacity(44 + data_len as usize);
        w.extend_from_slice(b"RIFF");
        w.extend_from_slice(&(36 + data_len).to_le_bytes());
        w.extend_from_slice(b"WAVEfmt ");
        w.extend_from_slice(&16u32.to_le_bytes());
        w.extend_from_slice(&1u16.to_le_bytes()); // PCM
        w.extend_from_slice(&1u16.to_le_bytes()); // mono
        w.extend_from_slice(&RATE.to_le_bytes());
        w.extend_from_slice(&(RATE * 2).to_le_bytes());
        w.extend_from_slice(&2u16.to_le_bytes());
        w.extend_from_slice(&16u16.to_le_bytes());
        w.extend_from_slice(b"data");
        w.extend_from_slice(&data_len.to_le_bytes());
        for n in 0..samples {
            let t = f64::from(n) / f64::from(RATE);
            let v = (t * f64::from(hz) * std::f64::consts::TAU).sin() * 0.05 * f64::from(i16::MAX);
            w.extend_from_slice(&(v as i16).to_le_bytes());
        }
        w
    }

    /// Length of every generated tone, in `tone_wav`.
    const TONE_MS: i64 = 10_000;

    /// Where test entry `id` plays from: its `url`, else its tone written to the
    /// cache dir.
    pub(crate) fn source(env: &mut JNIEnv, id: &str) -> R<String> {
        let e = entry(id).ok_or_else(|| format!("not a test entry: {id}"))?;
        if !e.url.is_empty() {
            return Ok(e.url.to_owned());
        }
        let service = state_lock()?
            .as_ref()
            .ok_or("play before onCreate")?
            .service
            .clone();
        let dir = env
            .call_method(service.as_obj(), "getCacheDir", "()Ljava/io/File;", &[])
            .and_then(|v| v.l())
            .map_err(jerr("getCacheDir"))?;
        let dir = env
            .call_method(&dir, "getAbsolutePath", "()Ljava/lang/String;", &[])
            .and_then(|v| v.l())
            .map_err(jerr("getAbsolutePath"))?;
        let dir = string(env, &JString::from(dir))?;
        let path = format!("{dir}/{}.wav", e.id.replace(':', "-"));
        std::fs::write(&path, tone_wav(e.hz)).map_err(|err| format!("write {path}: {err}"))?;
        Ok(path)
    }
}

#[cfg(not(feature = "test-catalogue"))]
mod imp {
    use super::super::*;

    pub(crate) fn folder(_parent: &str) -> Option<Vec<Item>> {
        None
    }

    pub(crate) fn root_rows() -> Vec<Item> {
        Vec::new()
    }

    pub(crate) fn playables() -> Vec<Item> {
        Vec::new()
    }

    pub(crate) fn playable(_id: &str) -> Option<Item> {
        None
    }

    pub(crate) fn is_fixture(_id: &str) -> bool {
        false
    }

    pub(crate) fn matches(_query: &str) -> Vec<Item> {
        Vec::new()
    }

    pub(crate) fn source(_env: &mut JNIEnv, id: &str) -> R<String> {
        Err(format!("no test catalogue in this build: {id}"))
    }
}

pub(crate) use imp::*;

#[cfg(all(test, not(feature = "test-catalogue")))]
mod tests {
    use super::*;

    #[test]
    fn without_the_feature_there_is_no_test_folder_or_tone() {
        assert!(root_rows().is_empty());
        assert!(folder("test").is_none());
        assert!(playables().is_empty());
        assert!(playable("tone:440").is_none());
        assert!(matches("test tone").is_empty());
        assert!(!is_fixture("tone:440"));
    }
}

#[cfg(all(test, feature = "test-catalogue"))]
mod tests {
    use super::*;

    #[test]
    fn with_the_feature_the_root_holds_the_test_folder() {
        let root = root_rows();
        assert_eq!(
            root.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(),
            ["test"]
        );
        assert_eq!(folder("test").map(|k| k.len()), Some(4));
        assert!(folder("root").is_none());
        assert!(playable("tone:440").is_some());
        assert!(playable("test").is_none());
        assert_eq!(
            matches("test tone c").first().map(|i| i.id.as_str()),
            Some("tone:523")
        );
    }
}
