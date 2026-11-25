//! The tray application.
//!
//! Two cooperating halves live in this process:
//!
//! - **The eframe loop** (main thread): owns the now-playing window (see
//!   `ui.rs`). It starts hidden and is shown/hidden by a left click on the
//!   tray icon. Closing the window does not stop the app: the loop simply
//!   re-creates a (hidden) window so the next click can show it again.
//! - **The tick thread** (`run_tick`): the tray icon (StatusNotifierItem
//!   via the ksni backend), GStreamer events, the API/ICY/icon tasks on a
//!   tokio runtime, the config watch, history persistence and the album-art
//!   fetch pipeline. It talks to the eframe loop through `WindowCtl`.
//!
//! Left click on the tray icon shows/hides the window; right click shows
//! the (minimal) context menu (`menu.rs`). The two halves share state
//! through `Arc<Mutex<Shared>>`.
//!
//! The config window is still a separate process (`radiopittray --config`,
//! see configui.rs); this process notices its result by watching config.toml.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::os::unix::io::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use tray_icon::menu::MenuEvent;
use tray_icon::{TrayIconBuilder, TrayIconEvent};

use crate::api::{self, Client, Station};
use crate::artwork;
use crate::config;
use crate::configui;
use crate::history;
use crate::icy::{self, IcyCmd};
use crate::icon;
use crate::menu;
use crate::player::{self, Cmd, Ev};
use crate::remote::{self, Action, RemoteInCmd, RemoteOut};
use crate::state::{Playback, Shared};
use crate::ui::MainWindow;
use crate::urls;

/// Artwork pipeline: how many fetches may be in flight at once, and how
/// deep into the history we fill art.
const ART_CONCURRENCY: usize = 4;
const ART_FILL_DEPTH: usize = 40;
/// Sizes we cache album art at (now-playing and table).
const ART_SIZE: u32 = 192;
const THUMB_SIZE: u32 = 48;

/// What the tray icon currently shows.
#[derive(Clone, PartialEq, Debug)]
enum ShownIcon {
    /// The procedural RadioPit logo; bool = playing (green dot).
    Logo(bool),
    /// The icon of the station with this id.
    Station(String),
}

/// A request from the tray window (`ui.rs`) to the tick thread. The window
/// has no access to the player, the API client or the remote WebSocket, so
/// it pushes these and `TrayApp::handle_window_controls` performs them.
#[derive(Clone)]
pub(crate) enum WindowReq {
    /// Play the station with this id (or steer the controlled remote to it).
    PlayStation(String),
    /// Select the controlled device; `None` = local playback.
    SetRemote(Option<String>),
    /// Play/pause the controlled remote.
    RemotePlayPause,
    /// Volume step (±10) for the controlled remote.
    RemoteVolume(i32),
    /// Re-fetch the library.
    Refresh,
    /// Open radiopit.fm in the browser.
    OpenWeb,
}

/// Handshake between the tick thread and the (possibly absent) eframe
/// loop: `ctx` is `Some` only while a window loop is running. The window
/// module (`ui.rs`) reads these flags to place and hide itself.
pub(crate) struct WindowCtl {
    ctx: Mutex<Option<egui::Context>>,
    /// Whether the user wants the window shown (survives the loop being
    /// recreated after the window is closed).
    pub(crate) want_visible: AtomicBool,
    /// *Physical* screen-pixel top-left (as the tray reports it) to place the
    /// window under the icon. `None` = let the window manager choose.
    pos: Mutex<Option<[f32; 2]>>,
    /// The window should (re)position itself at `pos` on its next visible
    /// frame. Placement happens in the eframe loop, where the viewport is
    /// real and the scale factor is known; doing it while hidden is ignored
    /// and the window manager centres the window instead.
    pub(crate) needs_place: AtomicBool,
    /// Whether the window has had keyboard focus since it was last shown.
    /// Focus-loss dismisses the window (see `ui.rs`), but only after it has
    /// actually been focused once — otherwise the popup would hide itself in
    /// the frames right after showing, before the window manager hands over
    /// focus. Reset to `false` every time the window is shown.
    pub(crate) focused_seen: AtomicBool,
    /// Play/stop requests raised by the window's buttons, picked up by the
    /// tick thread (the window has no direct access to the player channel).
    pub(crate) want_play: AtomicBool,
    pub(crate) want_stop: AtomicBool,
    /// Requests raised by the window's widgets, drained by
    /// `TrayApp::handle_window_controls`.
    requests: Mutex<Vec<WindowReq>>,
}

