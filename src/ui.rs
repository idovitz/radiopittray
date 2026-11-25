//! The tray window: opened with a left click on the tray icon (or
//! *Open main window* in the right-click menu).
//!
//! Top: large album art plus the now-playing info and the transport
//! controls. Below that a "Playing on…" row when the account has other
//! online players, and at the bottom a row of secondary actions.
//!
//! The rest is split over two tabs:
//! - **Stations**: the library as collapsible playlist sections, one row
//!   per station with its icon; clicking a row plays it (or steers the
//!   controlled remote to it). A search field filters by name.
//! - **History**: one row per track with a small album-art thumbnail,
//!   as before.
//!
//! All artwork comes from the shared thumbnail cache in `artwork`: the
//! UI only *reads* it synchronously ([`artwork::peek_thumb`]) and uploads
//! it as a GPU texture; the actual fetching happens on the tray tick's
//! background tasks (`artwork_tick` in `tray.rs`). The now-playing art
//! falls back from album art to the current station's icon, and a
//! missing thumbnail (or a station without an icon) shows the procedural
//! logo.
//!
//! The window has no direct access to the player, API or WebSocket: every
//! action goes through `WindowCtl` (`want_play`/`want_stop`/`WindowReq`),
//! and the tick thread performs it.

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use egui::{ColorImage, TextureHandle, TextureOptions};
use egui_extras::{Column, TableBuilder};

use crate::tray::{WindowCtl, WindowReq};
use crate::{artwork, history::HistoryEntry, icon, state::Shared};

/// Size (px) of the now-playing album art.
const ART_SIZE: u32 = 192;
/// Size (px) of a history-table thumbnail and of a station icon.
const THUMB_SIZE: u32 = 48;
/// Row height in the history table.
const ROW_HEIGHT: f32 = 40.0;
/// Station icon size as drawn in the station list (texture is THUMB_SIZE,
/// so it stays sharp on HiDPI).
const STATION_ICON: f32 = 28.0;

/// Which tab the window shows.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Stations,
    History,
}

/// The eframe application behind the tray window.
pub struct MainWindow {
    shared: Arc<Mutex<Shared>>,
    /// Window-visibility handshake with the tray tick thread.
    ctl: Arc<WindowCtl>,
    /// Textures already uploaded, keyed by `url#size`.
    textures: HashMap<String, TextureHandle>,
    /// The procedural RadioPit logo, cached once and reused as the generic
    /// stand-in for stations without (yet) an icon.
    generic_tex: Option<TextureHandle>,
    /// The "no album art" placeholder (gray square with a note), cached
    /// once and reused in the history table.
    note_tex: Option<TextureHandle>,
    /// Which tab is active; survives repaints, resets when the window
    /// loop itself is recreated.
    tab: Tab,
    /// Selected playlist sub-tab in the Stations tab (index into
    /// `UiSnapshot::sections`; clamped when the library changes).
    station_section: usize,
    /// Search filter for the station list.
    filter: String,
}

impl MainWindow {
    pub fn new(shared: Arc<Mutex<Shared>>, ctl: Arc<WindowCtl>) -> Self {
        Self {
            shared,
            ctl,
            textures: HashMap::new(),
            generic_tex: None,
            note_tex: None,
            tab: Tab::Stations,
            station_section: 0,
            filter: String::new(),
        }
    }

    /// Upload a cached thumbnail for `url` at `size` and return its
    /// texture handle, loading it into the GPU on first use. Returns
    /// `None` when the thumbnail is not (yet) available.
    fn texture(
        &mut self,
        ctx: &egui::Context,
        url: &str,
        size: u32,
    ) -> Option<TextureHandle> {
        let key = format!("{url}#{size}");
        if let Some(tex) = self.textures.get(&key) {
            return Some(tex.clone());
        }
        let img = artwork::peek_thumb(url, size)?;
        let image = ColorImage::from_rgba_unmultiplied([img.width() as usize, img.height() as usize], img.as_raw());
        let tex = ctx.load_texture(key.clone(), image, TextureOptions::LINEAR);
        self.textures.insert(key, tex.clone());
        Some(tex)
    }

    /// The procedural RadioPit logo as a texture, uploaded once and cached.
    /// Shown for stations without an icon, and while their icon is still
    /// loading (so the rows never show a blank gap).
    fn generic_texture(&mut self, ctx: &egui::Context) -> TextureHandle {
        if let Some(tex) = &self.generic_tex {
            return tex.clone();
        }
        let rgba = icon::tray_icon_rgba(false);
        let img = ColorImage::from_rgba_unmultiplied(
            [icon::ICON_SIZE as usize, icon::ICON_SIZE as usize],
            &rgba,
        );
        let tex = ctx.load_texture("generic-logo", img, TextureOptions::NEAREST);
        self.generic_tex = Some(tex.clone());
        tex
    }

