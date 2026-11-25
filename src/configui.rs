//! The config window: a standalone process, started with `--config`.
//!
//! It has a real window (the only window this program can open) and does
//! one thing: enter and validate the RadioPit key, then save it to
//! config.toml. The tray process is watching that file and reloads the
//! library as soon as the key changes. A flock makes sure only one config
//! window exists at a time.

use std::fs::File;
use std::os::unix::io::AsRawFd;
use std::sync::{Arc, Mutex};

use crate::api::Client;
use crate::config;
use crate::icon;

/// UI state of the config window (the eframe callback is 'static, so it
/// shares this through an Arc).
struct UiState {
    input: String,
    loading: bool,
    error: Option<String>,
    /// Whether "start at login" is enabled (mirrors the config).
    autostart: bool,
    /// Whether new tracks raise a desktop notification (mirrors the config).
    notify_new_track: bool,
    /// How long such a notification stays up, in seconds (mirrors the
    /// config).
    notify_duration_secs: u32,
    /// Whether notifications look up the track's album art (mirrors
    /// the config).
    notify_album_art: bool,
    /// Whether other devices may control this player (mirrors the config).
    allow_remote_control: bool,
    /// Set once the key has been saved; the window closes itself.
    saved: bool,
}

struct ConfigApp {
    state: Arc<Mutex<UiState>>,
    handle: tokio::runtime::Handle,
}

impl ConfigApp {
    fn new(state: Arc<Mutex<UiState>>, handle: tokio::runtime::Handle) -> Self {
        Self { state, handle }
    }
}

impl eframe::App for ConfigApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ctx, |ui| {
            draw_key_view(ui, &self.state, &self.handle, ctx);
        });

        // A key was just saved: close the window.
        if self.state.lock().unwrap().saved {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }
}

/// Validate the key and save it to the config file.
fn connect(
    state: Arc<Mutex<UiState>>,
    handle: &tokio::runtime::Handle,
    ctx: &egui::Context,
    key: String,
) {
    let (base, state, ctx) = {
        let base = config::Config::load().api_url;
        (base, state.clone(), ctx.clone())
    };
    let client = Client::new(&base);
    tracing::info!("connecting with key ({} chars)", key.len());
    handle.spawn(async move {
        {
            let mut s = state.lock().unwrap();
            s.loading = true;
            s.error = None;
        }
        if let Err(e) = client.validate(&key).await {
            let mut s = state.lock().unwrap();
            s.loading = false;
            s.error = Some(e.to_string());
            ctx.request_repaint();
            return;
        }
        let mut conf = config::Config::load();
        if conf.api_key == key {
            // Nothing to save; still close the window.
            state.lock().unwrap().saved = true;
            ctx.request_repaint();
            return;
        }
        conf.api_key = key;
        if let Err(e) = conf.save() {
            let mut s = state.lock().unwrap();
            s.loading = false;
            s.error = Some(format!("saving config: {e:#}"));
            ctx.request_repaint();
            return;
        }
        tracing::info!("key saved");
        state.lock().unwrap().saved = true;
        ctx.request_repaint();
    });
}

