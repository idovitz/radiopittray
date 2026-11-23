//! Audio playback on a dedicated thread using GStreamer.
//!
//! Pipeline: a single `playbin` element. It auto-plugs source, decoder and
//! audio sink, which avoids the decodebin "link before the src pad exists"
//! dance (a decodebin's src pad only appears dynamically via pad-added).
//! Volume and mute are playbin's own proxied properties.
//!
//! Commands come in over an mpsc channel; state changes go out over a second
//! channel. Stream errors are read from the GStreamer bus (there is no
//! `error` signal in this version of the bindings); a few automatic
//! retries are made before giving up.
//!
//! There is no pause: "stop" tears the stream down completely, and "play"
//! always (re)starts the stream at the live edge.

use std::sync::mpsc::{RecvTimeoutError, Receiver, Sender};
use std::time::{Duration, Instant};

use gstreamer as gst;
use gstreamer::prelude::*;

use crate::api::Station;

const MAX_RETRIES: u32 = 3;
/// Ignore a short burst of error messages (GStreamer can emit several for
/// one underlying failure).
const MIN_RETRY_GAP: Duration = Duration::from_secs(2);

#[derive(Debug)]
pub enum Cmd {
    Play(Station),
    Stop,
    SetVolume(f64),
    SetMute(bool),
}

#[derive(Debug)]
pub enum Ev {
    Playing,
    Stopped,
    Failed(String),
}

/// Spawn the player thread.
pub fn spawn(
    cmd_rx: Receiver<Cmd>,
    ev_tx: Sender<Ev>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("radiopit-player".into())
        .spawn(move || {
            let panic_tx = ev_tx.clone();
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run_guarded(cmd_rx, ev_tx);
            }));
            if let Err(panic) = result {
                let msg = panic
                    .downcast_ref::<String>()
                    .cloned()
                    .unwrap_or_else(|| "unknown panic".into());
                tracing::error!("player thread panicked: {msg}");
                let _ = panic_tx.send(Ev::Failed(format!("player thread panicked: {msg}")));
            }
        })
        .expect("spawning player thread")
}

fn run_guarded(cmd_rx: Receiver<Cmd>, ev_tx: Sender<Ev>) {
    tracing::debug!("player thread: initializing GStreamer");
    if let Err(e) = gst::init() {
        let _ = ev_tx.send(Ev::Failed(format!("GStreamer init failed: {e}")));
        return;
    }
    tracing::debug!("player thread: GStreamer initialized");
    for plugin in ["playbin"] {
        if gst::ElementFactory::find(plugin).is_none() {
            let _ = ev_tx.send(Ev::Failed(format!(
                "GStreamer element '{plugin}' not found. \
                 Install the GStreamer plugins \
                 (gstreamer-plugins-base/good/bad/ugly)."
            )));
            return;
        }
    }
    tracing::debug!("player thread: all elements present, building pipeline");
    run(cmd_rx, ev_tx);
    tracing::debug!("player thread: exiting");
}

struct Player {
    pipeline: gst::Pipeline,
    bus: gst::Bus,
    playbin: gst::Element,
    uri: Option<String>,
    want_playing: bool,
    retries: u32,
    last_error: Option<Instant>,
}

impl Player {
    fn restart_stream(&mut self, ev_tx: &Sender<Ev>) {
        let Some(uri) = self.uri.clone() else {
            return;
        };
        // Full reset: Null -> set uri -> Playing. Just re-setting the state
        // does not always restart a dead stream.
        let _ = self.pipeline.set_state(gst::State::Null);
        self.playbin.set_property("uri", uri.as_str());
        match self.pipeline.set_state(gst::State::Playing) {
            Ok(_) => {
                tracing::info!("stream (re)started: {uri}");
                let _ = ev_tx.send(Ev::Playing);
            }
            Err(e) => {
                tracing::warn!("starting stream: {e}");
            }
        }
    }

    /// Handle a GStreamer error message: retry a few times, then give up.
    fn handle_error(&mut self, ev_tx: &Sender<Ev>, reason: &str) {
        tracing::warn!("stream error: {reason}");
        if !self.want_playing || self.uri.is_none() {
            return;
        }
        // Debounce: GStreamer may emit several error messages for one failure.
        let now = Instant::now();
        if let Some(last) = self.last_error {
            if now.duration_since(last) < MIN_RETRY_GAP {
                return;
            }
        }
        self.last_error = Some(now);

        self.retries += 1;
        if self.retries > MAX_RETRIES {
            self.want_playing = false;
            let _ = ev_tx.send(Ev::Failed(format!(
                "stream keeps failing, giving up ({reason})"
            )));
            return;
        }
        tracing::info!("reconnect attempt {}/{}", self.retries, MAX_RETRIES);
        self.restart_stream(ev_tx);
    }

    /// Poll the bus for messages that matter to us (mainly errors).
    fn drain_bus(&mut self, ev_tx: &Sender<Ev>) {
        while let Some(msg) = self.bus.pop() {
            if let gstreamer::MessageView::Error(err) = msg.view() {
                self.handle_error(ev_tx, &err.to_string());
            }
        }
    }
}

fn build_player() -> Option<Player> {
    let pipeline = gst::Pipeline::builder().build();
    let playbin = match gst::ElementFactory::make("playbin").build() {
        Ok(e) => e,
        Err(e) => {
            tracing::error!("player: creating playbin failed: {e}");
            return None;
        }
    };

    if let Err(e) = pipeline.add(&playbin) {
        tracing::error!("player: adding playbin to pipeline failed: {e}");
        return None;
    }

    let bus = pipeline.bus()?;

    Some(Player {
        pipeline,
        bus,
        playbin,
        uri: None,
        want_playing: false,
        retries: 0,
        last_error: None,
    })
}

fn run(cmd_rx: Receiver<Cmd>, ev_tx: Sender<Ev>) {
    let Some(mut p) = build_player() else {
        let _ = ev_tx.send(Ev::Failed("could not build GStreamer pipeline".into()));
        return;
    };

    loop {
        p.drain_bus(&ev_tx);

        match cmd_rx.recv_timeout(Duration::from_millis(250)) {
            Ok(Cmd::Play(station)) => {
                p.retries = 0;
                p.last_error = None;
                // Stop any running stream first: changing playbin's uri
                // mid-playback does not reliably stop the old stream.
                let _ = p.pipeline.set_state(gst::State::Null);
                p.uri = Some(station.url.clone());
                p.playbin.set_property("uri", station.url.as_str());
                p.want_playing = true;
                match p.pipeline.set_state(gst::State::Playing) {
                    Ok(_) => {
                        tracing::info!("playing: {}", station.name);
                        let _ = ev_tx.send(Ev::Playing);
                    }
                    Err(e) => {
                        p.want_playing = false;
                        let _ = ev_tx.send(Ev::Failed(format!(
                            "could not start stream: {e}"
                        )));
                    }
                }
            }
            Ok(Cmd::Stop) => {
                p.want_playing = false;
                p.uri = None;
                p.retries = 0;
                p.last_error = None;
                let _ = p.pipeline.set_state(gst::State::Null);
                tracing::info!("stopped");
                let _ = ev_tx.send(Ev::Stopped);
            }
            Ok(Cmd::SetVolume(v)) => {
                p.playbin.set_property("volume", v.clamp(0.0, 1.0));
            }
            Ok(Cmd::SetMute(m)) => {
                p.playbin.set_property("mute", m);
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                tracing::debug!("player thread: command channel closed, exiting");
                break;
            }
        }
    }

    let _ = p.pipeline.set_state(gst::State::Null);
}
