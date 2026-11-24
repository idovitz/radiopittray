# radiopittray

A system tray radio player for your [RadioPit.fm](https://radiopit.fm)
library, written in Rust. It is a **pure tray app**: the tray icon is the
icon of the station that is playing, and the menu shows what is playing and
lets you pick stations. The app opens **no window** — the only window of the
program is the configuration screen (Configuration… in the menu), which runs
as a separate process and closes again. Audio is played through GStreamer;
track titles/artists come from the stream's ICY metadata. You edit your
stations on radiopit.fm itself — this little app needs no editor.

> **Vibe-coded:** this app was written with AI assistance.

## Dependencies

- A recent Rust toolchain (`rustc` + `cargo`)
- GTK 3 / GLib
- GStreamer with the base and good plugin sets, plus the libav plugins
  (codec coverage for whatever the streams throw at the player)
- A StatusNotifierItem-capable tray backend (e.g. libayatana-appindicator)
- `xdg-open` (xdg-utils) for the "Open radiopit.fm" menu item

## Building

```sh
cargo build --release
```

The binary ends up in `target/release/radiopittray`.

## Usage

1. Start `radiopittray`. On the first run (no key yet) the configuration
   screen opens by itself; you find your key on radiopit.fm under
   ⋮ → *Your Key*.
2. Right-click the tray icon:
   - **top**: what is playing — the station title, the track title and the
     artist (as long as the stream provides ICY metadata; some streams,
     e.g. HLS, do not).
   - **Play stream [X] / Stop stream [X]**: a single line that toggles.
     Play always starts the last played station (also after restarting the
     app); Stop stops the stream.
   - *Control device:* (only when other players of the account are online):
     switch which device is being controlled (see
     [Remote control](#remote-control)).
   - stations per playlist — clicking a station plays it immediately (and
     stops the previous stream).
   - Refresh library, Open radiopit.fm, Configuration…, Quit.
3. The tray icon shows the icon of the playing station; when nothing is
   playing (or the icon is missing) it shows the RadioPit logo. The tooltip
   shows the current station.
4. The configuration screen (Configuration…) is a separate window: there you
   set the key (Connect/Save validates and stores it), the *Start at login*
   checkbox and the *Notify on new track* checkbox. The window closes itself
   once the key has been saved.
5. Quitting is only possible via *Quit* in the menu (or Ctrl+C). At most one
   tray process can run; a second start exits with "already running".

## Remote control

radiopittray speaks RadioPit's remote-control protocol, so it appears in the
"Playing on…" list of the web and Android apps **and** can control them in
return:

- **Controllable.** While running, the tray registers on the account as a
  player named `radiopittray (<hostname>)`. From the web or Android app you
  can start/stop it, change its volume, and switch its station; the tray icon
  and tooltip follow what it is playing. (Set `allow_remote_control = false`
  to block this incoming direction — see *Configuration*.)
- **Controlling.** When the account has other online players, the menu gains
  a *Control device:* submenu (plus *Volume −10* / *Volume +10* while one is
  selected). Pick another player to drive it instead of the local one: the
  now-playing info, the play/stop line, and station clicks all apply to the
  selected device, while the tray icon keeps showing this tray's own player
  (it does not mirror the remote). "This device" switches back to local
  playback.

Controlling another device leaves this tray's own player untouched, and a
remote volume change is session-only (it does not rewrite the saved
`volume`). When the tray itself is started by a remote command, it plays as
usual but raises no desktop "new track" notification.

## Configuration

`~/.config/radiopittray/config.toml`:

```toml
api_key = "..."
api_url = "https://api.radiopit.fm"
volume = 0.8
last_station_id = null
autostart = false
notify_new_track = true
notify_duration_secs = 10
device_id = "..."
allow_remote_control = true
```

`last_station_id` is updated on every playback and determines where
*Play stream* plays to. `autostart` writes/removes
`~/.config/autostart/radiopittray.desktop`. `notify_new_track` shows a
desktop notification (XDG `org.freedesktop.Notifications`, as rendered
by GNOME, Plasma, …) whenever a new track starts: the station icon,
the station name, the artist and the track title. `notify_duration_secs`
is how long it stays up (default 10; 0 keeps it on screen until you
dismiss it). It only triggers on
a track that actually changed, so ICY streams that repeat a title do
not spam, and no notification appears when the stream does not provide
metadata. `device_id` is the stable identity behind the
[Remote control](#remote-control) "Playing on…" feature; it is created on
first start if missing. `allow_remote_control` (on by default) switches the
*incoming* direction on or off: when off, the web/Android apps can no longer
control this player (it registers as non-playable, so it is hidden from their
device lists and inbound commands are ignored) — but this player can still
list and control the account's other devices. It is available as the
*Allow remote control* checkbox in the Configuration window.

## Tests

```sh
cargo test
```

An opt-in live check against the real RadioPit server (auth, device
registration, and a command round-trip) is `#[ignore]`d and runs only when you
provide a key:

```sh
RADIOPI_LIVE_TEST_KEY="<your key>" cargo test -- --ignored
```

## Current limitations

- The metadata connection runs 2–3 s alongside the audio stream; shortly
  after playback starts, the track title/artist may be missing for a while.
- Stations without ICY metadata only show the station title.
- The station icon is rendered at 32×32; quality depends on what the server
  serves.
- [Remote control](#remote-control) registers this tray as a "web" device —
  the server currently only accepts "web" and "android" — so it shows up like
  a web player in the "Playing on…" list, and it needs a connection to the
  RadioPit server (when the socket is down the feature is simply unavailable
  and the menu shows only local playback).