impl WindowCtl {
    fn new() -> Self {
        Self {
            ctx: Mutex::new(None),
            want_visible: AtomicBool::new(false),
            pos: Mutex::new(None),
            needs_place: AtomicBool::new(false),
            focused_seen: AtomicBool::new(false),
            want_play: AtomicBool::new(false),
            want_stop: AtomicBool::new(false),
            requests: Mutex::new(Vec::new()),
        }
    }

    /// Push a request from the window to the tick thread.
    pub(crate) fn request(&self, req: WindowReq) {
        self.requests.lock().unwrap().push(req);
    }

    /// Take all pending requests (FIFO).
    pub(crate) fn take_requests(&self) -> Vec<WindowReq> {
        std::mem::take(&mut *self.requests.lock().unwrap())
    }

    /// Toggle window visibility (left click on the tray icon). `phys` is the
    /// click position in *physical* screen pixels; on showing, the window is
    /// flagged to move itself just under the icon on its first visible frame.
    fn toggle(&self, phys: [f32; 2]) {
        let next = !self.want_visible.load(Ordering::Relaxed);
        self.want_visible.store(next, Ordering::Relaxed);
        if next {
            *self.pos.lock().unwrap() = Some(phys);
            self.needs_place.store(true, Ordering::Relaxed);
            // A fresh show has not been focused yet; without clearing this the
            // stale `true` from the previous session would make the focus-loss
            // handler hide the window again on its very first frame.
            self.focused_seen.store(false, Ordering::Relaxed);
        }
        if let Some(ctx) = self.ctx.lock().unwrap().as_ref() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(next));
            if next {
                // Take focus on show so that focus-loss (click outside) can
                // dismiss the popup; an undecorated popup is not always
                // focused automatically by the window manager.
                ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            }
            ctx.request_repaint();
        }
    }

    /// Show the window without toggling (the menu's "Open main window").
    /// There is no click position to place against, so the window manager
    /// decides where it lands.
    fn show(&self) {
        if self.want_visible.swap(true, Ordering::Relaxed) {
            return;
        }
        self.focused_seen.store(false, Ordering::Relaxed);
        if let Some(ctx) = self.ctx.lock().unwrap().as_ref() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            ctx.request_repaint();
        }
    }

    /// The stored click position, if any.
    pub(crate) fn pos(&self) -> Option<[f32; 2]> {
        *self.pos.lock().unwrap()
    }

    /// Ask a running window to repaint (state changed). Not gated on
    /// `want_visible`: a repaint on a hidden window is harmless (nothing is
    /// drawn) and event-driven, so gating it only risks swallowing the repaint
    /// that reveals newly arrived data if the visible flag ever desyncs from
    /// the real window state.
    fn wake(&self) {
        if let Some(ctx) = self.ctx.lock().unwrap().as_ref() {
            ctx.request_repaint();
        }
    }
}