    /// The "no album art" placeholder as a texture, uploaded once and
    /// cached. Drawn at 4× the row size so it stays sharp on HiDPI.
    fn note_texture(&mut self, ctx: &egui::Context) -> TextureHandle {
        if let Some(tex) = &self.note_tex {
            return tex.clone();
        }
        let size = THUMB_SIZE * 4;
        let img = icon::note_placeholder(size);
        let image = ColorImage::from_rgba_unmultiplied([size as usize, size as usize], img.as_raw());
        let tex = ctx.load_texture("note-placeholder", image, TextureOptions::LINEAR);
        self.note_tex = Some(tex.clone());
        tex
    }

    /// Transport glyphs, bottom-aligned against the lower edge of the art.
    /// While a remote is controlled, they steer it; otherwise this tray's
    /// own player. Borderless clickable labels: no frame, no outline;
    /// dimmed grey and inert when the action does not apply.
    fn transport(&mut self, ui: &mut egui::Ui, snapshot: &crate::state::UiSnapshot) {
        let icon = |ui: &mut egui::Ui, glyph: &str, enabled: bool| -> bool {
            let color = if enabled {
                egui::Color32::WHITE
            } else {
                egui::Color32::from_gray(110)
            };
            let clicked = ui
                .add(
                    egui::Label::new(
                        egui::RichText::new(glyph).size(52.0).color(color).strong(),
                    )
                    .sense(egui::Sense::click()),
                )
                .on_hover_cursor(egui::CursorIcon::PointingHand)
                .clicked();
            clicked && enabled
        };
        let remote = snapshot.remote_target.is_some();
        // `right_to_left` + `Align::Max` pins the row's bottom to the
        // bottom of the (taller) art, and Stop-before-Play still reads
        // "Play  Stop" from left to right.
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Max), |ui| {
            if icon(ui, "⏹", snapshot.can_stop) {
                if remote {
                    self.ctl.request(WindowReq::RemotePlayPause);
                } else {
                    self.ctl.want_stop.store(true, Ordering::Relaxed);
                }
            }
            ui.add_space(14.0);
            if icon(ui, "▶", snapshot.can_play) {
                if remote {
                    self.ctl.request(WindowReq::RemotePlayPause);
                } else {
                    self.ctl.want_play.store(true, Ordering::Relaxed);
                }
            }
        });
    }

    /// "Playing on…" selector: switch between local playback and the
    /// account's other online players, with volume stepping while one is
    /// selected. Only drawn when the remote-control link is up.
    fn remote_row(&mut self, ui: &mut egui::Ui, snapshot: &crate::state::UiSnapshot) {
        let label = match &snapshot.remote_target {
            None => "This device".to_string(),
            Some(id) => snapshot
                .remote_devices
                .iter()
                .find(|d| d.id == *id)
                .map(|d| format!("{} — {}", d.name, d.state_text))
                .unwrap_or_else(|| "This device".to_string()),
        };
        // The combo selects against a local clone of the target; when the
        // selection differs from the shared state, push it to the tick
        // thread (the window never writes `Shared` itself). The item click
        // must be caught inside the closure: `show_ui` reports the button's
        // response only, which changes when the dropdown opens — a frame
        // before any item can be selected.
        let mut new_target = snapshot.remote_target.clone();
        let mut target_changed = false;
        let _ = egui::ComboBox::from_id_salt("playing-on")
            .selected_text(format!("Playing on: {label}"))
            .show_ui(ui, |ui| {
                if ui.selectable_value(&mut new_target, None, "This device").clicked() {
                    target_changed = true;
                }
                for d in &snapshot.remote_devices {
                    if ui
                        .selectable_value(
                            &mut new_target,
                            Some(d.id.clone()),
                            format!("{} — {}", d.name, d.state_text),
                        )
                        .clicked()
                    {
                        target_changed = true;
                    }
                }
            });
        if target_changed && new_target != snapshot.remote_target {
            self.ctl.request(WindowReq::SetRemote(new_target));
        }
        if snapshot.remote_target.is_some() {
            let clicked = ui
                .add(
                    egui::Label::new(egui::RichText::new("−").size(20.0).strong())
                        .sense(egui::Sense::click()),
                )
                .on_hover_cursor(egui::CursorIcon::PointingHand)
                .clicked();
            if clicked {
                self.ctl.request(WindowReq::RemoteVolume(-10));
            }
            let vol = snapshot
                .remote_volume
                .map(|v| format!("{v}"))
                .unwrap_or_else(|| "–".to_string());
            ui.label(vol);
            let clicked = ui
                .add(
                    egui::Label::new(egui::RichText::new("+").size(20.0).strong())
                        .sense(egui::Sense::click()),
                )
                .on_hover_cursor(egui::CursorIcon::PointingHand)
                .clicked();
            if clicked {
                self.ctl.request(WindowReq::RemoteVolume(10));
            }
        }
    }

    /// The Stations tab: playlist categories as horizontally scrollable
    /// sub-tabs, with the selected category's stations in a vertically
    /// scrollable icon list below, and a search filter on top.
    fn stations_tab(&mut self, ctx: &egui::Context, ui: &mut egui::Ui, snapshot: &crate::state::UiSnapshot) {
        // The playlist sub-tab strip.
        let mut selected = self.station_section;
        if selected >= snapshot.sections.len() {
            selected = 0;
        }
        self.station_section = selected;
        if snapshot.sections.len() > 1 {
            // NB: `ScrollArea::horizontal()` only enables horizontal
            // scrolling; it does not change the layout direction, so the
            // tabs must be placed in an explicit horizontal layout. The
            // (default) shrink-to-content sizing keeps the strip one line
            // tall; horizontal scrolling kicks in when the tabs overflow.
            egui::ScrollArea::horizontal()
                .id_salt("stations-subtabs")
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        for (i, section) in snapshot.sections.iter().enumerate() {
                            if ui
                                .selectable_label(i == selected, &section.name)
                                .clicked()
                            {
                                selected = i;
                                self.station_section = i;
                            }
                        }
                    });
                });
            ui.separator();
        }

        // Filter + the selected category's station list.
        let filter = self.filter.trim().to_lowercase();
        ui.add(
            egui::TextEdit::singleline(&mut self.filter)
                .hint_text("Filter stations…")
                .desired_width(ui.available_width().min(280.0)),
        );

        match snapshot.sections.get(selected) {
            None => {
                if let Some(note) = &snapshot.library_note {
                    ui.label(note);
                }
            }
            Some(section) => {
                let stations: Vec<_> = if filter.is_empty() {
                    section.stations.iter().collect()
                } else {
                    section
                        .stations
                        .iter()
                        .filter(|s| s.name.to_lowercase().contains(&filter))
                        .collect()
                };
                if stations.is_empty() {
                    ui.label("No stations match the filter.");
                    return;
                }
                // Vertically scrollable list filling the remaining space.
                egui::ScrollArea::vertical()
                    .id_salt("stations-list")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        for st in stations {
                            let row = ui
                                .horizontal(|ui| {
                                    let icon_tex = st
                                        .icon_url
                                        .as_deref()
                                        .and_then(|url| self.texture(ctx, url, THUMB_SIZE));
                                    let icon_tex = match icon_tex {
                                        Some(t) => t,
                                        // No own icon (or still loading):
                                        // the generic RadioPit logo.
                                        None => self.generic_texture(ctx),
                                    };
                                    ui.add(
                                        egui::Image::new(&icon_tex)
                                            .max_size(egui::vec2(STATION_ICON, STATION_ICON)),
                                    );
                                    // A full-width selectable label: the
                                    // whole row is clickable, and the
                                    // currently playing station is
                                    // highlighted.
                                    let text = egui::RichText::new(&st.name);
                                    let text = if st.is_current {
                                        text.strong()
                                    } else {
                                        text
                                    };
                                    ui.add(egui::SelectableLabel::new(st.is_current, text))
                                })
                                .inner;
                            if row.clicked() {
                                self.ctl.request(WindowReq::PlayStation(st.id.clone()));
                            }
                            ui.separator();
                        }
                    });
            }
        }
    }

    /// The History tab: the track table, as before.
    fn history_tab(&mut self, ctx: &egui::Context, ui: &mut egui::Ui, snapshot: &crate::state::UiSnapshot) {
        if snapshot.history.is_empty() {
            ui.label("Nothing has played yet.");
            return;
        }
        let rows = snapshot.history.len();
        let history = &snapshot.history;
        // No header row and no resizable columns: the only thing
        // separating rows is the striping (alternating colours),
        // with no grid lines between cells.
        TableBuilder::new(ui)
            .striped(true)
            .column(Column::exact(THUMB_SIZE as f32 + 10.0))
            .column(Column::exact(150.0))
            .column(Column::remainder().at_least(120.0))
            .column(Column::exact(72.0))
            .body(|body| {
                body.rows(ROW_HEIGHT, rows, |mut row| {
                    let e = &history[row.index()];
                    // Thumbnail; no album art (or not fetched yet): the
                    // gray note placeholder.
                    row.col(|ui| {
                        let tex = e
                            .artwork_url
                            .as_deref()
                            .and_then(|u| self.texture(ctx, u, THUMB_SIZE))
                            .unwrap_or_else(|| self.note_texture(ctx));
                        ui.add(
                            egui::Image::new(&tex)
                                .max_size(egui::vec2(THUMB_SIZE as f32, THUMB_SIZE as f32)),
                        );
                    });
                    // Station.
                    row.col(|ui| {
                        ui.label(&e.station);
                    });
                    // Track: title + artist on one line.
                    row.col(|ui| {
                        let label = match &e.artist {
                            Some(a) => format!("{} — {}", a, e.title),
                            None => e.title.clone(),
                        };
                        ui.label(label);
                    });
                    // Time.
                    row.col(|ui| {
                        ui.label(format_time(e));
                    });
                });
            });
    }
}

