//! The fixed test catalogue: generated tones and one stream.

use super::*;

pub(crate) const TEST: &str = "test";

/// One entry in the fixed test catalogue.
pub(crate) struct Entry {
    pub(crate) id: &'static str,
    pub(crate) parent: &'static str,
    pub(crate) title: &'static str,
    pub(crate) subtitle: &'static str,
    pub(crate) playable: bool,
    /// Tone frequency for playable entries without a `url`.
    pub(crate) hz: u32,
    /// An http(s) stream; when set it plays instead of the tone.
    pub(crate) url: &'static str,
    /// An http(s) image shown as the item's icon and album art.
    pub(crate) art: &'static str,
}

pub(crate) const CATALOGUE: &[Entry] = &[
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

pub(crate) fn test_entry(id: &str) -> Option<&'static Entry> {
    CATALOGUE.iter().find(|e| e.id == id)
}

pub(crate) fn test_playables() -> Vec<Item> {
    CATALOGUE
        .iter()
        .filter(|e| e.playable)
        .map(Item::of_entry)
        .collect()
}

/// Test entries whose title holds `query`; none for an empty query.
pub(crate) fn test_matches(query: &str) -> Vec<Item> {
    let q = query.to_lowercase();
    if q.trim().is_empty() {
        return Vec::new();
    }
    CATALOGUE
        .iter()
        .filter(|e| e.playable && e.title.to_lowercase().contains(&q))
        .map(Item::of_entry)
        .collect()
}

/// How many of `hits` are test entries rather than library results.
pub(crate) fn test_matches_count(hits: &[Item]) -> usize {
    hits.iter().filter(|i| test_entry(&i.id).is_some()).count()
}

/// A 10 s, 16-bit mono, 22.05 kHz sine WAV at low volume.
pub(crate) fn tone_wav(hz: u32) -> Vec<u8> {
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
pub(crate) const TONE_MS: i64 = 10_000;

/// Where a test entry plays from: its `url`, else its tone written to the
/// cache dir.
pub(crate) fn test_source(env: &mut JNIEnv, e: &Entry) -> R<String> {
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
