//! State shared between the tray menu, the config window and the background
//! tasks (API fetches, ICY metadata).
//!
//! The player thread owns the playback truth; it mirrors state changes here
//! through events, and the UI sends it commands.

use crate::api::{Library, Station};
use crate::config::Config;
use crate::history::{self, HistoryEntry};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Playback {
    Stopped,
    /// Stream is being set up / reconnected.
    Loading,
    Playing,
}

/// The playback state of a (remote) player, in the remote-control
/// protocol's words: `stopped`, `connecting`, `playing`, `paused`,
/// `error`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteState {
    pub state: String,
    /// 0..=100.
    pub volume: u32,
    /// `"normal"` or `"bitperfect"` (the tray has no mode of its own).
    pub mode: Option<String>,
    pub station_id: Option<String>,
}

/// A device on the account's "Playing on…" list (the other online
/// players, as reported by the remote-control server; devices that
/// cannot play are filtered out).
#[derive(Debug, Clone)]
pub struct RemoteDevice {
    pub id: String,
    pub name: String,
    /// Last reported state; `None` until the device publishes one.
    pub state: Option<RemoteState>,
}

#[derive(Debug)]
pub struct Shared {
    pub config: Config,
    pub library: Option<Library>,
    pub library_loading: bool,
    pub library_error: Option<String>,
    /// Fetched station icon bytes: (icon url, data). Consumed by the tray
    /// main loop, which turns them into the tray icon.
    pub icon_bytes: Option<(String, Vec<u8>)>,
    /// Icon url whose fetch is in flight (avoids spawning duplicate tasks).
    pub icon_fetch_for: Option<String>,
    /// Icon url whose fetch failed (avoids retry spam every frame).
    pub icon_failed_for: Option<String>,
    pub current: Option<Station>,
    pub playback: Playback,
    pub playback_error: Option<String>,
    /// Current ICY stream title (the track), if the stream provides it.
    pub icy_title: Option<String>,
    /// Current ICY artist, if present.
    pub icy_artist: Option<String>,
    /// Linear volume, 0.0..=1.0
    pub volume: f64,
    pub muted: bool,
    /// Remote control: the account's WebSocket is connected and the
    /// server speaks protocol >= 1 (the "Playing on…" feature).
    pub remote_connected: bool,
    /// The account's other online players ("Playing on…" list).
    pub remote_devices: Vec<RemoteDevice>,
    /// The device the tray is controlling; `None` = local playback.
    pub remote_target: Option<String>,
    /// The current playback was started by a remote command (not a local
    /// menu click), so the "new track" notification is suppressed.
    pub remote_initiated: bool,
    /// Playback history, newest first, for the tray window's list.
    pub history: Vec<HistoryEntry>,
    /// `history` changed and must be persisted to disk.
    pub history_dirty: bool,
    /// The tray window should repaint (now-playing / history changed).
    pub ui_dirty: bool,
}

/// A station as the tray window's station list shows it.
#[derive(Clone, Debug)]
pub struct StationRow {
    pub id: String,
    pub name: String,
    pub icon_url: Option<String>,
    /// This is the station the tray is (or last) played.
    pub is_current: bool,
}

/// A group of stations: one playlist, or the "No playlist" bucket.
#[derive(Clone, Debug)]
pub struct StationSection {
    pub name: String,
    pub stations: Vec<StationRow>,
}

/// A device on the "Playing on…" list as the window's selector shows it.
#[derive(Clone, Debug)]
pub struct RemoteRow {
    pub id: String,
    pub name: String,
    /// Human-readable last state ("Playing", "Idle", …).
    pub state_text: String,
}

