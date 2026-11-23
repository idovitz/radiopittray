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
   - stations per playlist — clicking a station plays it immediately (and
     stops the previous stream).
   - Refresh library, Open radiopit.fm, Configuration…, Quit.
3. The tray icon shows the icon of the playing station; when nothing is
   playing (or the icon is missing) it shows the RadioPit logo. The tooltip
   shows the current station.
4. The configuration screen (Configuration…) is a separate window: there you
   set the key (Connect/Save validates and stores it) and the *Start at
   login* checkbox. The window closes itself once the key has been saved.
5. Quitting is only possible via *Quit* in the menu (or Ctrl+C). At most one
   tray process can run; a second start exits with "already running".

## Configuration

`~/.config/radiopittray/config.toml`:

```toml
api_key = "..."
api_url = "https://api.radiopit.fm"
volume = 0.8
last_station_id = null
autostart = false
```

`last_station_id` is updated on every playback and determines where
*Play stream* plays to. `autostart` writes/removes
`~/.config/autostart/radiopittray.desktop`.

## Tests

```sh
cargo test
```

## Current limitations

- The metadata connection runs 2–3 s alongside the audio stream; shortly
  after playback starts, the track title/artist may be missing for a while.
- Stations without ICY metadata only show the station title.
- The station icon is rendered at 32×32; quality depends on what the server
  serves.
