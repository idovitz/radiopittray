//! The windowless tray application.
//!
//! No GUI window exists in this process at all: the tray icon
//! (StatusNotifierItem via libappindicator) carries the menu and the
//! now-playing info, GStreamer runs in its own thread, and the API/ICY/icon
//! tasks live on a tokio runtime. The main thread is a plain tick loop:
//! drain menu and player events, pump the GLib main context (the tray menu
//! talks over D-Bus), watch the config file, and sync the tray.
//!
//! The config window is a separate process (`radiopittray --config`, see
//! configui.rs); this process notices its result by watching config.toml.

use std::collections::HashMap;
use std::fs::File;
use std::os::unix::io::AsRawFd;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use tray_icon::menu::MenuEvent;
use tray_icon::TrayIconBuilder;

use crate::api::{Client, Station};
use crate::config;
use crate::configui;
use crate::icy::{self, IcyCmd};
use crate::icon;
use crate::menu;
use crate::player::{self, Cmd, Ev};
use crate::state::{Playback, Shared};

/// What the tray icon currently shows.
#[derive(Clone, PartialEq, Debug)]
enum ShownIcon {
    /// The icon theme's "radio" icon (the .desktop entry's icon);
    /// bool = playing (green dot at the antenna tip).
    Theme(bool),
    /// The icon of the station with this id.
    Station(String),
}

struct TrayApp {
    shared: Arc<Mutex<Shared>>,
    tray: tray_icon::TrayIcon,
    cmd_tx: Sender<Cmd>,
    ev_rx: Receiver<Ev>,
    icy: icy::IcyHandle,
    /// Owns the tokio runtime, keeping its worker threads alive. A bare
    /// `Handle` would not: tasks spawned on a dropped runtime are abandoned.
    #[allow(dead_code)]
    runtime: tokio::runtime::Runtime,
    handle: tokio::runtime::Handle,
    /// Decoded station icons for the tray, keyed by station id (32×32 RGBA).
    tray_icon_cache: HashMap<String, Vec<u8>>,
    /// What the tray icon currently shows.
    tray_shown: ShownIcon,
    /// mtime of config.toml as last seen; a change means the config window
    /// (or an editor) saved something.
    config_mtime: Option<SystemTime>,
}

impl TrayApp {
    fn new(shared: Arc<Mutex<Shared>>) -> anyhow::Result<Self> {
        let (cmd_tx, cmd_rx) = mpsc::channel::<Cmd>();
        let (ev_tx, ev_rx) = mpsc::channel::<Ev>();
        player::spawn(cmd_rx, ev_tx);

        // Must outlive the app: the API fetches run as tasks on its workers.
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("building tokio runtime");
        let handle = runtime.handle().clone();

        // Apply the stored volume to the new player.
        let (volume, muted) = {
            let s = shared.lock().unwrap();
            (s.volume, s.muted)
        };
        let _ = cmd_tx.send(Cmd::SetVolume(volume));
        let _ = cmd_tx.send(Cmd::SetMute(muted));

        let icy = icy::IcyHandle::spawn(handle.clone(), shared.clone());

        let tray_menu = menu::build_menu(&shared.lock().unwrap());
        let tray = TrayIconBuilder::new()
            .with_id("radiopittray")
            .with_menu(Box::new(tray_menu))
            .with_icon(icon::idle_icon())
            .with_tooltip("RadioPit")
            .build()?;

        let config_mtime = std::fs::metadata(config::config_path())
            .ok()
            .and_then(|m| m.modified().ok());

        Ok(Self {
            shared,
            tray,
            cmd_tx,
            ev_rx,
            icy,
            runtime,
            handle,
            tray_icon_cache: HashMap::new(),
            tray_shown: ShownIcon::Theme(false),
            config_mtime,
        })
    }