/// The key entry view.
fn draw_key_view(
    ui: &mut egui::Ui,
    state: &Arc<Mutex<UiState>>,
    handle: &tokio::runtime::Handle,
    ctx: &egui::Context,
) {
    let (error, loading) = {
        let s = state.lock().unwrap();
        (s.error.clone(), s.loading)
    };

    ui.add_space(14.0);
    ui.heading("RadioPit");
    ui.add_space(10.0);
    ui.label("Enter your RadioPit key to connect your library.");
    ui.label("Find it on radiopit.fm under  ⋮  →  Your Key.");
    ui.add_space(14.0);

    ui.label("Key");
    let resp = ui.add(
        egui::TextEdit::singleline(&mut state.lock().unwrap().input)
            .password(true)
            .desired_width(f32::INFINITY),
    );
    // A single-line TextEdit releases focus when Enter is pressed;
    // read the focus state *before* re-requesting it below, or the
    // lost_focus signal is masked.
    let enter_pressed = ui.input(|i| i.key_pressed(egui::Key::Enter));
    let focused = resp.has_focus();
    let just_submitted = resp.lost_focus();
    if !focused {
        resp.request_focus();
    }
    if (focused || just_submitted) && enter_pressed && !loading {
        let key = state.lock().unwrap().input.trim().to_string();
        if !key.is_empty() {
            connect(state.clone(), handle, ctx, key);
        }
    }

    if let Some(e) = &error {
        ui.add_space(4.0);
        ui.colored_label(egui::Color32::from_rgb(248, 113, 113), e.as_str());
    }

    ui.add_space(10.0);
    ui.horizontal(|ui| {
        let label = if loading { "Connecting…" } else { "Connect" };
        let resp = ui.add_enabled(!loading, egui::Button::new(label));
        if resp.clicked() {
            let key = state.lock().unwrap().input.trim().to_string();
            if !key.is_empty() {
                connect(state.clone(), handle, ctx, key);
            }
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.hyperlink("Open radiopit.fm").clicked() {
                open_url("https://radiopit.fm");
            }
        });
    });

    // -- Start at login ------------------------------------------------------
    let mut autostart = state.lock().unwrap().autostart;
    if ui.checkbox(&mut autostart, "Start at login").changed() {
        match config::set_autostart(autostart) {
            Ok(()) => {
                if let Err(e) = {
                    let mut conf = config::Config::load();
                    conf.autostart = autostart;
                    conf.save()
                } {
                    let mut s = state.lock().unwrap();
                    s.error = Some(format!("saving config: {e:#}"));
                } else {
                    state.lock().unwrap().autostart = autostart;
                }
            }
            Err(e) => {
                let mut s = state.lock().unwrap();
                s.error = Some(format!("autostart: {e:#}"));
            }
        }
        ctx.request_repaint();
    }

    // -- New track notifications ---------------------------------------------
    let mut notify = state.lock().unwrap().notify_new_track;
    if ui.checkbox(&mut notify, "Notify on new track").changed() {
        if let Err(e) = {
            let mut conf = config::Config::load();
            conf.notify_new_track = notify;
            conf.save()
        } {
            let mut s = state.lock().unwrap();
            s.error = Some(format!("saving config: {e:#}"));
        } else {
            state.lock().unwrap().notify_new_track = notify;
        }
        ctx.request_repaint();
    }
    let mut duration = state.lock().unwrap().notify_duration_secs;
    let duration_resp = ui.horizontal(|ui| {
        ui.label("Notification duration");
        let resp = ui.add(
            egui::DragValue::new(&mut duration)
                .range(0..=3600)
                .suffix(" s"),
        );
        ui.label("(0 = until dismissed)");
        resp
    });
    if duration_resp.inner.changed() {
        if let Err(e) = {
            let mut conf = config::Config::load();
            conf.notify_duration_secs = duration;
            conf.save()
        } {
            let mut s = state.lock().unwrap();
            s.error = Some(format!("saving config: {e:#}"));
        } else {
            state.lock().unwrap().notify_duration_secs = duration;
        }
        ctx.request_repaint();
    }
    let mut album_art = state.lock().unwrap().notify_album_art;
    if ui.checkbox(&mut album_art, "Use album art in notifications").changed() {
        if let Err(e) = {
            let mut conf = config::Config::load();
            conf.notify_album_art = album_art;
            conf.save()
        } {
            let mut s = state.lock().unwrap();
            s.error = Some(format!("saving config: {e:#}"));
        } else {
            state.lock().unwrap().notify_album_art = album_art;
        }
        ctx.request_repaint();
    }

    // -- Remote control -------------------------------------------------------
    ui.separator();
    ui.label("Remote control via radiopit.fm");
    ui.label(
        "When off, the web/Android apps can no longer control this player \
         (it still lists and controls them).",
    );
    let mut allow_remote = state.lock().unwrap().allow_remote_control;
    if ui.checkbox(&mut allow_remote, "Allow remote control").changed() {
        if let Err(e) = {
            let mut conf = config::Config::load();
            conf.allow_remote_control = allow_remote;
            conf.save()
        } {
            let mut s = state.lock().unwrap();
            s.error = Some(format!("saving config: {e:#}"));
        } else {
            state.lock().unwrap().allow_remote_control = allow_remote;
        }
        ctx.request_repaint();
    }

    ui.with_layout(egui::Layout::bottom_up(egui::Align::Center), |ui| {
        let resp = ui.add_enabled(!loading, egui::Button::new("Save"));
        if resp.clicked() {
            let key = state.lock().unwrap().input.trim().to_string();
            if !key.is_empty() {
                connect(state.clone(), handle, ctx, key);
            }
        }
    });
}

