//! A title's versions (spec 022 FR-002): the files one film or episode can
//! play from, how the picker names them, and which one plays. Shared by
//! every reason a title has more than one file (a 3D edition, a cut, a
//! second rip); a new fact about a version extends [`version_label`].

use pharos_client::api::{ItemRef, Stereo, Version};

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
/// the first 3D one when `prefer_3d`, else the primary (`""`).
pub fn chosen_version<'a>(
    versions: &'a [Version],
    stored: Option<&str>,
    prefer_3d: bool,
) -> &'a str {
    if let Some(v) = stored.and_then(|s| versions.iter().find(|v| v.id == s)) {
        return &v.id;
    }
    if prefer_3d {
        if let Some(v) = versions
            .iter()
            .find(|v| v.stereo.is_some_and(|s| s != Stereo::Mvc))
        {
            return &v.id;
        }
    }
    ""
}

#[cfg(test)]
mod tests {
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
        assert_eq!(chosen_version(&versions, Some("3d-hsbs"), false), "3d-hsbs");
        assert_eq!(chosen_version(&versions, Some(""), true), "");
        assert_eq!(chosen_version(&versions, Some("gone"), false), "");
        assert_eq!(chosen_version(&versions, None, false), "");
        assert_eq!(chosen_version(&versions, None, true), "3d-hsbs");
    }

    #[test]
    fn a_headset_never_defaults_to_mvc() {
        let versions = [version("", "", None), version("m", "", Some(Stereo::Mvc))];
        assert_eq!(chosen_version(&versions, None, true), "");
    }
}
