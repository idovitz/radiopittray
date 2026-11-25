//! The tray application.
//!
//! No GUI window exists in this process at all: the tray icon
//! (StatusNotifierItem via the ksni backend) carries the menu and the
//! now-playing info, GStreamer runs in its own thread, and the
//! API/ICY/icon/remote-control tasks live on a tokio runtime. The
//! main thread is a plain tick loop: drain menu, player and remote
//! control events, watch the config file, and sync the tray.
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

use crate::api::{self, Client, Station};
use crate::config;
use crate::configui;
use crate::icy::{self, IcyCmd};
use crate::icon;
use crate::menu;
use crate::player::{self, Cmd, Ev};
use crate::remote::{self, Action, RemoteInCmd, RemoteOut};
use crate::state::{Playback, Shared};
use crate::urls;

/// What the tray icon currently shows.
#[derive(Clone, PartialEq, Debug)]
enum ShownIcon {
    /// The procedural RadioPit logo; bool = playing (green dot).
    Logo(bool),
    /// The icon of the station with this id.
    Station(String),
}

struct TrayApp {
    shared: Arc<Mutex<Shared>>,
    tray: tray_icon::TrayIcon,
    cmd_tx: Sender<Cmd>,
    ev_rx: Receiver<Ev>,
    icy: icy::IcyHandle,
    /// Remote control ("Playing on…"): the account's WebSocket.
    remote: remote::RemoteHandle,
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
    /// The stream URI the ICY metadata task is currently tracking (the
    /// direct stream, or the proxy fallback once the player switches to
    /// it). Compared against the player's `Ev::Playing` URI so the
    /// tracker is only restarted when the URI actually changes.
    icy_url: Option<String>,
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
        let remote = remote::RemoteHandle::spawn(handle.clone(), shared.clone());

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
            remote,
            runtime,
            handle,
            tray_icon_cache: HashMap::new(),
            tray_shown: ShownIcon::Logo(false),
            config_mtime,
            icy_url: None,
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
            self.drain_remote_commands();
            self.watch_config();
            self.sync_tray();

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
    fn play_station(&mut self, station: Station, from_remote: bool) {
        // A station URL points at bytes the app downloads and decodes
        // (playbin and the ICY connection). Refuse anything that is not
        // a plain public http(s) stream: no file://, no local or private
        // addresses (SSRF guard, see urls.rs).
        if let Err(e) = urls::validate(&station.url) {
            tracing::warn!("refusing to play {:?}: {e:#}", station.name);
            let mut s = self.shared.lock().unwrap();
            s.playback = Playback::Stopped;
            s.playback_error = Some(format!("stream URL refused: {e}"));
            s.menu_dirty = true;
            return;
        }
        // Fallback for when the direct stream is unreachable (e.g. blocked
        // by the local network): the RadioPit server proxies it, like the
        // web app does. The player switches to it automatically on retries.
        let api_url = self.shared.lock().unwrap().config.api_url.clone();
        let proxy_url = Some(api::proxy_stream_url(&api_url, &station.url));
        if let Err(e) = self.cmd_tx.send(Cmd::Play {
            station: station.clone(),
            proxy_url,
        }) {
            tracing::warn!("player thread gone: {e}");
            return;
        }
        let mut s = self.shared.lock().unwrap();
        s.current = Some(station.clone());
        s.playback = Playback::Loading;
        s.playback_error = None;
        s.icy_title = None;
        s.icy_artist = None;
        // A remote-initiated play does not raise a "new track" notification.
        s.remote_initiated = from_remote;
        s.config.last_station_id = Some(station.id.clone());
        if let Err(e) = s.config.save() {
            tracing::warn!("saving config: {e:#}");
        }
        s.menu_dirty = true;
        // Metadata tracking starts on the direct stream; the player's
        // `Ev::Playing` events redirect it to the proxy when that is where
        // the audio actually ends up playing.
        self.icy_url = Some(station.url.clone());
        self.icy.send(IcyCmd::Play(station.url));
    }

    fn stop(&mut self) {
        let _ = self.cmd_tx.send(Cmd::Stop);
        self.icy.send(IcyCmd::Stop);
        self.icy_url = None;
    }

