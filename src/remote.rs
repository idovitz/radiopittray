//! Remote control of the RadioPit account ("Playing on…").
//!
//! One WebSocket to the RadioPit server (same host as the HTTP API),
//! authenticated with `{ userId, apiKey }`. The tray registers as a
//! player — so web/Android devices can control it — and at the same
//! time acts as a controller — so the tray can steer the account's
//! other online players.
//!
//! Protocol (as spoken by the web app, `protocol >= 1`):
//!
//! ```text
//! -> AUTH { userId, apiKey }              <- AUTH_OK { protocol }
//! -> DEVICE_HELLO { deviceId, name, kind, canPlay }
//! <- DEVICES [ { deviceId, name, kind, canPlay, state? } ]   (full list)
//! -> DEVICE_STATE { state, volume, mode, stationId? }
//! <- DEVICE_STATE { deviceId, state, volume, mode, stationId? }
//! -> PING                                  (the server drops devices silent for ~60 s)
//! -> COMMAND { to, action, params }         (the server routes it to `to`)
//! <- COMMAND { from, action, params }       (delivered with the sender's id)
//! <- ERROR { code }
//! ```
//!
//! This module owns only the socket. It writes the device list and the
//! connection flag into `Shared`, and forwards inbound `COMMAND`s to
//! the tray's main loop over a channel (so play/stop/volume go through
//! the same code path as a menu click). The main loop sends commands
//! out through the same handle. Selecting which device to control is
//! plain `Shared` state — the server does not need to know.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, WebSocketStream};

use crate::api::Client;
use crate::state::{RemoteState, Shared};

/// The `kind` the server accepts for this player. The RadioPit backend
/// currently only validates `"web"` and `"android"` (anything else
/// answers `ERROR bad_device`); switch to `"tray"` once it accepts it.
const KIND: &str = "web";
/// PING cadence; the server drops devices silent for ~60 s.
const PING_EVERY: Duration = Duration::from_secs(25);
/// Delay between (re)connection attempts.
const RECONNECT_DELAY: Duration = Duration::from_secs(5);
/// Delay before retrying a server that rejects us (no protocol,
/// `bad_device`).
const BACKOFF_DELAY: Duration = Duration::from_secs(60);
/// How often this device re-publishes its own state. The payload is
/// diffed, so a steady state stays silent.
const STATE_POLL: Duration = Duration::from_millis(250);
/// How often to re-check the config while idle (no key yet).
const IDLE_POLL: Duration = Duration::from_secs(30);
/// The web app caps device names at 40 characters; so do we.
const MAX_NAME: usize = 40;

/// A remote-control action, in the protocol's words.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Play,
    Pause,
    SetVolume,
    SetStation,
}

impl Action {
    fn wire(self) -> &'static str {
        match self {
            Action::Play => "PLAY",
            Action::Pause => "PAUSE",
            Action::SetVolume => "SET_VOLUME",
            Action::SetStation => "SET_STATION",
        }
    }
}

/// What the tray asks the socket to do.
#[derive(Debug, Clone)]
pub enum RemoteOut {
    /// Steer another device. `volume` (0..=100) is set for `SetVolume`,
    /// `station_id` for `SetStation`.
    Command {
        to: String,
        action: Action,
        volume: Option<u32>,
        station_id: Option<String>,
    },
    /// Drop the session and re-establish it with the current config
    /// (key or URL changed).
    Reconnect,
}

/// A command the server routed to this device.
#[derive(Debug, Clone)]
pub enum RemoteInCmd {
    Play,
    Pause,
    /// 0..=100.
    SetVolume(u32),
    SetStation(String),
}

/// Owns the remote-control task; the tray's main loop uses this.
pub struct RemoteHandle {
    out_tx: UnboundedSender<RemoteOut>,
    in_rx: UnboundedReceiver<RemoteInCmd>,
}

impl RemoteHandle {
    /// Spawn the long-running task on the given runtime.
    pub fn spawn(handle: tokio::runtime::Handle, shared: Arc<Mutex<Shared>>) -> Self {
        let (out_tx, out_rx) = unbounded_channel::<RemoteOut>();
        let (in_tx, in_rx) = unbounded_channel::<RemoteInCmd>();
        handle.spawn(remote_loop(shared, in_tx, out_rx));
        Self { out_tx, in_rx }
    }

    pub fn send(&self, cmd: RemoteOut) {
        if let Err(e) = self.out_tx.send(cmd) {
            tracing::debug!("remote task gone: {e}");
        }
    }

