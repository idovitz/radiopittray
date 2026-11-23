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
            menu_dirty: true,
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
