//! radiopittray — a system tray radio player for your RadioPit.fm library.
//!
//! The tray process (no arguments): the tray icon (StatusNotifierItem via
//! the ksni backend) carries the playlist/controls menu; audio playback runs
//! on GStreamer in a dedicated thread, and ICY stream titles are read on a
//! background task. A left click on the tray icon opens a window with
//! now-playing info and a playback history (see `ui.rs`).
//!
//! The config window is a separate process, started with `--config`: it
//! validates and stores the RadioPit key, and the tray process picks the
//! change up from the config file.

mod api;
mod artwork;
mod config;
mod configui;
mod history;
mod icon;
mod icy;
mod menu;
mod notify;
mod player;
mod remote;
mod state;
mod tray;
mod ui;
mod urls;

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "radiopittray=info,warn".into()),
        )
        .init();

    let config_mode = std::env::args().skip(1).any(|a| a == "--config");
    let result = if config_mode {
        configui::run()
    } else {
        tray::run()
    };
    if let Err(e) = result {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}
