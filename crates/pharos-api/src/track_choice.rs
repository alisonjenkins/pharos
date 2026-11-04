//! The wire form of a group track choice (`TrackChoice` in the schema), shared
//! by the server that writes it and the client that reads it.

use crate::pharos_capnp::track_choice;

/// One of the group's track choices: a stream index of the playing item.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TrackChoice {
    /// The group has made no choice; each member keeps the player's default.
    #[default]
    Unset,
    /// The stream with this index.
    Index(u32),
    /// Subtitles off.
    Off,
}

pub fn write(mut out: track_choice::Builder<'_>, choice: TrackChoice) {
    match choice {
        TrackChoice::Unset => {}
        TrackChoice::Index(index) => {
            out.set_set(true);
            out.set_index(index);
        }
        TrackChoice::Off => {
            out.set_set(true);
            out.set_off(true);
        }
    }
}

pub fn read(choice: track_choice::Reader<'_>) -> TrackChoice {
    match (choice.get_set(), choice.get_off()) {
        (false, _) => TrackChoice::Unset,
        (true, true) => TrackChoice::Off,
        (true, false) => TrackChoice::Index(choice.get_index()),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn every_choice_survives_the_wire() {
        for choice in [TrackChoice::Unset, TrackChoice::Off, TrackChoice::Index(7)] {
            let mut message = capnp::message::Builder::new_default();
            write(message.init_root::<track_choice::Builder<'_>>(), choice);
            let back = read(
                message
                    .get_root_as_reader::<track_choice::Reader<'_>>()
                    .unwrap(),
            );
            assert_eq!(back, choice);
        }
    }
}
