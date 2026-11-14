//! The transport commands, applied the same way whoever sends them: the
//! car's session callbacks or the phone UI. Each logs its source and verdict,
//! so a log line says which client asked.

use super::*;

/// Who sent a command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Source {
    Car,
    App,
}

impl Source {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Source::Car => "car",
            Source::App => "app",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Command {
    /// Replace the queue with `ids` (item ids or car ids) and play `index`.
    Play {
        ids: Vec<String>,
        index: usize,
    },
    Pause,
    /// Continue what is loaded; with nothing loaded, play the queue's
    /// current track.
    Resume,
    Seek(u64),
    Next,
    Previous,
    Stop,
    /// Replace the queue with `ids`, where `index` is the track now playing;
    /// it keeps playing.
    SetQueue {
        ids: Vec<String>,
        index: usize,
    },
    /// Apply the level last stored by [`set_volume`] to the current player.
    Volume,
}

impl Command {
    fn label(&self) -> &'static str {
        match self {
            Command::Play { .. } => "play",
            Command::Pause => "pause",
            Command::Resume => "resume",
            Command::Seek(_) => "seek",
            Command::Next => "next",
            Command::Previous => "previous",
            Command::Stop => "stop",
            Command::SetQueue { .. } => "set_queue",
            Command::Volume => "volume",
        }
    }
}

/// Where a play request starts, or why it cannot.
pub(crate) fn start_index(len: usize, index: usize) -> R<usize> {
    if index < len {
        Ok(index)
    } else {
        Err(format!("start index {index} is outside a queue of {len}"))
    }
}

/// The row for `id` (a car id): the test entry, else the newest remembered
/// row with a title, else a bare playable row the library fills in when it
/// plays.
fn item_for(id: &str) -> Item {
    if let Some(item) = fixture::playable(id) {
        return item;
    }
    let remembered = LISTS.lock().ok().and_then(|lists| {
        lists
            .iter()
            .rev()
            .flatten()
            .find(|i| i.id == id && !i.title.is_empty())
            .cloned()
    });
    remembered.unwrap_or_else(|| Item {
        id: id.to_owned(),
        title: String::new(),
        subtitle: String::new(),
        browsable: false,
        playable: true,
        art: String::new(),
        duration_ms: 0,
        album: String::new(),
    })
}

/// Play `id` (a car id) within the list it was picked from: the test
/// catalogue, or the remembered browse or search list that holds it.
pub(crate) fn play_from(id: &str) -> Command {
    let list = if fixture::is_fixture(id) {
        fixture::playables()
    } else {
        queue_for(id)
    };
    let ids = ids(&list);
    match ids.iter().position(|i| i == id) {
        Some(index) => Command::Play { ids, index },
        None => Command::Play {
            ids: vec![id.to_owned()],
            index: 0,
        },
    }
}

pub(crate) fn apply(env: &mut JNIEnv, command: Command, source: Source) -> R<()> {
    let name = command.label();
    let from = source.label();
    match command {
        Command::Play { ids, index } => {
            let at = match start_index(ids.len(), index) {
                Ok(at) => at,
                Err(why) => {
                    log(&format!(
                        "command {name} source={from} verdict=rejected reason={why}"
                    ));
                    snapshot::note_error(&why);
                    return Ok(());
                }
            };
            let queue: Vec<Item> = ids
                .iter()
                .map(|id| item_for(&snapshot::car_id(id)))
                .collect();
            let id = queue[at].id.clone();
            log(&format!(
                "command {name} source={from} id={id} index={at} queue_len={}",
                queue.len()
            ));
            start(env, &id, Some(queue))
        }
        Command::Pause => {
            log(&format!("command {name} source={from}"));
            transport(env, "pause", STATE_PAUSED)
        }
        Command::Resume => {
            let has_player = state_lock()?
                .as_ref()
                .is_some_and(|s| s.player.is_some() && s.prepared);
            let verdict = if has_player { "continue" } else { "restart" };
            log(&format!("command {name} source={from} verdict={verdict}"));
            if has_player {
                transport(env, "start", STATE_PLAYING)
            } else {
                skip(env, 0, true)
            }
        }
        Command::Seek(ms) => {
            log(&format!("command {name} source={from} pos_ms={ms}"));
            seek(env, ms)
        }
        Command::Next => {
            log(&format!("command {name} source={from}"));
            // The car wraps round its queue; the app's queue ends.
            skip(env, 1, source == Source::Car)
        }
        Command::Previous => {
            let first = snapshot::current().index == Some(0);
            if source == Source::App && first {
                log(&format!(
                    "command {name} source={from} verdict=rewind reason=first_in_queue"
                ));
                return seek(env, 0);
            }
            log(&format!("command {name} source={from} verdict=back"));
            skip(env, -1, true)
        }
        Command::Stop => {
            log(&format!("command {name} source={from}"));
            transport(env, "stop", STATE_STOPPED)?;
            snapshot::clear();
            Ok(())
        }
        Command::SetQueue { ids, index } => {
            let car: Vec<String> = ids.iter().map(|id| snapshot::car_id(id)).collect();
            let mut guard = state_lock()?;
            let st = guard.as_mut().ok_or("set_queue before onCreate")?;
            let current = st.current.as_ref().map(|i| i.id.clone());
            let verdict = queue_verdict(current.as_deref(), &car, index);
            log(&format!(
                "command {name} source={from} verdict={verdict} index={index} queue_len={}",
                car.len()
            ));
            if verdict != "replaced" {
                return Ok(());
            }
            st.queue = car.iter().map(|id| item_for(id)).collect();
            drop(guard);
            snapshot::note_queue(&car, index);
            Ok(())
        }
        Command::Volume => {
            let level = volume();
            let player = state_lock()?.as_ref().and_then(|s| s.player.clone());
            log(&format!(
                "command {name} source={from} level={level:.2} loaded={}",
                player.is_some()
            ));
            match player {
                Some(p) => apply_volume(env, p.as_obj()),
                None => Ok(()),
            }
        }
    }
}