    /// A command the server routed to this device, if any is pending.
    pub fn try_recv_in(&mut self) -> Option<RemoteInCmd> {
        self.in_rx.try_recv().ok()
    }
}

/// The reconnect loop: read credentials, run one session, repeat.
async fn remote_loop(
    shared: Arc<Mutex<Shared>>,
    in_tx: UnboundedSender<RemoteInCmd>,
    mut out_rx: UnboundedReceiver<RemoteOut>,
) {
    loop {
        let (base, key) = {
            let s = shared.lock().unwrap();
            (s.config.api_url.clone(), s.config.api_key.clone())
        };
        if key.is_empty() {
            // No account: be absent from the "Playing on…" list and
            // wait for the config to change (the tray loop sends
            // Reconnect when the key lands).
            set_connected(&shared, false, true);
            match tokio::time::timeout(IDLE_POLL, out_rx.recv()).await {
                Ok(Some(_)) => continue,
                Ok(None) => return, // handle dropped -> process exiting
                Err(_) => continue, // re-check the config
            }
        }

        // The WebSocket authenticates with the internal user id, which
        // the HTTP API hands out against the key.
        let user_id = match Client::new(&base).auth_user_id(&key).await {
            Ok(id) => id,
            Err(e) => {
                tracing::warn!("remote: resolving user id: {e:#}");
                set_connected(&shared, false, true);
                tokio::time::sleep(RECONNECT_DELAY).await;
                continue;
            }
        };

        // Run one session; it ends when the socket dies or the tray
        // asks to reconnect (key/URL change).
        let backoff =
            session(&shared, &in_tx, &mut out_rx, &base, &key, &user_id).await;
        tokio::time::sleep(if backoff {
            BACKOFF_DELAY
        } else {
            RECONNECT_DELAY
        })
        .await;
    }
}

