//! Tray menu construction (rebuilt whenever `Shared::menu_dirty` is set).
//!
//! Layout:
//! ```text
//! Now playing info (disabled lines: station / track / artist)
//! ---
//! Stop stream [X]  |  Play stream [X]        (single toggling line)
//! ---
//! Playlist submenus with plain station buttons
//! ---
//! Library / site / autostart / key
//! ---
//! Quit
//! ```
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

pub fn build_menu(s: &Shared) -> Menu {
    let menu = Menu::new();

    // -- Now playing info (disabled lines at the top) ---------------------
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

    let _ = menu.append(&tray_icon::menu::PredefinedMenuItem::separator());

    // -- Single toggling play/stop line ------------------------------------
    let (pp_label, pp_enabled) = match s.playback {
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
    };
    let _ = menu.append(&MenuItem::with_id(PLAY_STOP, pp_label, pp_enabled, None));

    let _ = menu.append(&tray_icon::menu::PredefinedMenuItem::separator());

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
