//! State shared between the tray menu, the config window and the background
//! tasks (API fetches, ICY metadata).
//!
//! The player thread owns the playback truth; it mirrors state changes here
//! through events, and the UI sends it commands.

use crate::api::{Library, Station};
use crate::config::Config;

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
    /// The tray menu/icon needs to be (re)built.
    pub menu_dirty: bool,
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
            menu_dirty: true,
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

    /// The name of the device the tray is controlling, if any.
    pub fn remote_target_name(&self) -> Option<&str> {
        let target = self.remote_target.as_ref()?;
        self.remote_devices
            .iter()
            .find(|d| d.id == *target)?
            .name
            .as_str()
            .into()
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