/// One connected session. Returns whether the next attempt should
/// back off (the server rejected us, not the network).
async fn session(
    shared: &Arc<Mutex<Shared>>,
    in_tx: &UnboundedSender<RemoteInCmd>,
    out_rx: &mut UnboundedReceiver<RemoteOut>,
    base: &str,
    key: &str,
    user_id: &str,
) -> bool {
    let wss = wss_url(base);
    let mut sock = match connect_async(wss.clone()).await {
        Ok((s, _)) => s,
        Err(e) => {
            tracing::debug!("remote: connecting to {wss}: {e:#}");
            return false;
        }
    };
    tracing::info!("remote: connected to {wss}");

    let (device_id, name, allow_remote) = {
        let s = shared.lock().unwrap();
        (
            s.config
                .device_id
                .clone()
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
            device_name(),
            s.config.allow_remote_control,
        )
    };

    let msg = json!({ "type": "AUTH", "userId": user_id, "apiKey": key });
    if sock
        .send(Message::Text(msg.to_string().into()))
        .await
        .is_err()
    {
        return false;
    }

    // Wait for AUTH_OK; anything else means a protocol mismatch.
    let protocol = match recv_value(&mut sock).await {
        Some(Ok(im)) if im.kind == "AUTH_OK" => im.protocol,
        _ => return false,
    };
    let Some(protocol) = protocol else {
        // A server from before remote control: connect again later.
        return true;
    };
    if protocol < 1 {
        return true;
    }

    // Registered, and the server speaks remote control.
    set_connected(shared, true, true);
    // With remote control disabled we register as non-playable: the server
    // then won't route inbound commands to us, and the other clients'
    // "Control device:" lists hide us — but we still receive the device
    // list and can send commands (the outgoing direction is unaffected).
    if !allow_remote {
        tracing::info!("remote: inbound remote control disabled (registering non-playable)");
    }
    let hello = json!({
        "type": "DEVICE_HELLO",
        "payload": { "deviceId": device_id, "name": name, "kind": KIND, "canPlay": allow_remote },
    });
    if sock
        .send(Message::Text(hello.to_string().into()))
        .await
        .is_err()
    {
        return false;
    }

    let mut last_published: Option<String> = None;
    let mut hello_sent = true; // a `not_registered` error gets one retry
    let mut ping = tokio::time::interval(PING_EVERY);
    let mut poll = tokio::time::interval(STATE_POLL);
    ping.tick().await; // drop the immediate first tick
    poll.tick().await;

    loop {
        tokio::select! {
            cmd = out_rx.recv() => match cmd {
                Some(RemoteOut::Reconnect) => return false,
                Some(RemoteOut::Command { to, action, volume, station_id }) => {
                    let params = match action {
                        Action::SetVolume => json!({ "volume": volume.unwrap_or(0) }),
                        Action::SetStation => json!({ "stationId": station_id.unwrap_or_default() }),
                        _ => json!({}),
                    };
                    let msg = json!({
                        "type": "COMMAND",
                        "payload": { "to": to, "action": action.wire(), "params": params },
                    });
                    tracing::debug!("remote: sending {} to {to}", action.wire());
                    if sock.send(Message::Text(msg.to_string().into())).await.is_err() {
                        return false;
                    }
                }
                None => return false, // tray process exiting
            },

            _ = ping.tick() => {
                let msg = json!({ "type": "PING" });
                if sock.send(Message::Text(msg.to_string().into())).await.is_err() {
                    return false;
                }
            }

            _ = poll.tick() => {
                // Publish our own state when it changed.
                let st = shared.lock().unwrap().local_remote_state();
                let wire = state_wire(&st).to_string();
                if last_published.as_deref() != Some(wire.as_str()) {
                    last_published = Some(wire.clone());
                    let msg = json!({ "type": "DEVICE_STATE", "payload": state_wire(&st) });
                    if sock.send(Message::Text(msg.to_string().into())).await.is_err() {
                        return false;
                    }
                }
            }

            msg = recv_value(&mut sock) => match msg {
                Some(Ok(im)) => match im.kind.as_str() {
                    "DEVICES" => {
                        let list = match im.payload.as_ref() {
                            Some(p) => match serde_json::from_value::<Vec<WireDevice>>(p.clone()) {
                                Ok(l) => l,
                                Err(e) => {
                                    tracing::warn!(
                                        "remote: parsing DEVICES failed: {e} (payload={p})"
                                    );
                                    Vec::new()
                                }
                            },
                            None => Vec::new(),
                        };
                        apply_devices(shared, &device_id, &list);
                    }
                    "DEVICE_STATE" => {
                        if let Some(p) = im.payload.as_ref() {
                            if let (Some(id), Some(st)) = (
                                p.get("deviceId").and_then(Value::as_str),
                                state_from_wire(p),
                            ) {
                                apply_device_state(shared, id, st);
                            }
                        }
                    }
                    "COMMAND" => handle_command(in_tx, allow_remote, im.payload.as_ref()),
                    "ERROR" => {
                        let code = im
                            .payload
                            .as_ref()
                            .and_then(|p| p.get("code"))
                            .and_then(Value::as_str)
                            .unwrap_or("unknown");
                        match code {
                            // The server forgot us (e.g. it restarted): one re-hello.
                            "not_registered" if hello_sent => {
                                hello_sent = false;
                                if sock
                                    .send(Message::Text(hello.to_string().into()))
                                    .await
                                    .is_err()
                                {
                                    return false;
                                }
                            }
                            "bad_device" => {
                                tracing::warn!(
                                    "remote: server rejected this device (kind {KIND:?}); \
                                     retrying in {BACKOFF_DELAY:?}"
                                );
                                return true;
                            }
                            other => tracing::warn!("remote: server error {other:?}"),
                        }
                    }
                    "PONG" | "AUTH_OK" => {}
                    other => tracing::debug!("remote: unhandled message {other:?}"),
                },
                _ => return false, // closed or unparseable
            }
        }
    }
}

// ── Shared updates ─────────────────────────────────────────────────────

/// (Re)set the connection flag; optionally clear the device list and
/// any controller selection (nothing is reachable without a session).
fn set_connected(shared: &Arc<Mutex<Shared>>, connected: bool, clear_devices: bool) {
    let mut s = shared.lock().unwrap();
    if s.remote_connected != connected {
        s.remote_connected = connected;
        s.ui_dirty = true;
    }
    if clear_devices && (!s.remote_devices.is_empty() || s.remote_target.is_some()) {
        s.remote_devices.clear();
        s.remote_target = None;
        s.ui_dirty = true;
    }
}