#[derive(Clone, Debug)]
pub struct UiSnapshot {
    /// Name of the playing station, if any.
    pub station: Option<String>,
    /// Icon URL of the playing station, if it has one. Fallback for the
    /// now-playing art when the track has no album art.
    pub station_icon_url: Option<String>,
    /// ICY title (track), if present.
    pub title: Option<String>,
    /// ICY artist, if present.
    pub artist: Option<String>,
    /// Whether this tray's player is actively playing.
    pub playing: bool,
    /// Whether this tray's stream is active (connecting or playing). When
    /// it is not, the now-playing panel shows no stale art or track info:
    /// `current` (the last played station) is kept for resuming, but it
    /// is not "playing".
    pub live: bool,
    /// A short human state string ("Playing", "Connecting…", …).
    pub state_text: String,
    /// A play from the window makes sense (stopped with a known station).
    pub can_play: bool,
    /// A stop from the window makes sense (playing or connecting).
    pub can_stop: bool,
    /// Playback history, newest first.
    pub history: Vec<HistoryEntry>,
    /// Stations grouped by playlist for the station list.
    pub sections: Vec<StationSection>,
    /// Status message when there is no station list yet (loading / error /
    /// missing key), or when the library is empty.
    pub library_note: Option<String>,
    /// Remote control: the account's WebSocket is connected ("Playing on…").
    pub remote_connected: bool,
    /// The account's other online players.
    pub remote_devices: Vec<RemoteRow>,
    /// The device being controlled; `None` = local playback.
    pub remote_target: Option<String>,
    /// Volume (0..=100) of the controlled remote, when it has reported one.
    pub remote_volume: Option<u32>,
}

/// A remote player's state, in human words (the web app's wording).
pub fn remote_state_label(state: &str) -> &'static str {
    match state {
        "playing" => "Playing",
        "connecting" => "Connecting…",
        "paused" => "Paused",
        "error" => "Error",
        "stopped" => "Stopped",
        _ => "Idle",
    }
}

impl Shared {
    pub fn new(config: Config) -> Self {
        let volume = config.volume;
        Self {
            config,
            library: None,
            library_loading: false,
            library_error: None,
            icon_bytes: None,
            icon_fetch_for: None,
            icon_failed_for: None,
            current: None,
            playback: Playback::Stopped,
            playback_error: None,
            icy_title: None,
            icy_artist: None,
            volume,
            muted: false,
            remote_connected: false,
            remote_devices: Vec::new(),
            remote_target: None,
            remote_initiated: false,
            history: Vec::new(),
            history_dirty: false,
            ui_dirty: true,
        }
    }

    /// A cheap clone of just the fields the tray window draws, so the UI
    /// never holds the shared mutex while egui renders.
    pub fn clone_snapshot(&self) -> UiSnapshot {
        let state_text = match self.playback {
            Playback::Playing => "Playing",
            Playback::Loading => "Connecting…",
            Playback::Stopped if self.playback_error.is_some() => "Error",
            Playback::Stopped => "Stopped",
        }
        .to_string();

        // Station list for the window's Stations tab.
        let mut sections = Vec::new();
        let mut library_note = None;
        match self.library.as_ref() {
            Some(lib) => {
                let current_id = self.current.as_ref().map(|c| c.id.as_str());
                for pl in &lib.playlists {
                    if pl.stations.is_empty() {
                        continue;
                    }
                    sections.push(StationSection {
                        name: pl.playlist.name.clone(),
                        stations: pl
                            .stations
                            .iter()
                            .map(|st| StationRow {
                                id: st.id.clone(),
                                name: st.name.clone(),
                                icon_url: st.icon_url.clone(),
                                is_current: Some(st.id.as_str()) == current_id,
                            })
                            .collect(),
                    });
                }
                if !lib.unassigned.is_empty() {
                    sections.push(StationSection {
                        name: "No playlist".to_string(),
                        stations: lib
                            .unassigned
                            .iter()
                            .map(|st| StationRow {
                                id: st.id.clone(),
                                name: st.name.clone(),
                                icon_url: st.icon_url.clone(),
                                is_current: Some(st.id.as_str()) == current_id,
                            })
                            .collect(),
                    });
                }
                if sections.is_empty() {
                    library_note = Some("No stations yet (add some on radiopit.fm)".to_string());
                }
            }
            None => {
                library_note = Some(if self.library_loading {
                    "Loading library…".to_string()
                } else if self.library_error.is_some() {
                    "Library unavailable".to_string()
                } else if self.config.api_key.is_empty() {
                    "Enter your key in Configuration…".to_string()
                } else {
                    "Connecting…".to_string()
                });
            }
        }

        // Remote control ("Playing on…") for the window's device selector.
        let remote_devices = self
            .remote_devices
            .iter()
            .map(|d| RemoteRow {
                id: d.id.clone(),
                name: d.name.clone(),
                state_text: d
                    .state
                    .as_ref()
                    .map(|st| remote_state_label(&st.state).to_string())
                    .unwrap_or_else(|| "Idle".to_string()),
            })
            .collect();
        let remote_target_state = self.remote_target_state();
        let remote_active = matches!(
            remote_target_state.map(|st| st.state.as_str()),
            Some("playing") | Some("connecting")
        );

        // The transport buttons act on the controlled remote when one is
        // selected, otherwise on this tray's own player.
        let (can_play, can_stop) = if self.remote_target.is_some() {
            (!remote_active, remote_active)
        } else {
            (
                self.playback == Playback::Stopped && self.current.is_some(),
                self.playback != Playback::Stopped,
            )
        };

        UiSnapshot {
            station: self.current.as_ref().map(|c| c.name.clone()),
            station_icon_url: self.current.as_ref().and_then(|c| c.icon_url.clone()),
            title: self.icy_title.clone(),
            artist: self.icy_artist.clone(),
            playing: self.playback == Playback::Playing,
            live: matches!(self.playback, Playback::Loading | Playback::Playing),
            can_play,
            can_stop,
            state_text,
            history: self.history.clone(),
            sections,
            library_note,
            remote_connected: self.remote_connected,
            remote_devices,
            remote_target: self.remote_target.clone(),
            remote_volume: remote_target_state.map(|st| st.volume),
        }
    }