    /// The main tick loop: no event loop library, just a 100 ms heartbeat.
    fn run(mut self) {
        // First run without a key: open the config window (a separate
        // process) so the user can enter one. With a key, load the library.
        let keyless = {
            let s = self.shared.lock().unwrap();
            s.config.api_key.is_empty()
        };
        if keyless {
            configui::spawn_config_window();
        } else {
            self.refresh_library();
        }

        loop {
            self.drain_player_events();
            self.drain_menu_events();
            self.watch_config();
            self.sync_tray();

            // The tray menu talks to us over D-Bus (libappindicator); keep
            // the GLib main context pumping so menu events get processed.
            #[cfg(target_os = "linux")]
            glib::MainContext::default().iteration(false);

            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// (Re)fetch the library in the background.
    fn refresh_library(&self) {
        let (base, key) = {
            let s = self.shared.lock().unwrap();
            (s.config.api_url.clone(), s.config.api_key.clone())
        };
        if key.is_empty() {
            return;
        }
        let client = Client::new(&base);
        let shared = self.shared.clone();
        self.handle.spawn(async move {
            {
                let mut s = shared.lock().unwrap();
                s.library_loading = true;
                s.library_error = None;
            }
            match client.fetch_library(&key).await {
                Ok(library) => {
                    let mut s = shared.lock().unwrap();
                    s.library = Some(library);
                    s.library_loading = false;
                    s.restore_current();
                    s.menu_dirty = true;
                }
                Err(e) => {
                    tracing::warn!("fetching library: {e:#}");
                    let mut s = shared.lock().unwrap();
                    s.library_loading = false;
                    s.library_error = Some(e.to_string());
                    s.menu_dirty = true;
                }
            }
        });
    }

    /// Play the given station (station button, or the "Play stream" line).
    /// Remembers it as the last played station and starts ICY metadata.
    fn play_station(&mut self, station: Station) {
        if let Err(e) = self.cmd_tx.send(Cmd::Play(station.clone())) {
            tracing::warn!("player thread gone: {e}");
            return;
        }
        let mut s = self.shared.lock().unwrap();
        s.current = Some(station.clone());
        s.playback = Playback::Loading;
        s.playback_error = None;
        s.icy_title = None;
        s.icy_artist = None;
        s.config.last_station_id = Some(station.id.clone());
        if let Err(e) = s.config.save() {
            tracing::warn!("saving config: {e:#}");
        }
        s.menu_dirty = true;
        self.icy.send(IcyCmd::Play(station.url));
    }

    fn stop(&mut self) {
        let _ = self.cmd_tx.send(Cmd::Stop);
        self.icy.send(IcyCmd::Stop);
    }

    fn drain_player_events(&mut self) {
        loop {
            let Ok(ev) = self.ev_rx.try_recv() else {
                break;
            };
            let mut s = self.shared.lock().unwrap();
            match ev {
                Ev::Playing => {
                    s.playback = Playback::Playing;
                    s.playback_error = None;
                }
                Ev::Stopped => s.playback = Playback::Stopped,
                Ev::Failed(msg) => {
                    s.playback = Playback::Stopped;
                    s.playback_error = Some(msg);
                    // The stream gave up; the metadata connection is dead too.
                    self.icy.send(IcyCmd::Stop);
                }
            }
            s.menu_dirty = true;
        }
    }

    fn drain_menu_events(&mut self) {
        while let Ok(ev) = MenuEvent::receiver().try_recv() {
            self.handle_menu_action(ev.id.as_ref());
        }
    }

    fn handle_menu_action(&mut self, id: &str) {
        use menu::*;
        match id {
            PLAY_STOP => match {
                let s = self.shared.lock().unwrap();
                s.playback
            } {
                Playback::Stopped => {
                    let station = {
                        let s = self.shared.lock().unwrap();
                        s.current.clone()
                    };
                    if let Some(st) = station {
                        self.play_station(st);
                    }
                }
                // Loading or playing: the line reads "Stop stream [X]".
                _ => self.stop(),
            }
            REFRESH => self.refresh_library(),
            WEB => open_url("https://radiopit.fm"),
            CONFIG => configui::spawn_config_window(),
            QUIT => {
                let _ = self.cmd_tx.send(Cmd::Stop);
                std::process::exit(0);
            }
            id if id.starts_with("st:") => {
                let station = {
                    let s = self.shared.lock().unwrap();
                    s.library
                        .as_ref()
                        .and_then(|lib| lib.by_id(id.strip_prefix("st:").unwrap()).cloned())
                };
                if let Some(st) = station {
                    self.play_station(st);
                }
            }
            _ => {}
        }
    }

    /// Pick up changes saved to config.toml (by the config window or an
    /// editor): a new key reloads the library, a cleared key resets it.
    fn watch_config(&mut self) {
        let mtime = std::fs::metadata(config::config_path())
            .ok()
            .and_then(|m| m.modified().ok());
        if mtime == self.config_mtime {
            return;
        }
        self.config_mtime = mtime;
        let conf = config::Config::load();
        let (old_key, old_volume) = {
            let s = self.shared.lock().unwrap();
            (s.config.api_key.clone(), s.volume)
        };
        if conf.api_key != old_key {
            let mut s = self.shared.lock().unwrap();
            s.config = conf.clone();
            if conf.api_key.is_empty() {
                s.library = None;
                s.library_loading = false;
                s.library_error = None;
                s.current = None;
                s.icy_title = None;
                s.icy_artist = None;
                s.menu_dirty = true;
            }
            drop(s);
            if conf.api_key.is_empty() {
                self.stop();
            } else {
                tracing::info!("api key changed, loading library");
                self.refresh_library();
            }
        } else {
            let mut s = self.shared.lock().unwrap();
            s.config = conf.clone();
        }
        if conf.volume != old_volume {
            let _ = self.cmd_tx.send(Cmd::SetVolume(conf.volume));
        }
    }

    /// Rebuild the tray menu/icon/tooltip when state has changed.
    fn sync_tray(&mut self) {
        let (tooltip, want, playing) = {
            let mut s = self.shared.lock().unwrap();
            if s.menu_dirty {
                let m = menu::build_menu(&s);
                self.tray.set_menu(Some(Box::new(m)));
                s.menu_dirty = false;
            }
            let tooltip = match &s.current {
                Some(st) => format!("RadioPit — {}", st.name),
                None => "RadioPit".to_string(),
            };
            let playing = s.playback == Playback::Playing;
            // The station icon is shown while a stream is active; when
            // none is available (fetch pending, no URL, failure) the
            // theme's radio icon shows, dotted while actually playing.
            let want = match s.playback {
                Playback::Loading | Playback::Playing => s
                    .current
                    .as_ref()
                    .map(|st| (st.id.clone(), st.icon_url.clone())),
                Playback::Stopped => None,
            };
            (tooltip, want, playing)
        };
        if let Some(ic) = self.next_tray_icon(want, playing) {
            let _ = self.tray.set_icon(Some(ic));
        }
        let _ = self.tray.set_tooltip(Some(tooltip));
    }

    /// Compute the tray icon for the desired state; returns the new icon
    /// only when something changed.
    fn next_tray_icon(
        &mut self,
        want: Option<(String, Option<String>)>,
        playing: bool,
    ) -> Option<tray_icon::Icon> {
        if let Some((sid, url)) = want {
            // 1) Cached station icon.
            if let Some(rgba) = self.tray_icon_cache.get(&sid) {
                if self.tray_shown == ShownIcon::Station(sid.clone()) {
                    return None;
                }
                self.tray_shown = ShownIcon::Station(sid);
                return icon::tray_icon_from_rgba(rgba.clone());
            }
            // 2) Fetch/decode this station's icon (falls back to the
            //    theme icon while the fetch is in flight or when there
            //    is none).
            if let Some(ic) = self.try_station_icon(&sid, url.as_deref()) {
                return Some(ic);
            }
        }
        self.apply_theme(playing)
    }

    /// Try to get (or start fetching) the icon for a station. Returns the
    /// ready icon, if any; on success `tray_shown` becomes that station.
    fn try_station_icon(&mut self, sid: &str, url: Option<&str>) -> Option<tray_icon::Icon> {
        let Some(url) = url else {
            return None;
        };

        // Bytes fetched by the background task?
        let bytes = {
            let mut s = self.shared.lock().unwrap();
            let match_url = s
                .icon_bytes
                .as_ref()
                .map(|(u, _)| u == url)
                .unwrap_or(false);
            if match_url {
                s.icon_bytes.take().map(|(_, b)| b)
            } else {
                None
            }
        };
        if let Some(b) = bytes {
            match image::load_from_memory(&b) {
                Ok(img) => {
                    let rgba = resize_rgba32(&img);
                    if let Some(ic) = icon::tray_icon_from_rgba(rgba.clone()) {
                        self.tray_icon_cache.insert(sid.to_string(), rgba);
                        self.tray_shown = ShownIcon::Station(sid.to_string());
                        return Some(ic);
                    }
                }
                Err(e) => {
                    tracing::warn!("decoding station icon: {e}");
                    // The bytes are consumed; don't re-fetch them every frame.
                    let mut s = self.shared.lock().unwrap();
                    s.icon_failed_for = Some(url.to_string());
                }
            }
            return None;
        }

        // Nothing fetched yet (and no known failure): start the fetch.
        let need_fetch = {
            let s = self.shared.lock().unwrap();
            s.icon_fetch_for.as_deref() != Some(url)
                && s.icon_failed_for.as_deref() != Some(url)
        };
        if need_fetch {
            let url = url.to_string();
            let mut s = self.shared.lock().unwrap();
            s.icon_fetch_for = Some(url.clone());
            let shared = self.shared.clone();
            self.handle.spawn(async move {
                match fetch_icon_bytes(&url).await {
                    Ok(b) => {
                        let mut s = shared.lock().unwrap();
                        s.icon_bytes = Some((url.clone(), b));
                        s.icon_fetch_for = None;
                    }
                    Err(e) => {
                        tracing::debug!("fetching station icon: {e:#}");
                        let mut s = shared.lock().unwrap();
                        s.icon_fetch_for = None;
                        s.icon_failed_for = Some(url);
                    }
                }
            });
        }
        None
    }

    /// Show the theme's "radio" icon (the same one the .desktop entry
    /// uses), with a green dot at the antenna tip while playing; falls
    /// back to the procedural logo when the theme has no such icon.
    fn apply_theme(&mut self, playing: bool) -> Option<tray_icon::Icon> {
        if self.tray_shown == ShownIcon::Theme(playing) {
            return None;
        }
        self.tray_shown = ShownIcon::Theme(playing);
        Some(if playing {
            icon::playing_icon()
        } else {
            icon::idle_icon()
        })
    }
}

/// Run the tray application: build it and enter the tick loop.
pub fn run() -> anyhow::Result<()> {
    let lock = match acquire_tray_lock() {
        Some(f) => f,
        None => {
            eprintln!("radiopittray: already running");
            return Ok(());
        }
    };

    let conf = config::Config::load();
    let shared = Arc::new(Mutex::new(Shared::new(conf)));

    // GTK (and with it the GLib main context) is used by the tray's menu on
    // Linux; it must be initialized on the main thread, and the context is
    // pumped from the tick loop.
    #[cfg(target_os = "linux")]
    if let Err(e) = gtk::init() {
        eprintln!("warning: gtk::init() failed: {e}");
    }

    TrayApp::new(shared)?.run();
    // Unreachable: the tick loop runs until the process exits (which
    // releases the lock).
    drop(lock);
    Ok(())
}

/// Single-instance guard: two tray processes would register the same
/// StatusNotifierItem id and show duplicate tray icons.
fn acquire_tray_lock() -> Option<File> {
    let dir = dirs::config_dir()?;
    let path = dir.join("radiopittray").join("tray.lock");
    let _ = std::fs::create_dir_all(path.parent().unwrap_or(&dir));
    let file = std::fs::OpenOptions::new().create(true).write(true).open(&path).ok()?;
    let ok = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if ok == 0 {
        Some(file)
    } else {
        None
    }
}

/// Decode an image and resize it to 32×32 RGBA.
fn resize_rgba32(img: &image::DynamicImage) -> Vec<u8> {
    img.resize(32, 32, image::imageops::FilterType::Lanczos3)
        .to_rgba8()
        .as_raw()
        .to_vec()
}

/// Fetch an icon URL without the API key header.
async fn fetch_icon_bytes(url: &str) -> anyhow::Result<Vec<u8>> {
    let url = fetch_icon_url(url);
    let client = reqwest::Client::builder()
        .user_agent(concat!("radiopittray/", env!("CARGO_PKG_VERSION")))
        .timeout(std::time::Duration::from_secs(10))
        .build()?;
    let resp = client.get(&url).send().await?.error_for_status()?;
    Ok(resp.bytes().await?.to_vec())
}

/// Wikimedia only serves thumbnails in a fixed set of sizes (640px is not
/// one of them — it answers 400) — rewrite the requested size to 120px,
/// which is allowed and still well above the 32×32 tray icon.
fn fetch_icon_url(url: &str) -> String {
    let (prefix, rest) = match url.rsplit_once('/') {
        Some((p, r)) => (p, r),
        None => return url.to_string(),
    };
    let (size, file) = match rest.split_once("px-") {
        Some((s, f)) if s.parse::<u32>().is_ok() => (s, f),
        _ => return url.to_string(),
    };
    if size == "120" || !prefix.contains("wikimedia.org") || !prefix.contains("/thumb/") {
        return url.to_string();
    }
    format!("{prefix}/120px-{file}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrites_wikimedia_thumbnail_size() {
        let url = "https://upload.wikimedia.org/wikipedia/commons/thumb/d/da/NPO_Radio_1_logo_2014.svg/640px-NPO_Radio_1_logo_2014.svg.png";
        assert_eq!(
            fetch_icon_url(url),
            "https://upload.wikimedia.org/wikipedia/commons/thumb/d/da/NPO_Radio_1_logo_2014.svg/120px-NPO_Radio_1_logo_2014.svg.png"
        );
    }

    #[test]
    fn leaves_other_urls_alone() {
        let url = "https://example.com/icon/640px-foo.png";
        assert_eq!(fetch_icon_url(url), url);
        let url = "https://upload.wikimedia.org/wikipedia/commons/foo.png";
        assert_eq!(fetch_icon_url(url), url);
    }
}

fn open_url(url: &str) {
    let url = url.to_string();
    std::thread::spawn(move || {
        if let Err(e) = open::that_detached(&url) {
            tracing::warn!("opening {url}: {e}");
        }
    });
}