/// Replace the device list with the server's snapshot (full list,
/// minus this device and devices that cannot play). A controller
/// selection pointing at a device that is no longer listed is dropped,
/// as the web app does.
fn apply_devices(
    shared: &Arc<Mutex<Shared>>,
    self_id: &str,
    list: &[WireDevice],
) {
    let mut s = shared.lock().unwrap();
    let devices: Vec<crate::state::RemoteDevice> = list
        .iter()
        .filter(|d| d.device_id != self_id && d.can_play)
        .map(|d| crate::state::RemoteDevice {
            id: d.device_id.clone(),
            name: d.name.clone(),
            state: d.state.as_ref().map(state_from_wire_opt),
        })
        .collect();
    tracing::debug!(
        "remote: DEVICES: {} total, {} controllable (self={self_id})",
        list.len(),
        devices.len(),
    );
    let target_gone = s
        .remote_target
        .as_ref()
        .map(|t| !devices.iter().any(|d| d.id == *t))
        .unwrap_or(false);
    s.remote_devices = devices;
    if target_gone {
        s.remote_target = None;
    }
    s.ui_dirty = true;
}

/// Update one device's last reported state.
fn apply_device_state(shared: &Arc<Mutex<Shared>>, device_id: &str, st: RemoteState) {
    let mut s = shared.lock().unwrap();
    if s.remote_devices.iter().any(|d| d.id == device_id) {
        if let Some(d) = s.remote_devices.iter_mut().find(|d| d.id == device_id) {
            d.state = Some(st);
        }
        s.ui_dirty = true;
    }
}

/// A command the server routed to this device: validate it and hand it
/// to the tray's main loop (which runs it through the same code path as
/// a menu click). The server already routes by recipient — the payload
/// it delivers carries the sender's id in `from`, not a `to` field — so
/// there is nothing to filter on here. Inbound commands are dropped when
/// remote control is disabled (normally the server already won't route
/// them to a non-playable device; this is belt-and-suspenders).
fn handle_command(
    in_tx: &UnboundedSender<RemoteInCmd>,
    allow_remote: bool,
    payload: Option<&Value>,
) {
    let p = payload;
    if !allow_remote {
        tracing::debug!("remote: ignoring inbound command (remote control disabled)");
        return;
    }
    tracing::debug!(
        "remote: command from {:?}",
        p.and_then(|p| p.get("from")).and_then(|v| v.as_str())
    );
    let action = match p.and_then(|p| p.get("action")).and_then(Value::as_str) {
        Some(a) => a,
        None => return,
    };
    let params = p.and_then(|p| p.get("params"));
    let cmd = match action {
        "PLAY" => RemoteInCmd::Play,
        "PAUSE" => RemoteInCmd::Pause,
        "SET_VOLUME" => {
            let v = match params.and_then(|q| q.get("volume")).and_then(Value::as_u64) {
                Some(v) => v,
                None => return,
            };
            RemoteInCmd::SetVolume(v.clamp(0, 100) as u32)
        }
        "SET_STATION" => {
            let id = match params
                .and_then(|q| q.get("stationId"))
                .and_then(Value::as_str)
            {
                Some(id) if !id.is_empty() => id.to_string(),
                _ => return,
            };
            RemoteInCmd::SetStation(id)
        }
        // SET_EQ / SET_MODE / SET_OUTPUT: the tray has no equalizer and
        // no output modes; the web app likewise ignores them on
        // devices without them.
        other => {
            tracing::debug!("remote: ignoring command {other:?}");
            return;
        }
    };
    if in_tx.send(cmd).is_err() {
        tracing::debug!("remote: tray loop gone, dropping command");
    }
}

// ── Wire format ────────────────────────────────────────────────────────

/// One inbound message, in the shapes the server sends.
#[derive(Deserialize)]
struct InMsg {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    payload: Option<Value>,
    #[serde(default)]
    protocol: Option<u32>,
}

/// A device in a `DEVICES` snapshot.
#[derive(Debug, Deserialize)]
struct WireDevice {
    #[serde(rename = "deviceId")]
    device_id: String,
    name: String,
    #[serde(rename = "canPlay", default)]
    can_play: bool,
    #[serde(default)]
    state: Option<Value>,
}

/// Serialize a state for our own `DEVICE_STATE` messages.
fn state_wire(st: &RemoteState) -> Value {
    let mut v = json!({
        "state": st.state,
        "volume": st.volume,
        "mode": st.mode.as_deref().unwrap_or("normal"),
    });
    if let Some(id) = &st.station_id {
        v["stationId"] = json!(id);
    }
    v
}