struct TrayApp {
    shared: Arc<Mutex<Shared>>,
    ctl: Arc<WindowCtl>,
    tray: tray_icon::TrayIcon,
    cmd_tx: Sender<Cmd>,
    ev_rx: Receiver<Ev>,
    icy: icy::IcyHandle,
    /// Remote control ("Playing on…"): the account's WebSocket.
    remote: remote::RemoteHandle,
    handle: tokio::runtime::Handle,
    /// Decoded station icons for the tray, keyed by station id (32×32 RGBA).
    tray_icon_cache: HashMap<String, Vec<u8>>,
    /// What the tray icon currently shows.
    tray_shown: ShownIcon,
    /// mtime of config.toml as last seen; a change means the config window
    /// (or an editor) saved something.
    config_mtime: Option<SystemTime>,
    /// Album-art pipeline bookkeeping: fetch keys currently in flight and
    /// those already tried once this session.
    art_inflight: HashSet<String>,
    art_tried: HashSet<String>,
    art_done_tx: Sender<String>,
    art_done_rx: Receiver<String>,
    /// When history was last written to disk (to throttle saves).
    last_history_save: Option<SystemTime>,
    /// The stream URI the ICY metadata task is currently tracking (the
    /// direct stream, or the proxy fallback once the player switches to
    /// it). Compared against the player's `Ev::Playing` URI so the
    /// tracker is only restarted when the URI actually changes.
    icy_url: Option<String>,
}

impl TrayApp {
    /// Build everything and return a ready-to-run instance. Must be called
    /// on the tick thread (the tray icon lives here and is not `Send`).
    fn new(shared: Arc<Mutex<Shared>>, ctl: Arc<WindowCtl>, handle: tokio::runtime::Handle) -> anyhow::Result<Self> {
        let (cmd_tx, cmd_rx) = mpsc::channel::<Cmd>();
        let (ev_tx, ev_rx) = mpsc::channel::<Ev>();
        player::spawn(cmd_rx, ev_tx);

        // Apply the stored volume to the new player.
        let (volume, muted) = {
            let s = shared.lock().unwrap();
            (s.volume, s.muted)
        };
        let _ = cmd_tx.send(Cmd::SetVolume(volume));
        let _ = cmd_tx.send(Cmd::SetMute(muted));

        let icy = icy::IcyHandle::spawn(handle.clone(), shared.clone());
        let remote = remote::RemoteHandle::spawn(handle.clone(), shared.clone());

        let tray_menu = menu::build_menu();
        let tray = TrayIconBuilder::new()
            .with_id("radiopittray")
            .with_menu(Box::new(tray_menu))
            .with_icon(icon::idle_icon())
            .with_tooltip("RadioPit")
            .build()?;

        let config_mtime = std::fs::metadata(config::config_path())
            .ok()
            .and_then(|m| m.modified().ok());
        let (art_done_tx, art_done_rx) = mpsc::channel::<String>();

        Ok(Self {
            shared,
            ctl,
            tray,
            cmd_tx,
            ev_rx,
            icy,
            remote,
            handle,
            tray_icon_cache: HashMap::new(),
            tray_shown: ShownIcon::Logo(false),
            config_mtime,
            art_inflight: HashSet::new(),
            art_tried: HashSet::new(),
            art_done_tx,
            art_done_rx,
            icy_url: None,
            last_history_save: None,
        })
    }