    fn drain_player_events(&mut self) {
        let mut icy_uri: Option<String> = None;
        while let Ok(ev) = self.ev_rx.try_recv() {
            let mut s = self.shared.lock().unwrap();
            match ev {
                Ev::Playing(uri) => {
                    s.playback = Playback::Playing;
                    s.playback_error = None;
                    // Metadata tracking must follow the URI the player
                    // actually uses. A (re)start on the same URI must not
                    // restart the tracker — that would clear the current
                    // title.
                    if self.icy_url.as_deref() != Some(uri.as_str()) {
                        self.icy_url = Some(uri.clone());
                        icy_uri = Some(uri);
                    }
                }
                Ev::Stopped => {
                    s.playback = Playback::Stopped;
                    // The stream is gone; its metadata is stale and must
                    // not linger in the menu or notifications.
                    s.icy_title = None;
                    s.icy_artist = None;
                }
                Ev::Failed(msg) => {
                    s.playback = Playback::Stopped;
                    s.playback_error = Some(msg);
                    s.icy_title = None;
                    s.icy_artist = None;
                    // The stream gave up; the metadata connection is dead too.
                    self.icy.send(IcyCmd::Stop);
                }
            }
            s.menu_dirty = true;
        }
        if let Some(uri) = icy_uri {
            self.icy.send(IcyCmd::Play(uri));
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
            PLAY_STOP => {
                // Controlling a remote: play/stop applies to the remote.
                if let Some(target) = self.remote_target() {
                    let active = matches!(
                        self.shared
                            .lock()
                            .unwrap()
                            .remote_target_state()
                            .map(|st| st.state.as_str()),
                        Some("playing") | Some("connecting")
                    );
                    let action = if active {
                        Action::Pause
                    } else {
                        Action::Play
                    };
                    self.send_remote_command(&target, action, None, None);
                } else {
                    match {
                        let s = self.shared.lock().unwrap();
                        s.playback
                    } {
                        Playback::Stopped => {
                            let station = {
                                let s = self.shared.lock().unwrap();
                                s.current.clone()
                            };
                            if let Some(st) = station {
                                self.play_station(st, false);
                            }
                        }
                        // Loading or playing: the line reads "Stop stream [X]".
                        _ => self.stop(),
                    }
                }
            }
            REFRESH => self.refresh_library(),
            WEB => open_url("https://radiopit.fm"),
            CONFIG => configui::spawn_config_window(),
            QUIT => {
                let _ = self.cmd_tx.send(Cmd::Stop);
                std::process::exit(0);
            }
            REMOTE_LOCAL => self.select_remote(None),
            id if id.starts_with("remote:") => {
                let device_id = id.strip_prefix("remote:").unwrap().to_string();
                self.select_remote(Some(device_id));
            }
            VOL_UP | VOL_DOWN => {
                let Some(target) = self.remote_target() else {
                    return;
                };
                let (current, delta) = {
                    let s = self.shared.lock().unwrap();
                    // Fall back to this tray's volume while the remote
                    // has not reported one yet.
                    let current = s
                        .remote_target_state()
                        .map(|st| st.volume)
                        .unwrap_or_else(|| (s.volume * 100.0).round() as u32);
                    (current, if id == VOL_UP { 10 } else { -10 })
                };
                let v = (current as i32 + delta).clamp(0, 100) as u32;
                self.send_remote_command(&target, Action::SetVolume, Some(v), None);
            }
            id if id.starts_with("st:") => {
                let station_id = id.strip_prefix("st:").unwrap().to_string();
                // Controlling a remote: the station switch happens there.
                if let Some(target) = self.remote_target() {
                    self.send_remote_command(&target, Action::SetStation, None, Some(station_id));
                    return;
                }
                let station = {
                    let s = self.shared.lock().unwrap();
                    s.library
                        .as_ref()
                        .and_then(|lib| lib.by_id(&station_id).cloned())
                };
                if let Some(st) = station {
                    self.play_station(st, false);
                }
            }
            _ => {}
        }
    }

    /// The device the tray is currently controlling, if any.
    fn remote_target(&self) -> Option<String> {
        self.shared.lock().unwrap().remote_target.clone()
    }

    /// Switch which device the tray is controlling (`None` = local).
    /// The selection is plain shared state; the server does not need to
    /// know about it.
    fn select_remote(&mut self, target: Option<String>) {
        let mut s = self.shared.lock().unwrap();
        if s.remote_target != target {
            s.remote_target = target;
            s.menu_dirty = true;
        }
    }

    /// Send a control command to a device on the account.
    fn send_remote_command(
        &self,
        to: &str,
        action: Action,
        volume: Option<u32>,
        station_id: Option<String>,
    ) {
        self.remote.send(RemoteOut::Command {
            to: to.to_string(),
            action,
            volume,
            station_id,
        });
    }