    /// This tray's own playback state, in the remote-control protocol's
    /// words (what the other devices would see in the "Playing on…" list).
    pub fn local_remote_state(&self) -> RemoteState {
        let state = if self.playback == Playback::Playing {
            "playing"
        } else if self.playback == Playback::Loading {
            "connecting"
        } else if self.playback_error.is_some() {
            "error"
        } else {
            "stopped"
        }
        .to_string();
        // Muted reads as volume 0 to the other devices (the protocol has
        // no mute field); unmuted by a remote volume > 0.
        let volume = if self.muted {
            0
        } else {
            (self.volume * 100.0).round() as u32
        };
        RemoteState {
            state,
            volume,
            mode: Some("normal".to_string()),
            station_id: self.current.as_ref().map(|s| s.id.clone()),
        }
    }

    /// The state of the device the tray is controlling, if any.
    pub fn remote_target_state(&self) -> Option<&RemoteState> {
        let target = self.remote_target.as_ref()?;
        self.remote_devices
            .iter()
            .find(|d| d.id == *target)?
            .state
            .as_ref()
    }

    /// Record a freshly started track at the front of the history (newest
    /// first) and flag it for persistence.
    pub fn record_track(&mut self, station: String, artist: Option<String>, title: String) {
        history::push(&mut self.history, HistoryEntry::new(station, artist, title));
        self.history_dirty = true;
    }

    /// Set the resolved artwork URL on the newest history entry that
    /// matches this track, so the tray window can fetch its thumbnail.
    pub fn set_latest_artwork(&mut self, title: &str, artist: Option<&str>, url: String) {
        if let Some(e) = self
            .history
            .iter_mut()
            .find(|e| e.title == title && e.artist.as_deref() == artist)
        {
            if e.artwork_url.is_none() {
                e.artwork_url = Some(url);
                self.history_dirty = true;
            }
        }
    }

    /// After a library (re)fetch: refresh `current` with the fresh station
    /// data, or restore the last played station from the config.
    pub fn restore_current(&mut self) {
        if let (Some(lib), Some(cur)) = (self.library.as_ref(), self.current.as_ref()) {
            self.current = lib.by_id(&cur.id).cloned();
        } else if self.current.is_none() {
            self.current = self
                .config
                .last_station_id
                .clone()
                .and_then(|id| self.library.as_ref().and_then(|l| l.by_id(&id).cloned()));
        }
    }
}
