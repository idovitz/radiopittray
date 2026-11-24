# AGENTS.md

Rust system-tray radio player for your RadioPit.fm library
(tray-icon + GStreamer; the config window is built with eframe/egui).
See `README.md` for human-facing docs (installation, usage, configuration).

## Setup

- System deps: GTK 3/GLib, GStreamer (base + good plugin sets + libav),
  a StatusNotifierItem tray backend (e.g. libayatana-appindicator),
  and `xdg-open`. The build links against these; without them it fails
  at the link step.

## Build & test

- Build: `cargo build --release` (binary in `target/release/radiopittray`).
- Test: `cargo test` — keep the suite green before finishing a change.

## Architecture

- One binary, two modes: `radiopittray` runs the tray process (no window);
  `radiopittray --config` runs the config window as a separate process.
  The two only communicate through `~/.config/radiopittray/config.toml` —
  the tray process watches its mtime and reloads (`src/tray.rs`).
  Don't introduce direct IPC between them.
- Audio: GStreamer on a dedicated thread (`src/player.rs`); ICY stream
  metadata on a tokio task (`src/icy.rs`). Don't casually restructure the
  thread/task split.
- Tray: StatusNotifierItem via libappindicator (`src/tray.rs`, `src/menu.rs`).
  The tray icon shows the playing station's icon, otherwise the drawn
  RadioPit logo (`src/icon.rs`).
- Remote control ("Playing on…", `src/remote.rs`): the tray registers itself
  on the account's WebSocket as a player and can also steer the account's
  other online players from the menu. This is an **external** connection to
  the RadioPit server (tokio-tungstenite over wss, one task on the tokio
  runtime) — **not** IPC between the two processes; the config.toml-only rule
  above is unchanged. Device identity is a `device_id` persisted in
  config.toml. The server only accepts `kind` "web"/"android", so we
  register as `KIND = "web"` (flip that one const if the backend later
  allows "tray"). The `allow_remote_control` config option (default on)
  blocks the *incoming* direction by registering non-playable (`canPlay:
  false`, so the server stops routing commands to us) and dropping inbound
  commands; outgoing control is unaffected.
- Both modes hold a `flock` lock; a second start exits with
  "already running".

## Conventions

- Rust, edition 2021; `anyhow` for errors; `tracing` for logs
  (override via `RUST_LOG`, default `radiopittray=info,warn`).
- Doc comments and the README are in English. Follow each file's existing
  style.
- `tray-icon` is intentionally built without default features (no `libxdo`)
  — don't re-enable them unless truly needed.

## Security

- The RadioPit API key lives in `~/.config/radiopittray/config.toml` only.
  Never commit it or embed it in code.