fn open_url(url: &str) {
    let url = url.to_string();
    std::thread::spawn(move || {
        if let Err(e) = open::that_detached(&url) {
            tracing::warn!("opening {url}: {e}");
        }
    });
}

/// Single-instance lock, so "Change key…" twice does not open two windows.
fn acquire_lock() -> Option<File> {
    let dir = config::ensure_config_dir();
    let path = dir.join("config-window.lock");
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
        tracing::info!("config window already open");
        None
    }
}

/// Run the config window (the `--config` mode).
pub fn run() -> anyhow::Result<()> {
    // The tray process starts us; if one is already open, exit quietly.
    let lock = match acquire_lock() {
        Some(f) => f,
        None => return Ok(()),
    };

    let conf = config::Config::load();
    let state = Arc::new(Mutex::new(UiState {
        input: conf.api_key,
        loading: false,
        error: None,
        autostart: conf.autostart,
        notify_new_track: conf.notify_new_track,
        notify_duration_secs: conf.notify_duration_secs,
        notify_album_art: conf.notify_album_art,
        allow_remote_control: conf.allow_remote_control,
        saved: false,
    }));

    // Owns the runtime; must outlive the eframe loop.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("building tokio runtime");
    let handle = runtime.handle().clone();

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size(egui::vec2(440.0, 400.0))
            .with_min_inner_size(egui::vec2(380.0, 320.0))
            .with_title("RadioPit")
            .with_icon(Arc::new(icon::egui_icon(false))),
        ..Default::default()
    };

    // `eframe::Error` is not `Send + Sync`, so it cannot be held in an
    // `anyhow::Error`; format it instead.
    let result = match eframe::run_native(
        "radiopittray-config",
        native_options,
        Box::new(move |_cc| Ok(Box::new(ConfigApp::new(state.clone(), handle.clone())))),
    ) {
        Ok(()) => Ok(()),
        Err(e) => Err(anyhow::anyhow!("eframe error: {e:?}")),
    };
    // The lock is released here, together with the process.
    drop(lock);
    result
}

/// Open (or re-open) the config window as a detached child process.
/// Called from the tray process.
pub fn spawn_config_window() {
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!("resolving executable: {e}");
            return;
        }
    };
    match std::process::Command::new(config_window_exe(&exe))
        .arg("--config")
        .spawn()
    {
        Ok(_) => tracing::info!("starting config window"),
        Err(e) => tracing::warn!("starting config window: {e}"),
    }
}

/// The path to re-exec for the config window. Normally just
/// `current_exe()`; but if the binary is replaced or deleted while it is
/// running (an in-place rebuild, a package upgrade), that reports a
/// `... (deleted)` path that cannot be spawned. On Linux the
/// `/proc/self/exe` magic link still resolves to the running inode.
fn config_window_exe(exe: &std::path::Path) -> &std::path::Path {
    if cfg!(target_os = "linux") && exe.to_string_lossy().ends_with(" (deleted)") {
        std::path::Path::new("/proc/self/exe")
    } else {
        exe
    }
}

#[cfg(test)]
mod tests {
    use super::config_window_exe;
    use std::path::Path;

    #[test]
    fn normal_path_is_kept() {
        let p = Path::new("/usr/local/bin/radiopittray");
        assert_eq!(config_window_exe(p), p);
    }

    #[test]
    fn deleted_path_falls_back_to_proc_self_exe() {
        let p = Path::new("/usr/local/bin/radiopittray (deleted)");
        let got = config_window_exe(p);
        if cfg!(target_os = "linux") {
            assert_eq!(got, Path::new("/proc/self/exe"));
        } else {
            assert_eq!(got, p);
        }
    }
}