impl eframe::App for MainWindow {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // The close button must hide the window, not quit the tray: cancel
        // the close eframe is about to do and hide instead, so the process
        // keeps running and the next tray click can show it again.
        if ctx.input(|i| i.viewport().close_requested()) {
            self.ctl.focused_seen.store(false, Ordering::Relaxed);
            self.ctl.want_visible.store(false, Ordering::Relaxed);
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        }

        // Focus-loss dismisses the window, like a real tray popup (click
        // anywhere outside to close). Only acted on while a window is wanted,
        // and only after it has actually been focused once (see
        // `WindowCtl::focused_seen`), so the first frames after showing —
        // before the WM has handed over focus — do not count as a focus loss.
        if self.ctl.want_visible.load(Ordering::Relaxed) {
            let focused = ctx.input(|i| i.focused);
            if focused {
                self.ctl.focused_seen.store(true, Ordering::Relaxed);
            } else if self.ctl.focused_seen.load(Ordering::Relaxed) {
                self.ctl.focused_seen.store(false, Ordering::Relaxed);
                self.ctl.want_visible.store(false, Ordering::Relaxed);
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            }
        }

        // On the first visible frame after a left click, move the window
        // under the tray icon. The click position is in physical pixels; egui
        // wants logical points, so divide by the viewport scale and nudge a
        // little below the top of the icon. Doing this here (not while hidden)
        // is what keeps the window manager from centring the window.
        if self.ctl.needs_place.swap(false, Ordering::Relaxed) {
            if let Some(phys) = self.ctl.pos() {
                let ppb = ctx.pixels_per_point().max(1.0);
                let pos = egui::pos2(phys[0] / ppb, phys[1] / ppb + 6.0);
                ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(pos));
            }
        }

        // Snapshot the state so we never hold the shared mutex while egui
        // draws or the user interacts with it.
        let snapshot = self.shared.lock().unwrap().clone_snapshot();

        // -- Now playing -----------------------------------------------------
        // Fallback chain: album art → the current station's icon → the
        // procedural RadioPit logo (the `None` arm below). The newest
        // history entry may still be the *previous* station's track while
        // the new station is loading (its first ICY title is not recorded
        // yet), so the art only counts when it belongs to the playing
        // station — otherwise the old station's album art would linger.
        // All of this only while the stream is active: when it is stopped,
        // the last played station is remembered (for resuming) but its
        // art and track info are stale and are not shown.
        let live = snapshot.live;
        let art = if live {
            snapshot
                .history
                .first()
                .filter(|e| Some(e.station.as_str()) == snapshot.station.as_deref())
                .and_then(|e| e.artwork_url.clone())
        } else {
            None
        };
        let art_tex = art
            .as_deref()
            .and_then(|u| self.texture(ctx, u, ART_SIZE))
            .or_else(|| {
                snapshot
                    .station_icon_url
                    .as_deref()
                    .and_then(|u| self.texture(ctx, u, ART_SIZE))
            });
        egui::TopBottomPanel::top("now-playing").show(ctx, |ui| {
            ui.add_space(10.0);
            let side = ART_SIZE as f32;
            ui.horizontal(|ui| {
                match &art_tex {
                    Some(tex) => {
                        ui.add(egui::Image::new(tex).max_size(egui::vec2(side, side)));
                    }
                    None => {
                        // No album art and no station icon (yet): the
                        // procedural logo as a placeholder.
                        let rgba = icon::tray_icon_rgba(snapshot.playing);
                        let img = ColorImage::from_rgba_unmultiplied(
                            [icon::ICON_SIZE as usize, icon::ICON_SIZE as usize],
                            &rgba,
                        );
                        let tex = ctx.load_texture("logo-placeholder", img, TextureOptions::NEAREST);
                        ui.add(egui::Image::new(&tex).max_size(egui::vec2(side, side)));
                    }
                }
                ui.vertical(|ui| {
                    let station = if live {
                        snapshot.station.as_deref().unwrap_or("Not playing")
                    } else {
                        "Not playing"
                    };
                    ui.heading(station);
                    if live {
                        if let Some(t) = &snapshot.title {
                            ui.label(format!("🎵  {t}"));
                        }
                        if let Some(a) = &snapshot.artist {
                            ui.label(a);
                        }
                    }
                    ui.label(snapshot.state_text.as_str());
                    if snapshot.remote_connected {
                        ui.add_space(4.0);
                        self.remote_row(ui, &snapshot);
                    }
                });
                self.transport(ui, &snapshot);
            });
            ui.add_space(10.0);
        });

        // -- Actions -----------------------------------------------------------
        // The secondary actions that used to sit at the bottom of the tray
        // menu. Configuration and Quit live in the right-click menu.
        egui::TopBottomPanel::bottom("actions").show(ctx, |ui| {
            ui.horizontal(|ui| {
                if ui
                    .add(
                        egui::Label::new("Refresh library").sense(egui::Sense::click()),
                    )
                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                    .clicked()
                {
                    self.ctl.request(WindowReq::Refresh);
                }
                ui.separator();
                if ui
                    .add(egui::Label::new("Open radiopit.fm").sense(egui::Sense::click()))
                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                    .clicked()
                {
                    self.ctl.request(WindowReq::OpenWeb);
                }
            });
            ui.add_space(4.0);
        });

        // -- Tabs --------------------------------------------------------------
        let tab = self.tab;
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.horizontal(|ui| {
                if ui
                    .selectable_label(tab == Tab::Stations, "Stations")
                    .clicked()
                {
                    self.tab = Tab::Stations;
                }
                if ui
                    .selectable_label(tab == Tab::History, "History")
                    .clicked()
                {
                    self.tab = Tab::History;
                }
            });
            ui.separator();

            // Draw the active tab. `self` is borrowed mutably for texture
            // uploads and request pushes; the snapshot is a separate clone,
            // so this is fine.
            let snapshot = snapshot.clone();
            match self.tab {
                Tab::Stations => self.stations_tab(ctx, ui, &snapshot),
                Tab::History => self.history_tab(ctx, ui, &snapshot),
            }
        });
    }
}