    /// Commands other devices of the account routed to this tray
    /// (it is the player side of the "Playing on…" feature). They run
    /// through the same code path as a menu click.
    fn drain_remote_commands(&mut self) {
        while let Some(cmd) = self.remote.try_recv_in() {
            match cmd {
                RemoteInCmd::Play => {
                    // Like the web app: resume the current station, or
                    // start the first one of the library.
                    let station = {
                        let s = self.shared.lock().unwrap();
                        s.current.clone().or_else(|| {
                            s.library
                                .as_ref()
                                .and_then(|lib| {
                                    lib.all_stations()
                                        .min_by_key(|st| st.order)
                                        .cloned()
                                })
                        })
                    };
                    if let Some(st) = station {
                        self.play_station(st, true);
                    }
                }
                RemoteInCmd::Pause => self.stop(),
                RemoteInCmd::SetVolume(v) => {
                    // Session-only, like the web app: the owner's saved
                    // volume in config.toml is left untouched.
                    let _ = self.cmd_tx.send(Cmd::SetVolume(v as f64 / 100.0));
                    let mut s = self.shared.lock().unwrap();
                    s.volume = v as f64 / 100.0;
                    s.muted = v == 0;
                    s.menu_dirty = true;
                }
                RemoteInCmd::SetStation(station_id) => {
                    let station = {
                        let s = self.shared.lock().unwrap();
                        s.library
                            .as_ref()
                            .and_then(|lib| lib.by_id(&station_id).cloned())
                    };
                    match station {
                        Some(st) => self.play_station(st, true),
                        // The web app would flash an error state; the
                        // tray has no such surface, so it is logged.
                        None => tracing::warn!("remote: unknown station {station_id:?}"),
                    }
                }
            }
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
        let (old_key, old_url, old_file_volume, old_allow_remote) = {
            let s = self.shared.lock().unwrap();
            (
                s.config.api_key.clone(),
                s.config.api_url.clone(),
                // The volume the last-read file carried — not the
                // session volume, which a remote volume change may have
                // moved (and must not be clobbered on a reload).
                s.config.volume,
                s.config.allow_remote_control,
            )
        };
        let key_or_url_changed = conf.api_key != old_key || conf.api_url != old_url;
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
        if key_or_url_changed || conf.allow_remote_control != old_allow_remote {
            // Re-authenticate the remote-control session with the new
            // credentials (a cleared key ends the session), or re-register
            // the device as (non-)playable when the toggle flipped.
            self.remote.send(RemoteOut::Reconnect);
        }
        if conf.volume != old_file_volume {
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
            // The icon and tooltip always follow this tray's own player —
            // even while a remote is controlled (no icon change for remote
            // playback). The menu carries the remote's state instead.
            let tooltip = match &s.current {
                Some(st) => format!("RadioPit — {}", st.name),
                None => "RadioPit".to_string(),
            };
            let playing = s.playback == Playback::Playing;
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
            if let Err(e) = self.tray.set_icon(Some(ic)) {
                tracing::warn!("updating tray icon: {e:#}");
            }
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
            // 2) Fetch/decode this station's icon.
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
            match icon::load_icon_image(&b) {
                Some(img) => {
                    let rgba = icon::resize_square(&img, 32).into_raw();
                    if let Some(ic) = icon::tray_icon_from_rgba(rgba.clone()) {
                        self.tray_icon_cache.insert(sid.to_string(), rgba);
                        self.tray_shown = ShownIcon::Station(sid.to_string());
                        return Some(ic);
                    }
                }
                None => {
                    tracing::warn!("decoding station icon failed (bad format or too large)");
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
                match icon::fetch_icon_bytes(&url).await {
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

    /// Show the procedural RadioPit logo.
    fn apply_theme(&mut self, playing: bool) -> Option<tray_icon::Icon> {
        if self.tray_shown == ShownIcon::Logo(playing) {
            return None;
        }
        self.tray_shown = ShownIcon::Logo(playing);
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

    let mut conf = config::Config::load();
    // A stable identity for the "Playing on…" list (the web/Android
    // apps keep theirs in localStorage); persisted so a restart is
    // still recognized as the same player.
    if conf.device_id.is_none() {
        conf.device_id = Some(uuid::Uuid::new_v4().to_string());
        if let Err(e) = conf.save() {
            tracing::warn!("saving config: {e:#}");
        }
    }
    let shared = Arc::new(Mutex::new(Shared::new(conf)));

    TrayApp::new(shared)?.run();
    // Unreachable: the tick loop runs until the process exits (which
    // releases the lock).
    drop(lock);
    Ok(())
}

/// Single-instance guard: two tray processes would register the same
/// StatusNotifierItem id and show duplicate tray icons.
fn acquire_tray_lock() -> Option<File> {
    let dir = config::ensure_config_dir();
    let path = dir.join("tray.lock");
    let file = std::fs::OpenOptions::new().create(true).write(true).open(&path).ok()?;
    let ok = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if ok == 0 {
        Some(file)
    } else {
        None
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