/// Parse a state object out of a `DEVICE_STATE` payload.
fn state_from_wire(p: &Value) -> Option<RemoteState> {
    Some(RemoteState {
        state: p.get("state")?.as_str()?.to_string(),
        volume: p.get("volume").and_then(Value::as_u64).unwrap_or(0) as u32,
        mode: p
            .get("mode")
            .and_then(Value::as_str)
            .map(|s| s.to_string()),
        station_id: p
            .get("stationId")
            .and_then(Value::as_str)
            .map(|s| s.to_string()),
    })
}

/// Same as [`state_from_wire`] for an optional object.
fn state_from_wire_opt(v: &Value) -> RemoteState {
    state_from_wire(v).unwrap_or_else(|| RemoteState {
        state: "stopped".into(),
        volume: 0,
        mode: None,
        station_id: None,
    })
}

/// Read one text message and parse it as an inbound protocol message.
/// `None` means the socket is closed.
async fn recv_value(
    sock: &mut WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
) -> Option<Result<InMsg, ()>> {
    let msg = match sock.next().await {
        Some(Ok(m)) => m,
        _ => return None,
    };
    let text = match msg {
        Message::Text(t) => t,
        Message::Close(_) => return None,
        _ => return None,
    };
    let v: Value = serde_json::from_str(&text).ok()?;
    let kind = v.get("type")?.as_str()?.to_string();
    Some(Ok(InMsg {
        kind,
        payload: v.get("payload").cloned(),
        protocol: v.get("protocol").and_then(Value::as_u64).map(|p| p as u32),
    }))
}

/// The WebSocket url for an HTTP(S) API base (the web app does the
/// same https->wss swap).
fn wss_url(base: &str) -> String {
    base.trim_end_matches('/')
        .replacen("https://", "wss://", 1)
        .replacen("http://", "ws://", 1)
        .to_string()
}

