//! Tray menu construction (rebuilt whenever `Shared::menu_dirty` is set).
//!
//! Layout:
//! ```text
//! Now playing info (disabled lines: station / track / artist)
//! ---
//! Stop stream [X]  |  Play stream [X]        (single toggling line)
//! ---
//! Control device: (submenu: this device + the account's other online
//! players; checked = which one is being controlled)
//! Volume −10 / Volume +10                      (while a remote is controlled)
//! ---
//! Playlist submenus with plain station buttons
//! ---
//! Library / site / autostart / key
//! ---
//! Quit
//! ```
//!
//! While a remote device is being controlled, the info lines mirror
//! that device's reported state, the play/stop line sends PLAY/PAUSE
//! to it, and station clicks send it a SET_STATION — the local player
//! stays untouched.
//!
//! DBusMenu has no notion of a maximum item width — long station names or
//! track titles would stretch the menu indefinitely — so labels are
//! truncated to [`MAX_LABEL`] characters with an ellipsis.

use tray_icon::menu::{Menu, MenuItem, Submenu};

use crate::state::{Playback, Shared};

pub const PLAY_STOP: &str = "play-stop";
pub const REFRESH: &str = "refresh";
pub const WEB: &str = "web";
pub const CONFIG: &str = "configuration";
pub const QUIT: &str = "quit";
/// "Control device:" — switch back to controlling this (local) device.
pub const REMOTE_LOCAL: &str = "remote:local";
/// Volume step for the controlled remote (10, like the web app's keys).
pub const VOL_UP: &str = "vol-up";
pub const VOL_DOWN: &str = "vol-down";

/// Maximum length of a menu label; longer text is cut with an ellipsis.
const MAX_LABEL: usize = 40;
/// Budget for the station name inside "Play/Stop stream [name]".
const MAX_LABEL_PLAYSTOP: usize = 25;

/// Cut `s` to at most `max` characters (character-boundary safe), adding
/// an ellipsis when something was cut.
fn label(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max - 1).collect();
        format!("{cut}…")
    }
}

/// The station id for a menu item, or the plain id for other items.
pub fn station_id(id: &str) -> String {
    format!("st:{id}")
}

/// The menu id for a device in the "Playing on…" list.
pub fn remote_id(device_id: &str) -> String {
    format!("remote:{device_id}")
}

/// A remote player's state as a label (the web app's wording).
pub fn state_label(state: &str) -> &'static str {
    match state {
        "playing" => "Playing",
        "connecting" => "Connecting…",
        "paused" => "Paused",
        "error" => "Error",
        "stopped" => "Stopped",
        _ => "Idle",
    }
}

