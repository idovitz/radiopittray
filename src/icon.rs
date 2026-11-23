//! Tray/window icons. The idle tray icon is the icon theme's "radio"
//! icon — the same icon the .desktop entry references — so the tray and
//! the app menu match. Everything else (window icon, playing fallback)
//! is the procedurally drawn RadioPit logo (radio waves + dot). No
//! binary assets needed.

pub const ICON_SIZE: u32 = 64;

/// Idle tray icon: the theme's "radio" icon, or the procedural logo when
/// the icon theme has no such icon.
pub fn idle_icon() -> tray_icon::Icon {
    theme_radio_icon().unwrap_or_else(|| tray_icon(false))
}

/// The "radio" icon from the icon theme, at `ICON_SIZE`.
#[cfg(target_os = "linux")]
fn theme_radio_icon() -> Option<tray_icon::Icon> {
    use gtk::prelude::*;
    let theme = gtk::IconTheme::default()?;
    let pb = theme
        .load_icon("radio", ICON_SIZE as i32, gtk::IconLookupFlags::FORCE_SIZE)
        .ok()
        .flatten()?;
    let rgba = pixbuf_to_rgba(&pb)?;
    tray_icon::Icon::from_rgba(rgba, ICON_SIZE, ICON_SIZE).ok()
}

#[cfg(not(target_os = "linux"))]
fn theme_radio_icon() -> Option<tray_icon::Icon> {
    None
}

/// Copy a pixbuf (RGB or RGBA, arbitrary rowstride) to tightly packed
/// RGBA8.
#[cfg(target_os = "linux")]
fn pixbuf_to_rgba(pb: &gtk::gdk_pixbuf::Pixbuf) -> Option<Vec<u8>> {
    let w = pb.width() as usize;
    let h = pb.height() as usize;
    let stride = pb.rowstride() as usize;
    let channels = pb.n_channels() as usize;
    if w == 0 || h == 0 || (channels != 3 && channels != 4) {
        return None;
    }
    let px = pb.read_pixel_bytes();
    let mut rgba = vec![0u8; w * h * 4];
    for y in 0..h {
        let src = y * stride;
        let dst = y * w * 4;
        for x in 0..w {
            let s = src + x * channels;
            let d = dst + x * 4;
            rgba[d] = px[s];
            rgba[d + 1] = px[s + 1];
            rgba[d + 2] = px[s + 2];
            rgba[d + 3] = if pb.has_alpha() { px[s + 3] } else { 255 };
        }
    }
    Some(rgba)
}

/// Draws the RadioPit logo as RGBA.
///
/// `playing` switches the dot to green and brightens the waves.
pub fn tray_icon_rgba(playing: bool) -> Vec<u8> {
    const S: u32 = ICON_SIZE;
    let cx = 32.0f32;
    let cy = 42.0f32;
    let dot_r = 4.5f32;
    let rings: [f32; 3] = [12.0, 20.0, 28.0];
    let thickness = 4.0f32;

    let wave: [u8; 3] = if playing { [255, 255, 255] } else { [170, 178, 194] };
    let dot: [u8; 3] = if playing { [74, 222, 128] } else { [100, 108, 124] };

    let mut px = vec![0u8; (S * S * 4) as usize];

    for y in 0..S {
        for x in 0..S {
            let dx = (x as f32 + 0.5) - cx;
            let dy = (y as f32 + 0.5) - cy;
            let r = (dx * dx + dy * dy).sqrt();

            let idx = ((y * S + x) * 4) as usize;

            if r <= dot_r + 0.5 {
                // Center dot.
                let cov = (dot_r + 0.5 - r).clamp(0.0, 1.0);
                px[idx] = dot[0];
                px[idx + 1] = dot[1];
                px[idx + 2] = dot[2];
                px[idx + 3] = (cov * 255.0) as u8;
            } else if dy <= 0.0 {
                // Upper semicircle waves only.
                let mut cov = 0.0f32;
                for &rr in &rings {
                    cov = cov.max((thickness / 2.0 + 0.5 - (r - rr).abs()).clamp(0.0, 1.0));
                }
                if cov > 0.0 {
                    px[idx] = wave[0];
                    px[idx + 1] = wave[1];
                    px[idx + 2] = wave[2];
                    px[idx + 3] = (cov * 255.0) as u8;
                }
            }
        }
    }

    px
}

/// egui icon data for the window.
pub fn egui_icon(playing: bool) -> egui::IconData {
    let rgba = tray_icon_rgba(playing);
    egui::IconData {
        rgba,
        width: ICON_SIZE,
        height: ICON_SIZE,
    }
}

/// tray-icon crate icon.
pub fn tray_icon(playing: bool) -> tray_icon::Icon {
    tray_icon::Icon::from_rgba(tray_icon_rgba(playing), ICON_SIZE, ICON_SIZE)
        .expect("valid rgba icon")
}

/// Tray icon from raw 32×32 RGBA data (a station icon).
pub fn tray_icon_from_rgba(rgba: Vec<u8>) -> Option<tray_icon::Icon> {
    tray_icon::Icon::from_rgba(rgba, 32, 32).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icon_has_expected_size_and_content() {
        let px = tray_icon_rgba(false);
        assert_eq!(px.len(), (ICON_SIZE * ICON_SIZE * 4) as usize);
        let opaque = px.chunks(4).filter(|p| p[3] > 0).count();
        assert!(opaque > 100, "icon should not be empty, got {opaque} px");
        // Dot area should be opaque at the center.
        let center = ((42 * ICON_SIZE + 32) * 4) as usize;
        assert!(px[center + 3] > 0);
    }
}