use std::time::{SystemTime, UNIX_EPOCH};

/// Format a history entry's timestamp as local time: "HH:MM" for tracks
/// that started within the last day, otherwise "MM-DD".
fn format_time(e: &HistoryEntry) -> String {
    const DAY: u64 = 24 * 60 * 60;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (hh, mm, mo, da) = match local(e.ts) {
        Some(t) => t,
        None => return format!("{}", e.ts),
    };
    if now.saturating_sub(e.ts) < DAY {
        format!("{hh:02}:{mm:02}")
    } else {
        format!("{mo:02}-{da:02}")
    }
}

/// Broken-down *local* time (hour, minute, month, day) for a seconds-since-
/// epoch value, via libc's `localtime_r` so the system timezone (and DST) is
/// honoured — the plain arithmetic it replaces showed UTC instead. Returns
/// `None` only when libc rejects the timestamp.
fn local(secs: u64) -> Option<(u32, u32, u32, u32)> {
    let t = secs as libc::time_t;
    // SAFETY: `localtime_r` writes into the (zeroed) `tm` we hand it and
    // returns null only for a timestamp outside the supported range.
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&t, &mut tm).is_null() {
            return None;
        }
        Some((
            tm.tm_hour as u32,
            tm.tm_min as u32,
            (tm.tm_mon + 1) as u32,
            tm.tm_mday as u32,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recent_tracks_format_as_clock() {
        // A track that just started must read as "HH:MM" (local), regardless
        // of the system timezone.
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let e = HistoryEntry {
            station: "s".into(),
            artist: None,
            title: "t".into(),
            ts: now,
            artwork_url: None,
        };
        let s = format_time(&e);
        assert_eq!(s.chars().count(), 5, "{s}");
        assert!(s.contains(':'), "{s}");
    }
}
