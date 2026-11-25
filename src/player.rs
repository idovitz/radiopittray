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
//! retries are made before giving up. When the direct stream keeps failing
//! (e.g. blocked by the user's firewall), the last retries go through the
//! RadioPit server's stream proxy, like the web app does.
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
    /// Start playing. `proxy_url` is the RadioPit server proxy of
    /// `station.url` (see [`crate::api::proxy_stream_url`]), used as a
    /// fallback when the direct stream keeps failing.
    Play {
        station: Station,
        proxy_url: Option<String>,
    },
    Stop,
    SetVolume(f64),
    SetMute(bool),
}

#[derive(Debug)]
pub enum Ev {
    /// The stream is (re)playing; the string is the URI actually in use
    /// (the direct stream, or the proxy fallback).
    Playing(String),
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
    /// The RadioPit proxy of `uri`, tried when the direct stream keeps
    /// failing (e.g. it is blocked by the local network).
    proxy_uri: Option<String>,
    want_playing: bool,
    retries: u32,
    last_error: Option<Instant>,
}

impl Player {
    fn restart_stream(&mut self, ev_tx: &Sender<Ev>, uri: String) {
        // Full reset: Null -> set uri -> Playing. Just re-setting the state
        // does not always restart a dead stream.
        let _ = self.pipeline.set_state(gst::State::Null);
        self.playbin.set_property("uri", uri.as_str());
        match self.pipeline.set_state(gst::State::Playing) {
            Ok(_) => {
                tracing::info!("stream (re)started: {uri}");
                let _ = ev_tx.send(Ev::Playing(uri));
            }
            Err(e) => {
                tracing::warn!("starting stream: {e}");
            }
        }
    }

    /// The URI for the next retry: once the direct stream has failed a
    /// couple of times, go through the RadioPit proxy (when we have one) —
    /// a stream the local firewall blocks is still reachable there.
    fn retry_uri(&self) -> Option<String> {
        if self.retries >= 2 {
            self.proxy_uri.clone().or_else(|| self.uri.clone())
        } else {
            self.uri.clone()
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
        let Some(uri) = self.retry_uri() else {
            return;
        };
        let via_proxy = self.proxy_uri.as_deref() == Some(uri.as_str());
        tracing::info!(
            "reconnect attempt {}/{}{}",
            self.retries,
            MAX_RETRIES,
            if via_proxy { " (via RadioPit proxy)" } else { "" }
        );
        self.restart_stream(ev_tx, uri);
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
        proxy_uri: None,
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
            Ok(Cmd::Play {
                station,
                proxy_url,
            }) => {
                p.retries = 0;
                p.last_error = None;
                // Stop any running stream first: changing playbin's uri
                // mid-playback does not reliably stop the old stream.
                let _ = p.pipeline.set_state(gst::State::Null);
                p.uri = Some(station.url.clone());
                p.proxy_uri = proxy_url;
                p.playbin.set_property("uri", station.url.as_str());
                p.want_playing = true;
                match p.pipeline.set_state(gst::State::Playing) {
                    Ok(_) => {
                        tracing::info!("playing: {}", station.name);
                        let _ = ev_tx.send(Ev::Playing(station.url));
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
                p.proxy_uri = None;
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