    /// The tick loop: a 100 ms heartbeat.
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
            self.drain_tray_events();
            self.drain_remote_commands();
            self.watch_config();
            self.sync_tray();
            self.handle_window_controls();
            self.artwork_tick();
            self.save_history_if_dirty();
            self.wake_window_if_dirty();

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
                    s.ui_dirty = true;
                }
                Err(e) => {
                    tracing::warn!("fetching library: {e:#}");
                    let mut s = shared.lock().unwrap();
                    s.library_loading = false;
                    s.library_error = Some(e.to_string());
                    s.ui_dirty = true;
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
        s.ui_dirty = true;
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
                    // not linger in the now-playing panel.
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
            s.ui_dirty = true;
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

    /// Tray icon clicks: a left click (Activate) shows/hides the window.
    /// Right clicks are handled by the StatusNotifier host (the menu) and
    /// never reach us.
    fn drain_tray_events(&mut self) {
        while let Ok(ev) = TrayIconEvent::receiver().try_recv() {
            if let TrayIconEvent::Click {
                button: tray_icon::MouseButton::Left,
                button_state: tray_icon::MouseButtonState::Up,
                position,
                ..
            } = ev
            {
                self.ctl.toggle([position.x as f32, position.y as f32]);
            }
        }
    }

    fn handle_menu_action(&mut self, id: &str) {
        match id {
            menu::OPEN_MAIN => self.ctl.show(),
            menu::CONFIG => configui::spawn_config_window(),
            menu::QUIT => {
                let _ = self.cmd_tx.send(Cmd::Stop);
                std::process::exit(0);
            }
            _ => {}
        }
    }

    /// The device the tray is currently controlling, if any.
    fn remote_target(&self) -> Option<String> {
        self.shared.lock().unwrap().remote_target.clone()
    }

    /// Switch which device the tray is controlling (`None` = local).
    fn select_remote(&mut self, target: Option<String>) {
        let mut s = self.shared.lock().unwrap();
        if s.remote_target != target {
            s.remote_target = target;
            s.ui_dirty = true;
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

    /// Commands other devices of the account routed to this tray. They run
    /// through the same code path as a menu click.
    fn drain_remote_commands(&mut self) {
        while let Some(cmd) = self.remote.try_recv_in() {
            match cmd {
                RemoteInCmd::Play => {
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
                    let _ = self.cmd_tx.send(Cmd::SetVolume(v as f64 / 100.0));
                    let mut s = self.shared.lock().unwrap();
                    s.volume = v as f64 / 100.0;
                    s.muted = v == 0;
                    s.ui_dirty = true;
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
                        None => tracing::warn!("remote: unknown station {station_id:?}"),
                    }
                }
            }
        }
    }

    /// Pick up changes saved to config.toml.
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
                s.ui_dirty = true;
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
            self.remote.send(RemoteOut::Reconnect);
        }
        if conf.volume != old_file_volume {
            let _ = self.cmd_tx.send(Cmd::SetVolume(conf.volume));
        }
    }

    /// Update the tray icon/tooltip when the state changed. The menu is
    /// static (see `menu.rs`) and built once in `new`.
    fn sync_tray(&mut self) {
        let (tooltip, want, playing) = {
            let s = self.shared.lock().unwrap();
            // The icon and tooltip always follow this tray's own player.
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

    /// Try to get (or start fetching) the icon for a station.
    fn try_station_icon(&mut self, sid: &str, url: Option<&str>) -> Option<tray_icon::Icon> {
        let url = url?;

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
                    let mut s = self.shared.lock().unwrap();
                    s.icon_failed_for = Some(url.to_string());
                }
            }
            return None;
        }

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

    /// Persist the history when it changed (throttled).
    fn save_history_if_dirty(&mut self) {
        let due = self
            .last_history_save
            .map(|t| t.elapsed().unwrap_or(Duration::ZERO) >= Duration::from_secs(2))
            .unwrap_or(true);
        if !due {
            return;
        }
        let (dirty, snapshot) = {
            let mut s = self.shared.lock().unwrap();
            if !s.history_dirty {
                (false, Vec::new())
            } else {
                s.history_dirty = false;
                (true, s.history.clone())
            }
        };
        if dirty {
            if let Err(e) = history::save(&snapshot) {
                tracing::warn!("saving history: {e:#}");
            }
            self.last_history_save = Some(SystemTime::now());
        }
    }

    /// Ask the window to repaint when the shared state changed.
    fn wake_window_if_dirty(&mut self) {
        let dirty = {
            let mut s = self.shared.lock().unwrap();
            std::mem::take(&mut s.ui_dirty)
        };
        if dirty {
            self.ctl.wake();
        }
    }

    /// Pick up the requests raised by the window's widgets and perform the
    /// same actions the (now minimal) menu used to do: start/stop this
    /// player, play a station (locally or on the controlled remote), steer
    /// the remote, refresh the library. The window itself has no access to
    /// the player command channel, the API or the remote WebSocket, so it
    /// pushes `WindowReq`s and they are executed here.
    fn handle_window_controls(&mut self) {
        if self.ctl.want_stop.swap(false, Ordering::Relaxed) {
            self.stop();
        } else if self.ctl.want_play.swap(false, Ordering::Relaxed) {
            let station = {
                let s = self.shared.lock().unwrap();
                if s.playback == Playback::Stopped {
                    s.current.clone()
                } else {
                    None
                }
            };
            if let Some(station) = station {
                self.play_station(station, false);
            }
        }
        for req in self.ctl.take_requests() {
            match req {
                WindowReq::PlayStation(id) => self.play_station_id(id),
                WindowReq::SetRemote(target) => self.select_remote(target),
                WindowReq::RemotePlayPause => self.remote_play_pause(),
                WindowReq::RemoteVolume(delta) => self.remote_volume(delta),
                WindowReq::Refresh => self.refresh_library(),
                WindowReq::OpenWeb => open_url("https://radiopit.fm"),
            }
        }
    }

    /// Play a station by id, or — while a remote is being controlled —
    /// tell that device to switch instead.
    fn play_station_id(&mut self, station_id: String) {
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

    /// Play/pause the controlled remote, depending on its last state.
    fn remote_play_pause(&mut self) {
        let Some(target) = self.remote_target() else {
            return;
        };
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
    }

    /// Step the controlled remote's volume by `delta` (±10, like the web
    /// app's keys).
    fn remote_volume(&mut self, delta: i32) {
        let Some(target) = self.remote_target() else {
            return;
        };
        let current = {
            let s = self.shared.lock().unwrap();
            // Fall back to this tray's volume while the remote
            // has not reported one yet.
            s.remote_target_state()
                .map(|st| st.volume)
                .unwrap_or_else(|| (s.volume * 100.0).round() as u32)
        };
        let v = (current as i32 + delta).clamp(0, 100) as u32;
        self.send_remote_command(&target, Action::SetVolume, Some(v), None);
    }

    /// Drive the artwork/thumbnail fetch pipeline: history album art and
    /// the station icons for the station list. Only runs while the window
    /// is open, so there is no background network traffic when nobody
    /// looks.
    fn artwork_tick(&mut self) {
        // Reap finished fetches so they can be retried if needed.
        while let Ok(key) = self.art_done_rx.try_recv() {
            self.art_inflight.remove(&key);
        }

        if !self.ctl.want_visible.load(Ordering::Relaxed) {
            return;
        }
        if self.art_inflight.len() >= ART_CONCURRENCY {
            return;
        }

        // Collect everything that still needs a thumbnail: history tracks
        // first (now-playing art matters most), then station icons, all
        // without holding the lock across spawns.
        let jobs: Vec<ArtJob> = {
            let s = self.shared.lock().unwrap();
            let mut out = Vec::new();
            for e in s.history.iter().take(ART_FILL_DEPTH) {
                if let Some(url) = &e.artwork_url {
                    // Artwork known: make sure the table thumb is cached.
                    if !artwork::thumb_known(url, THUMB_SIZE) {
                        out.push(ArtJob::Track {
                            title: e.title.clone(),
                            artist: e.artist.clone(),
                            existing: Some(url.clone()),
                        });
                    }
                } else {
                    out.push(ArtJob::Track {
                        title: e.title.clone(),
                        artist: e.artist.clone(),
                        existing: None,
                    });
                }
            }
            // The now-playing station's icon at the large size: the
            // now-playing pane falls back to it when the track has no
            // album art, so it must be cached at ART_SIZE, not just
            // THUMB_SIZE.
            if let Some(cur) = s.current.as_ref() {
                if let Some(url) = cur.icon_url.as_deref() {
                    if !artwork::thumb_known(url, ART_SIZE) {
                        out.push(ArtJob::Icon {
                            url: url.to_string(),
                            size: ART_SIZE,
                        });
                    }
                }
            }
            if let Some(lib) = s.library.as_ref() {
                for st in lib.all_stations() {
                    if let Some(url) = st.icon_url.as_deref() {
                        if !artwork::thumb_known(url, THUMB_SIZE) {
                            out.push(ArtJob::Icon {
                                url: url.to_string(),
                                size: THUMB_SIZE,
                            });
                        }
                    }
                }
            }
            out
        };

        let done_tx = self.art_done_tx.clone();
        for job in jobs {
            if self.art_inflight.len() >= ART_CONCURRENCY {
                break;
            }
            let key = job.key();
            if self.art_inflight.contains(&key) || self.art_tried.contains(&key) {
                continue;
            }
            self.art_inflight.insert(key.clone());
            self.art_tried.insert(key.clone());

            let shared = self.shared.clone();
            let done = done_tx.clone();
            self.handle.spawn(async move {
                match job {
                    ArtJob::Track {
                        title,
                        artist,
                        existing,
                    } => {
                        let url = match existing {
                            Some(u) => Some(u),
                            None => artwork::artwork_url(&key).await,
                        };
                        if let Some(url) = &url {
                            let mut s = shared.lock().unwrap();
                            s.set_latest_artwork(&title, artist.as_deref(), url.clone());
                        }
                        // Populate both sizes the UI needs *before* waking
                        // the window. Setting `ui_dirty` right after the URL
                        // resolved would repaint while both thumbnails are
                        // still uncached, so the art only appeared on some
                        // *later* repaint (e.g. a close/reopen, when the
                        // cache was already warm). Waking only once the
                        // thumbnails are in the cache makes them show up on
                        // the first open.
                        if let Some(url) = url.as_deref() {
                            artwork::thumb(url, THUMB_SIZE).await;
                            artwork::thumb(url, ART_SIZE).await;
                        }
                    }
                    ArtJob::Icon { url, size } => {
                        artwork::thumb(&url, size).await;
                    }
                }
                let mut s = shared.lock().unwrap();
                s.ui_dirty = true;
                let _ = done.send(key);
            });
        }
    }
}