/// Whether a new queue can replace the current one without a restart: only
/// when its `index` names the track now playing.
pub(crate) fn queue_verdict(current: Option<&str>, ids: &[String], index: usize) -> &'static str {
    match (current, ids.get(index)) {
        (_, None) => "rejected_index",
        (None, Some(_)) => "rejected_nothing_playing",
        (Some(cur), Some(id)) if cur != id => "rejected_not_current",
        _ => "replaced",
    }
}

/// The output level, as `f32` bits; 1.0 until the app sets one.
static VOLUME: AtomicU32 = AtomicU32::new(0x3f80_0000);

pub(crate) fn volume() -> f32 {
    f32::from_bits(VOLUME.load(Ordering::SeqCst))
}

/// Stores `level`, clamped to 0.0..=1.0 (NaN is silent), for the current
/// player and every later one.
pub(crate) fn set_volume(level: f32) -> f32 {
    let level = if level.is_nan() {
        0.0
    } else {
        level.clamp(0.0, 1.0)
    };
    VOLUME.store(level.to_bits(), Ordering::SeqCst);
    level
}

/// `MediaPlayer.setVolume` with the stored level on both channels, lowered
/// while another app has ducked us.
pub(crate) fn apply_volume(env: &mut JNIEnv, player: &JObject) -> R<()> {
    let level = volume() * focus::duck_factor();
    env.call_method(
        player,
        "setVolume",
        "(FF)V",
        &[JValue::Float(level), JValue::Float(level)],
    )
    .map_err(jerr("setVolume"))?;
    Ok(())
}

/// Seeks the prepared player to `ms`; a seek while paused stays paused, as
/// the car's progress bar runs only at PLAYING.
pub(crate) fn seek(env: &mut JNIEnv, ms: u64) -> R<()> {
    let guard = state_lock()?;
    let Some(st) = guard.as_ref() else {
        return Ok(());
    };
    let (Some(player), session) = (st.player.clone(), st.session.clone()) else {
        log("seek: nothing loaded, ignored");
        return Ok(());
    };
    if !st.prepared {
        log("seek: player still preparing, ignored");
        return Ok(());
    }
    drop(guard);
    let to = i32::try_from(ms).unwrap_or(i32::MAX);
    env.call_method(player.as_obj(), "seekTo", "(I)V", &[JValue::Int(to)])
        .map_err(jerr("seekTo"))?;
    let playing = env
        .call_method(player.as_obj(), "isPlaying", "()Z", &[])
        .and_then(|v| v.z())
        .map_err(jerr("isPlaying"))?;
    let state = if playing { STATE_PLAYING } else { STATE_PAUSED };
    log(&format!("seek pos_ms={ms} state={state}"));
    set_playback_state(env, session.as_obj(), state, i64::from(to))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_start_index_outside_the_queue_names_both_values() {
        assert_eq!(start_index(3, 2), Ok(2));
        assert_eq!(
            start_index(1, 5),
            Err("start index 5 is outside a queue of 1".to_owned())
        );
        assert!(start_index(0, 0).is_err());
    }

    #[test]
    fn an_id_from_no_remembered_list_plays_alone() {
        assert_eq!(
            play_from("track/987655"),
            Command::Play {
                ids: vec!["track/987655".to_owned()],
                index: 0
            }
        );
    }

    #[test]
    fn a_queue_replaces_only_around_the_current_track() {
        let q = |ids: &[&str]| ids.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        assert_eq!(
            queue_verdict(Some("track/2"), &q(&["track/1", "track/2"]), 1),
            "replaced"
        );
        assert_eq!(
            queue_verdict(Some("track/2"), &q(&["track/1", "track/2"]), 0),
            "rejected_not_current"
        );
        assert_eq!(
            queue_verdict(Some("track/2"), &q(&["track/2"]), 3),
            "rejected_index"
        );
        assert_eq!(
            queue_verdict(None, &q(&["track/2"]), 0),
            "rejected_nothing_playing"
        );
    }

    #[test]
    fn the_volume_is_clamped_and_kept() {
        assert_eq!(volume(), 1.0);
        assert_eq!(set_volume(0.25), 0.25);
        assert_eq!(volume(), 0.25);
        assert_eq!(set_volume(3.0), 1.0);
        assert_eq!(set_volume(-1.0), 0.0);
        assert_eq!(set_volume(f32::NAN), 0.0);
    }

    #[test]
    fn an_unlisted_id_becomes_a_bare_playable_row() {
        let item = item_for("track/987654");
        assert_eq!(item.id, "track/987654");
        assert!(item.playable && item.title.is_empty());
    }

    #[test]
    fn sources_and_commands_have_distinct_labels() {
        assert_ne!(Source::Car.label(), Source::App.label());
        let mut labels = [
            Command::Play {
                ids: Vec::new(),
                index: 0,
            },
            Command::Pause,
            Command::Resume,
            Command::Seek(0),
            Command::Next,
            Command::Previous,
            Command::Stop,
            Command::SetQueue {
                ids: Vec::new(),
                index: 0,
            },
            Command::Volume,
        ]
        .map(|c| c.label());
        labels.sort_unstable();
        let n = labels.len();
        let mut unique = labels.to_vec();
        unique.dedup();
        assert_eq!(unique.len(), n);
    }
}