/// The name this player shows in the "Playing on…" list, capped like
/// the web app's names.
fn device_name() -> String {
    let host = std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "my computer".into());
    let name = format!("radiopittray ({host})");
    if name.chars().count() <= MAX_NAME {
        name
    } else {
        name.chars().take(MAX_NAME).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wss_url_swaps_the_scheme() {
        assert_eq!(wss_url("https://api.radiopit.fm"), "wss://api.radiopit.fm");
        assert_eq!(
            wss_url("https://api.radiopit.fm/"),
            "wss://api.radiopit.fm"
        );
        assert_eq!(wss_url("http://localhost:8080"), "ws://localhost:8080");
    }

    #[test]
    fn action_wire_names() {
        assert_eq!(Action::Play.wire(), "PLAY");
        assert_eq!(Action::Pause.wire(), "PAUSE");
        assert_eq!(Action::SetVolume.wire(), "SET_VOLUME");
        assert_eq!(Action::SetStation.wire(), "SET_STATION");
    }

    #[test]
    fn state_wire_uses_camel_case_station_id() {
        let st = RemoteState {
            state: "playing".into(),
            volume: 80,
            mode: Some("normal".into()),
            station_id: Some("abc".into()),
        };
        let v = state_wire(&st);
        assert_eq!(v["stationId"], json!("abc"));
        assert_eq!(v["volume"], json!(80));
        assert!(v.get("mode").is_some());

        let st = RemoteState {
            state: "stopped".into(),
            volume: 0,
            mode: None,
            station_id: None,
        };
        let v = state_wire(&st);
        assert!(v.get("stationId").is_none());
    }

    #[test]
    fn state_from_wire_parses_a_payload() {
        let p = json!({
            "deviceId": "d1",
            "state": "playing",
            "volume": 42,
            "mode": "normal",
            "stationId": "s1",
        });
        let st = state_from_wire(&p).unwrap();
        assert_eq!(st.state, "playing");
        assert_eq!(st.volume, 42);
        assert_eq!(st.station_id.as_deref(), Some("s1"));
    }

    #[test]
    fn state_from_wire_rejects_garbage() {
        assert!(state_from_wire(&json!({})).is_none());
        assert!(state_from_wire(&json!({"state": 3})).is_none());
    }

    #[test]
    fn wire_device_reads_camel_case_can_play() {
        // The server sends `canPlay`; without the rename it would default
        // to false and every device would be filtered out of the list.
        let d: WireDevice = serde_json::from_value(json!({
            "deviceId": "d1", "name": "Pixel 9 Pro", "kind": "android",
            "canPlay": true, "state": { "state": "playing" },
        }))
        .unwrap();
        assert!(d.can_play);
        assert_eq!(d.device_id, "d1");

        // Missing canPlay still defaults (a device that cannot play).
        let d: WireDevice =
            serde_json::from_value(json!({ "deviceId": "d2", "name": "n" })).unwrap();
        assert!(!d.can_play);
    }

    #[test]
    fn device_name_is_capped() {
        let name = device_name();
        assert!(name.starts_with("radiopittray ("));
        assert!(name.chars().count() <= MAX_NAME);
    }

    /// Live check against the real RadioPit server: auth, hello,
    /// device list, and a command round-trip to itself. Skipped in
    /// plain `cargo test`; run with
    ///
    /// ```sh
    /// RADIOPI_LIVE_TEST_KEY=<key> cargo test -- --ignored
    /// ```
    #[test]
    #[ignore]
    fn live_auth_hello_and_command_roundtrip() {
        let Some(key) = std::env::var("RADIOPI_LIVE_TEST_KEY").ok() else {
            eprintln!("RADIOPI_LIVE_TEST_KEY not set, skipping live test");
            return;
        };
        let base = std::env::var("RADIOPI_LIVE_TEST_API_URL")
            .unwrap_or_else(|_| "https://api.radiopit.fm".into());
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        rt.block_on(async {
            let user_id = crate::api::Client::new(&base)
                .auth_user_id(&key)
                .await
                .expect("auth/validate");
            let (mut sock, _response) = connect_async(wss_url(&base))
                .await
                .expect("connecting");
            let auth = json!({ "type": "AUTH", "userId": user_id, "apiKey": key });
            sock.send(Message::Text(auth.to_string().into()))
                .await
                .expect("sending AUTH");
            let msg = recv_value(&mut sock).await.expect("AUTH_OK").expect("AUTH_OK");
            assert_eq!(msg.kind, "AUTH_OK");
            assert!(msg.protocol.unwrap_or(0) >= 1, "server lacks remote control");

            let device_id = uuid::Uuid::new_v4().to_string();
            let hello = json!({
                "type": "DEVICE_HELLO",
                "payload": { "deviceId": device_id, "name": "radiopittray-test",
                             "kind": KIND, "canPlay": true },
            });
            sock.send(Message::Text(hello.to_string().into()))
                .await
                .expect("sending DEVICE_HELLO");
            sock.send(Message::Text(json!({ "type": "COMMAND",
                "payload": { "to": device_id, "action": "PLAY", "params": {} } })
                .to_string()
                .into()))
                .await
                .expect("sending COMMAND");
            // The server should answer with DEVICES (itself in the list)
            // and route the command back, rewritten with `from`. Read raw
            // frames (skipping pings) rather than using recv_value, so a
            // protocol frame can't masquerade as a closed socket.
            let mut saw_devices = false;
            let mut saw_command = false;
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while (!saw_devices || !saw_command) && std::time::Instant::now() < deadline {
                let frame = match tokio::time::timeout(
                    std::time::Duration::from_secs(4),
                    sock.next(),
                )
                .await
                {
                    Ok(Some(Ok(m))) => m,
                    other => {
                        eprintln!("live-test: recv stopped early: {other:?}");
                        break;
                    }
                };
                let text = match frame {
                    Message::Text(t) => t,
                    Message::Close(_) => break,
                    other => {
                        eprintln!("live-test: skipping frame {other:?}");
                        continue;
                    }
                };
                let v: Value = match serde_json::from_str(&text) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                let payload = v.get("payload").cloned();
                match v.get("type").and_then(Value::as_str).unwrap_or("") {
                    "DEVICES" => {
                        let list = payload
                            .and_then(|p| serde_json::from_value::<Vec<WireDevice>>(p).ok())
                            .unwrap_or_default();
                        assert!(
                            list.iter().any(|d| d.device_id == device_id),
                            "this device is missing from DEVICES: {list:?}"
                        );
                        saw_devices = true;
                    }
                    "COMMAND" => {
                        saw_command = true;
                        assert_eq!(
                            payload
                                .as_ref()
                                .and_then(|p| p.get("from"))
                                .and_then(Value::as_str)
                                .unwrap_or(""),
                            device_id,
                            "command round-trip should carry `from`"
                        );
                        assert_eq!(
                            payload
                                .as_ref()
                                .and_then(|p| p.get("action"))
                                .and_then(Value::as_str),
                            Some("PLAY")
                        );
                    }
                    other => eprintln!("live-test: frame type {other:?}"),
                }
            }
            assert!(saw_devices, "no DEVICES after hello");
            assert!(saw_command, "command did not round-trip");
        });
    }
}