/// One artwork/thumbnail fetch request.
enum ArtJob {
    /// A history track: resolve its artwork when unknown, then cache both
    /// sizes the UI needs.
    Track {
        title: String,
        artist: Option<String>,
        /// Known artwork URL, or `None` to look it up by title/artist.
        existing: Option<String>,
    },
    /// A station icon: cache a thumbnail at `size` (the station list uses
    /// `THUMB_SIZE`, the now-playing pane `ART_SIZE`).
    Icon { url: String, size: u32 },
}

impl ArtJob {
    /// Dedup key: the artwork URL when known, otherwise the search query.
    fn key(&self) -> String {
        match self {
            ArtJob::Track {
                existing: Some(u),
                ..
            } => u.clone(),
            ArtJob::Track {
                existing: None,
                title,
                artist,
            } => artwork::art_query(artist.as_deref(), title),
            ArtJob::Icon { url, size } => format!("icon:{url}#{size}"),
        }
    }
}

/// Open a URL in the user's browser.
fn open_url(url: &str) {
    let url = url.to_string();
    std::thread::spawn(move || {
        if let Err(e) = open::that_detached(&url) {
            tracing::warn!("opening {url}: {e}");
        }
    });
}

/// Single-instance guard: two tray processes would register the same
/// StatusNotifierItem id and show duplicate tray icons.
fn acquire_tray_lock() -> Option<File> {
    let dir = config::ensure_config_dir();
    let path = dir.join("tray.lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .ok()?;
    let ok = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if ok == 0 {
        Some(file)
    } else {
        None
    }
}

/// Run the tray application: build it and enter the tick loop.
pub fn run() -> anyhow::Result<()> {
    let _lock = match acquire_tray_lock() {
        Some(f) => f,
        None => {
            eprintln!("radiopittray: already running");
            return Ok(());
        }
    };

    let mut conf = config::Config::load();
    if conf.device_id.is_none() {
        conf.device_id = Some(uuid::Uuid::new_v4().to_string());
        if let Err(e) = conf.save() {
            tracing::warn!("saving config: {e:#}");
        }
    }
    // Load the persisted history up front.
    let shared = Arc::new(Mutex::new(Shared::new(conf)));
    {
        let mut s = shared.lock().unwrap();
        s.history = history::load();
    }

    let handle = {
        // Owns the tokio runtime; kept alive for the whole process.
        let rt = std::mem::ManuallyDrop::new(
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("building tokio runtime"),
        );
        rt.handle().clone()
    };

    let ctl = Arc::new(WindowCtl::new());

    // The tick thread owns the tray icon (which is not `Send`) and drives
    // all tray-side logic.
    {
        let shared = shared.clone();
        let ctl = ctl.clone();
        let handle = handle.clone();
        std::thread::spawn(move || {
            let app = TrayApp::new(shared, ctl, handle);
            match app {
                Ok(app) => app.run(),
                Err(e) => {
                    tracing::error!("tray: {e:#}");
                    std::process::exit(1);
                }
            }
        });
    }

    // Force this window onto X11/XWayland. On a native Wayland session winit
    // forbids a client from positioning its own window or pinning it above
    // others, so "open under the tray icon, always on top" is impossible
    // there (KWin just centres it). Under XWayland those requests work. We
    // make this trade-off deliberately for this one window: the tray icon is
    // a D-Bus StatusNotifierItem and the audio player are both unaffected by
    // the window backend, so only this popup runs under XWayland.
    //
    // Must be set before winit picks its backend (inside `run_native`).
    std::env::set_var("XDG_SESSION_TYPE", "x11");
    std::env::remove_var("WAYLAND_DISPLAY");

    // The main thread is the eframe loop. It starts hidden; a left click on
    // the tray icon shows it and the close button hides it again (handled in
    // `ui.rs`), so the loop normally runs for the life of the process. It is
    // only restarted if the window backend itself fails. `lock` is dropped
    // only when the process exits.
    loop {
        let shared2 = shared.clone();
        let ctl2 = ctl.clone();
        let native_options = eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default()
                .with_inner_size(egui::vec2(520.0, 600.0))
                .with_min_inner_size(egui::vec2(420.0, 400.0))
                .with_title("RadioPit")
                // Start hidden; only a left click on the tray icon shows it.
                // Always on top so it surfaces over the other windows.
                .with_visible(false)
                .with_decorations(false)
                .with_always_on_top()
                .with_icon(Arc::new(icon::egui_icon(false))),
            ..Default::default()
        };
        let result = eframe::run_native(
            "radiopittray",
            native_options,
            Box::new(move |cc| {
                // Publish the context so the tick thread can toggle the
                // window, and honour a pending show request.
                let want = ctl2.want_visible.load(Ordering::Relaxed);
                *ctl2.ctx.lock().unwrap() = Some(cc.egui_ctx.clone());
                if want {
                    // The window is being shown again (it was hidden, not
                    // destroyed): ask it to re-place itself under the icon on
                    // its first visible frame.
                    ctl2.needs_place.store(true, Ordering::Relaxed);
                    ctl2.focused_seen.store(false, Ordering::Relaxed);
                    cc.egui_ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                    cc.egui_ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                } else {
                    // Enforce hidden. `with_visible(false)` is not always
                    // honoured on XWayland (winit can map the window before
                    // the visibility attribute applies), which left the window
                    // on screen while `want_visible` was still false. That
                    // desync suppressed the repaint that reveals freshly
                    // fetched artwork (see `WindowCtl::wake`), so art only
                    // appeared after a later click. Send the hide command
                    // explicitly so the WM and our flag agree from frame one.
                    cc.egui_ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
                }
                Ok(Box::new(MainWindow::new(shared2.clone(), ctl2.clone())))
            }),
        );
        // The window loop ended (the user closed it): clear the context so
        // the tick thread stops poking a dead window, then loop around to a
        // fresh hidden one.
        *ctl.ctx.lock().unwrap() = None;
        ctl.want_visible.store(false, Ordering::Relaxed);
        match result {
            Ok(()) => continue,
            Err(e) => {
                tracing::warn!("eframe: {e:?}");
                // A window backend error is not fatal for the tray; retry
                // after a beat so we do not spin.
                std::thread::sleep(Duration::from_millis(500));
                continue;
            }
        }
    }
}