pub fn build_menu(s: &Shared) -> Menu {
    let menu = Menu::new();

    // -- Now playing info (disabled lines at the top) ---------------------
    // While a remote device is being controlled, these mirror the
    // state it reports instead of this tray's own player.
    if let Some(name) = s.remote_target_name() {
        let st = s.remote_target_state();
        let state = st.map(|st| state_label(&st.state)).unwrap_or("…");
        let _ = menu.append(&MenuItem::with_id(
            "info-remote",
            label(&format!("Controlling: {name} — {state}"), MAX_LABEL),
            false,
            None,
        ));
        if let Some(station_name) = st
            .and_then(|st| st.station_id.as_deref())
            .and_then(|id| s.library.as_ref().and_then(|l| l.by_id(id)))
            .map(|st| st.name.clone())
        {
            let _ = menu.append(&MenuItem::with_id(
                "info-remote-station",
                label(&station_name, MAX_LABEL),
                false,
                None,
            ));
        }
    } else {
        match &s.current {
            Some(st) => {
                let _ = menu.append(&MenuItem::with_id(
                    "info-station",
                    label(&st.name, MAX_LABEL),
                    false,
                    None,
                ));
                if let Some(t) = &s.icy_title {
                    let _ = menu.append(&MenuItem::with_id(
                        "info-title",
                        label(t, MAX_LABEL),
                        false,
                        None,
                    ));
                }
                if let Some(a) = &s.icy_artist {
                    let _ = menu.append(&MenuItem::with_id(
                        "info-artist",
                        label(a, MAX_LABEL),
                        false,
                        None,
                    ));
                }
            }
            None => {
                let _ = menu.append(&MenuItem::with_id(
                    "info-none",
                    "Nothing playing",
                    false,
                    None,
                ));
            }
        }
    }

    let _ = menu.append(&tray_icon::menu::PredefinedMenuItem::separator());

    // -- Single toggling play/stop line ------------------------------------
    // With a remote controlled, it play/stops the remote instead.
    let (pp_label, pp_enabled) = if let Some(name) = s.remote_target_name() {
        let remote_active = matches!(
            s.remote_target_state().map(|st| st.state.as_str()),
            Some("playing") | Some("connecting")
        );
        if remote_active {
            (
                format!("Stop on [{}]", label(name, MAX_LABEL_PLAYSTOP)),
                true,
            )
        } else {
            (format!("Play on [{}]", label(name, MAX_LABEL_PLAYSTOP)), true)
        }
    } else {
        match s.playback {
            Playback::Stopped => match &s.current {
                Some(st) => (
                    format!("Play stream [{}]", label(&st.name, MAX_LABEL_PLAYSTOP)),
                    true,
                ),
                None => ("Play stream".to_string(), false),
            },
            Playback::Loading | Playback::Playing => match &s.current {
                Some(st) => (
                    format!("Stop stream [{}]", label(&st.name, MAX_LABEL_PLAYSTOP)),
                    true,
                ),
                None => ("Stop stream".to_string(), false),
            },
        }
    };
    let _ = menu.append(&MenuItem::with_id(PLAY_STOP, pp_label, pp_enabled, None));

    let _ = menu.append(&tray_icon::menu::PredefinedMenuItem::separator());

    // -- "Control device:": the account's other online players --------------
    // The checked entry is the device the tray is controlling; "This
    // device" (checked) means local playback.
    if s.remote_connected {
        let sub = Submenu::with_id("remote-menu", "Control device:", true);
        // This menu item type has no checked state, so the selection
        // is marked with a prefix instead.
        let local = if s.remote_target.is_none() {
            "✓ This device"
        } else {
            "This device"
        };
        let _ = sub.append(&MenuItem::with_id(REMOTE_LOCAL, local, true, None));
        for d in &s.remote_devices {
            let mut text = label(&d.name, 27);
            if let Some(st) = &d.state {
                text.push_str(&format!(" — {}", state_label(&st.state)));
            }
            if s.remote_target.as_deref() == Some(d.id.as_str()) {
                text = format!("✓ {text}");
            }
            let _ = sub.append(&MenuItem::with_id(remote_id(&d.id), text, true, None));
        }
        if s.remote_devices.is_empty() {
            let _ = sub.append(&MenuItem::with_id(
                "remote-none",
                "No other players online",
                false,
                None,
            ));
        }
        let _ = menu.append(&sub);

        // Volume steering for the controlled device (10 steps, the web
        // app's keyboard increment).
        if s.remote_target.is_some() {
            let _ = menu.append(&MenuItem::with_id(VOL_DOWN, "Volume −10", true, None));
            let _ = menu.append(&MenuItem::with_id(VOL_UP, "Volume +10", true, None));
        }
        let _ = menu.append(&tray_icon::menu::PredefinedMenuItem::separator());
    }

    // -- Station lists ------------------------------------------------------
    match &s.library {
        Some(lib) => {
            let has_anything = !lib.playlists.is_empty() || !lib.unassigned.is_empty();
            if !has_anything {
                let _ = menu.append(&MenuItem::with_id(
                    "empty",
                    "No stations yet (add some on radiopit.fm)",
                    false,
                    None,
                ));
            }
            for pl in &lib.playlists {
                if pl.stations.is_empty() {
                    continue;
                }
                let sub = Submenu::with_id(
                    format!("pl:{}", pl.playlist.id),
                    label(&pl.playlist.name, MAX_LABEL),
                    true,
                );
                for st in &pl.stations {
                    let item = MenuItem::with_id(
                        station_id(&st.id),
                        label(&st.name, MAX_LABEL),
                        true,
                        None,
                    );
                    let _ = sub.append(&item);
                }
                let _ = menu.append(&sub);
            }
            if !lib.unassigned.is_empty() {
                let sub = Submenu::with_id("pl:unassigned", "No playlist", true);
                for st in &lib.unassigned {
                    let item = MenuItem::with_id(
                        station_id(&st.id),
                        label(&st.name, MAX_LABEL),
                        true,
                        None,
                    );
                    let _ = sub.append(&item);
                }
                let _ = menu.append(&sub);
            }
        }
        None => {
            let label = if s.library_loading {
                "Loading library…"
            } else if s.library_error.is_some() {
                "Library unavailable"
            } else if s.config.api_key.is_empty() {
                "Enter your key…"
            } else {
                "Connecting…"
            };
            let _ = menu.append(&MenuItem::with_id("loading", label, false, None));
        }
    }

    let _ = menu.append(&tray_icon::menu::PredefinedMenuItem::separator());

    // -- Settings ------------------------------------------------------------
    let _ = menu.append(&MenuItem::with_id(REFRESH, "Refresh library", true, None));
    let _ = menu.append(&MenuItem::with_id(WEB, "Open radiopit.fm", true, None));
    let _ = menu.append(&MenuItem::with_id(CONFIG, "Configuration…", true, None));
    let _ = menu.append(&tray_icon::menu::PredefinedMenuItem::separator());
    let _ = menu.append(&MenuItem::with_id(QUIT, "Quit", true, None));

    menu
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncates_long_labels() {
        let long = "a".repeat(60);
        let t = label(&long, MAX_LABEL);
        assert_eq!(t.chars().count(), MAX_LABEL);
        assert!(t.ends_with('…'));
    }

    #[test]
    fn leaves_short_labels_alone() {
        assert_eq!(label("NPO Radio 1", MAX_LABEL), "NPO Radio 1");
    }

    #[test]
    fn truncates_on_char_boundaries() {
        let s = "éééé".repeat(15); // 60 non-ASCII chars
        let t = label(&s, MAX_LABEL);
        assert_eq!(t.chars().count(), MAX_LABEL);
        assert!(t.ends_with('…'));
    }
}
