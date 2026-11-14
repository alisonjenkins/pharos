//! Transport phrases that arrive as search queries. With Pharos on screen,
//! Android Auto turns "next song" into an in-app search rather than a skip,
//! so a query that is only a transport phrase runs that action instead.

use jni::JNIEnv;

use crate::{log, skip, state_lock, transport, R, STATE_PAUSED, STATE_PLAYING};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    Next,
    Previous,
    Pause,
    Resume,
}

const NEXT: &[&str] = &[
    "next", "next song", "next track", "next one", "play next", "play the next song", "play next song", "skip",
    "skip song", "skip track", "skip this", "skip this song", "skip this track",
];
const PREVIOUS: &[&str] = &[
    "previous", "previous song", "previous track", "last song", "go back", "back", "play the previous song",
    "play previous song",
];
// "stop" pauses: a driver who says it wants silence, not a lost queue.
const PAUSE: &[&str] = &["pause", "pause music", "pause the music", "pause song", "stop", "stop music", "stop the music"];
const RESUME: &[&str] = &["play", "resume", "resume music", "resume playback", "continue", "unpause"];

/// Lowercase words, punctuation dropped, a wake phrase and "please" removed.
fn normalise(query: &str) -> String {
    let cleaned: String = query
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '\'' { c.to_ascii_lowercase() } else { ' ' })
        .collect();
    let mut words: Vec<&str> = cleaned.split_whitespace().collect();
    if words.len() >= 2 && matches!(words[0], "hey" | "ok" | "okay") && words[1] == "google" {
        words.drain(..2);
    }
    words.retain(|w| *w != "please");
    words.join(" ")
}

/// The transport action `query` asks for, when it is nothing but that.
pub fn command(query: &str) -> Option<Command> {
    let q = normalise(query);
    [(NEXT, Command::Next), (PREVIOUS, Command::Previous), (PAUSE, Command::Pause), (RESUME, Command::Resume)]
        .into_iter()
        .find(|(phrases, _)| phrases.contains(&q.as_str()))
        .map(|(_, c)| c)
}

impl Command {
    /// What the car shows in place of search results.
    pub fn done(self) -> &'static str {
        match self {
            Command::Next => "Skipped to the next track",
            Command::Previous => "Back to the previous track",
            Command::Pause => "Paused",
            Command::Resume => "Playing",
        }
    }
}

/// Runs `cmd` on the session's player.
pub fn run(env: &mut JNIEnv, cmd: Command) -> R<()> {
    log(&format!("voice command={cmd:?}"));
    match cmd {
        Command::Next => skip(env, 1, true),
        Command::Previous => skip(env, -1, true),
        Command::Pause => transport(env, "pause", STATE_PAUSED),
        Command::Resume => {
            let has_player = state_lock()?.as_ref().is_some_and(|s| s.player.is_some() && s.prepared);
            if has_player {
                transport(env, "start", STATE_PLAYING)
            } else {
                skip(env, 0, true)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transport_phrases_are_commands() {
        assert_eq!(command("Next song"), Some(Command::Next));
        assert_eq!(command("Hey Google, skip this song, please."), Some(Command::Next));
        assert_eq!(command("previous track"), Some(Command::Previous));
        assert_eq!(command("Stop"), Some(Command::Pause));
        assert_eq!(command("resume"), Some(Command::Resume));
    }

    #[test]
    fn searches_are_not_commands() {
        assert_eq!(command("test tone c"), None);
        assert_eq!(command("next to you"), None);
        assert_eq!(command("play some jazz"), None);
        assert_eq!(command(""), None);
    }
}
