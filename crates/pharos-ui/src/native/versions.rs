//! A title's versions (V721): the files one film or episode can play from,
//! how the picker names them, and which one plays. Shared by every reason a
//! title has more than one file (a 3D edition, a cut, a second rip).

use pharos_client::api::{ItemRef, Version};

use super::stereo::packing_label;

/// The `localStorage` key holding the version last picked for `title`.
pub fn version_key(title: ItemRef) -> String {
    format!("pharos.version.{title}")
}

/// A version as the picker lists it: "2D" or "3D (half side-by-side)", then
/// its edition name when it has one that says more.
pub fn version_label(version: &Version) -> String {
    let kind = match version.stereo {
        None => "2D".to_owned(),
        Some(s) => format!("3D ({})", packing_label(s)),
    };
    let name = version.name.trim();
    if name.is_empty() || name.eq_ignore_ascii_case("3d") || name.eq_ignore_ascii_case("2d") {
        kind
    } else {
        format!("{kind} · {name}")
    }
}

/// The version to play: the stored pick when the title still has it, else
/// the primary (`""`).
pub fn chosen_version<'a>(versions: &'a [Version], stored: Option<&str>) -> &'a str {
    stored
        .and_then(|s| versions.iter().find(|v| v.id == s))
        .map_or("", |v| v.id.as_str())
}

#[cfg(test)]
mod tests {
    use pharos_client::api::Stereo;

    use super::*;

    fn version(id: &str, name: &str, stereo: Option<Stereo>) -> Version {
        Version {
            id: id.into(),
            name: name.into(),
            stereo,
            primary: id.is_empty(),
            ..Default::default()
        }
    }

    #[test]
    fn labels_say_2d_or_3d_and_the_packing() {
        assert_eq!(version_label(&version("", "", None)), "2D");
        assert_eq!(
            version_label(&version("3d-hsbs", "3D", Some(Stereo::HalfSideBySide))),
            "3D (half side-by-side)"
        );
        assert_eq!(
            version_label(&version("x", "Extended", Some(Stereo::FullTopAndBottom))),
            "3D (full top-and-bottom) · Extended"
        );
        assert_eq!(
            version_label(&version("m", "", Some(Stereo::Mvc))),
            "3D (MVC, plays in 2D here)"
        );
    }

    #[test]
    fn the_stored_pick_wins_while_the_title_has_it() {
        let versions = [
            version("", "", None),
            version("3d-hsbs", "", Some(Stereo::HalfSideBySide)),
        ];
        assert_eq!(chosen_version(&versions, Some("3d-hsbs")), "3d-hsbs");
        assert_eq!(chosen_version(&versions, Some("")), "");
        assert_eq!(chosen_version(&versions, Some("gone")), "");
    }

    #[test]
    fn nothing_stored_plays_the_primary_even_with_a_3d_version() {
        let versions = [
            version("", "", None),
            version("3d-hsbs", "", Some(Stereo::HalfSideBySide)),
        ];
        assert_eq!(chosen_version(&versions, None), "");
    }
}
