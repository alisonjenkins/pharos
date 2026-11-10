//! Settings → Devices, "Your devices": the devices the user is signed in on,
//! each with its sound-format toggles (T130, ADR-0095). The person holding a
//! device knows when a format plays badly on it, so they turn it off here
//! themselves; an admin can do the same for any device in Manage → Devices.
//! Pure, so it renders under `dioxus-ssr`.

use std::collections::BTreeSet;

use dioxus::prelude::*;
use pharos_client::api::MyDevice;

use crate::client::DeviceEntry;
use crate::views::admin::DeviceSoundFormats;

/// What the section shows while its list loads, once it has, or when it failed.
#[derive(Debug, Clone, PartialEq)]
pub enum MyDevicesState {
    Loading,
    Loaded(Vec<MyDevice>),
    Failed(String),
}

/// The toggles' view of a device.
fn entry(d: &MyDevice) -> DeviceEntry {
    DeviceEntry {
        id: d.id.clone(),
        name: d.name.clone(),
        no_direct_play_audio_codecs: d.converted_audio.clone(),
        ..Default::default()
    }
}

/// `devices` with `device_id`'s list replaced by `codecs`, as stored.
pub fn with_saved(
    mut devices: Vec<MyDevice>,
    device_id: &str,
    codecs: Vec<String>,
) -> Vec<MyDevice> {
    if let Some(d) = devices.iter_mut().find(|d| d.id == device_id) {
        d.converted_audio = codecs;
    }
    devices
}

/// The section: one row per device, this one first and marked. `saving`
/// holds the devices whose save is in flight; their toggles wait. A toggle
/// hands `on_change` the device and its whole new list.
#[component]
pub fn MyDevicesView(
    state: MyDevicesState,
    saving: BTreeSet<String>,
    problem: Option<String>,
    on_change: EventHandler<(String, Vec<String>)>,
) -> Element {
    let mut devices = match &state {
        MyDevicesState::Loaded(d) => d.clone(),
        _ => Vec::new(),
    };
    // This device first, the rest as served.
    devices.sort_by_key(|d| !d.current);
    rsx! {
        section { class: "pharos-settings-page pharos-my-devices",
            h2 { "Your devices" }
            p { class: "pharos-settings-lede",
                "If a device plays a sound format out of sync or not at all, turn it off here. "
                "Pharos then converts that format for the device instead of sending it as it is."
            }
            match &state {
                MyDevicesState::Loading => rsx! { p { class: "pharos-notice", role: "status", "Loading your devices…" } },
                MyDevicesState::Failed(text) => rsx! { p { class: "pharos-error", role: "alert", "{text}" } },
                MyDevicesState::Loaded(_) if devices.is_empty() => rsx! {
                    p { class: "pharos-empty", "You are not signed in on any device." }
                },
                MyDevicesState::Loaded(_) => rsx! {
                    ul { class: "pharos-settings-group pharos-my-devices-list",
                        for d in devices.iter() {
                            li { key: "{d.id}", class: "pharos-setting pharos-my-device",
                                p { class: "pharos-setting-name pharos-my-device-name",
                                    "{d.name}"
                                    if d.current {
                                        span { class: "pharos-my-device-current", " (this device)" }
                                    }
                                }
                                DeviceSoundFormats {
                                    device: entry(d),
                                    busy: saving.contains(&d.id),
                                    on_change: {
                                        let id = d.id.clone();
                                        move |codecs| on_change.call((id.clone(), codecs))
                                    },
                                }
                            }
                        }
                    }
                },
            }
            if let Some(text) = problem {
                p { class: "pharos-error", role: "alert", "{text}" }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    fn device(id: &str, current: bool, converted: &[&str]) -> MyDevice {
        MyDevice {
            id: id.into(),
            name: id.to_uppercase(),
            current,
            converted_audio: converted.iter().map(|c| (*c).to_string()).collect(),
        }
    }

    fn render(state: MyDevicesState, saving: &[&str]) -> String {
        let saving: BTreeSet<String> = saving.iter().map(|s| (*s).to_string()).collect();
        let mut dom = VirtualDom::new_with_props(
            |p: (MyDevicesState, BTreeSet<String>)| {
                rsx! {
                    MyDevicesView { state: p.0, saving: p.1, problem: None, on_change: |_| {} }
                }
            },
            (state, saving),
        );
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    #[test]
    fn this_device_comes_first_with_its_formats_pressed() {
        let html = render(
            MyDevicesState::Loaded(vec![
                device("phone", false, &[]),
                device("tv", true, &["eac3"]),
            ]),
            &[],
        );
        let tv = html.find("TV").expect("tv listed");
        let phone = html.find("PHONE").expect("phone listed");
        assert!(tv < phone, "this device first: {html}");
        assert!(html.contains("(this device)"), "{html}");
        assert_eq!(html.matches(r#"aria-pressed="true""#).count(), 1, "{html}");
    }

    #[test]
    fn a_device_being_saved_has_its_toggles_disabled() {
        let html = render(
            MyDevicesState::Loaded(vec![device("tv", true, &[]), device("phone", false, &[])]),
            &["tv"],
        );
        assert_eq!(html.matches("disabled").count(), 4, "{html}");
    }

    #[test]
    fn loading_failure_and_no_devices_say_so() {
        assert!(render(MyDevicesState::Loading, &[]).contains("Loading your devices"));
        assert!(render(MyDevicesState::Failed("down".into()), &[]).contains("down"));
        assert!(
            render(MyDevicesState::Loaded(Vec::new()), &[]).contains("not signed in on any device")
        );
    }

    #[test]
    fn a_saved_list_replaces_that_device_only() {
        let after = with_saved(
            vec![device("tv", true, &[]), device("phone", false, &["dts"])],
            "tv",
            vec!["eac3".into()],
        );
        assert_eq!(after[0].converted_audio, ["eac3"]);
        assert_eq!(after[1].converted_audio, ["dts"]);
    }
}
