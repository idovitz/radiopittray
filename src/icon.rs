//! Tray/window icons. The tray icon is the icon theme's "radio" icon —
//! the same icon the .desktop entry references — so the tray and the app
//! menu match; while playing, a green dot marks the antenna tip. The
//! procedurally drawn RadioPit logo (radio waves + dot) remains the
//! window icon and the last-resort fallback. No binary assets needed.

pub const ICON_SIZE: u32 = 64;

/// Center of the antenna tip and radius of the green ball in the theme's
/// "radio" icon (the top-left end of the diagonal antenna).
const ANTENNA_TIP: (u32, u32) = (11, 9);
const ANTENNA_DOT_R: f32 = 6.0;

/// Idle tray icon: the theme's "radio" icon, or the procedural logo when
/// the icon theme has no such icon.
pub fn idle_icon() -> tray_icon::Icon {
    theme_radio_rgba()
        .and_then(|rgba| tray_icon::Icon::from_rgba(rgba, ICON_SIZE, ICON_SIZE).ok())
        .unwrap_or_else(|| tray_icon(false))
}

/// Tray icon for a playing station without an icon of its own: the
/// theme's "radio" icon with a green dot at the tip of the antenna, or
/// the procedural logo when the icon theme has no such icon.
pub fn playing_icon() -> tray_icon::Icon {
    match theme_radio_rgba() {
        Some(rgba) => tray_icon::Icon::from_rgba(
            with_antenna_dot(rgba),
            ICON_SIZE,
            ICON_SIZE,
        )
        .unwrap_or_else(|_| tray_icon(true)),
        None => tray_icon(true),
    }
}

/// The "radio" icon from the icon theme, at `ICON_SIZE`, as RGBA.
#[cfg(target_os = "linux")]
fn theme_radio_rgba() -> Option<Vec<u8>> {
    use gtk::prelude::*;
    let theme = gtk::IconTheme::default()?;
    let pb = theme
        .load_icon("radio", ICON_SIZE as i32, gtk::IconLookupFlags::FORCE_SIZE)
        .ok()
        .flatten()?;
    pixbuf_to_rgba(&pb)
}

#[cfg(not(target_os = "linux"))]
fn theme_radio_rgba() -> Option<Vec<u8>> {
    None
}

/// Overlay a green ball at the tip of the antenna so the playing state
/// is visible at a glance (same green as the logo's dot).
fn with_antenna_dot(rgba: Vec<u8>) -> Vec<u8> {
    let mut px = rgba;
    let (cx, cy) = (ANTENNA_TIP.0 as f32, ANTENNA_TIP.1 as f32);
    let dot: [u8; 3] = [74, 222, 128];
    let r = ANTENNA_DOT_R;
    let x0 = (cx - r - 1.0).max(0.0) as u32;
    let x1 = (cx + r + 1.0).min(ICON_SIZE as f32) as u32;
    let y0 = (cy - r - 1.0).max(0.0) as u32;
    let y1 = (cy + r + 1.0).min(ICON_SIZE as f32) as u32;
    for y in y0..y1 {
        for x in x0..x1 {
            let dx = (x as f32 + 0.5) - cx;
            let dy = (y as f32 + 0.5) - cy;
            let dist = (dx * dx + dy * dy).sqrt();
            let cov = (r + 0.5 - dist).clamp(0.0, 1.0);
            if cov <= 0.0 {
                continue;
            }
            let idx = ((y * ICON_SIZE + x) * 4) as usize;
            px[idx] = (dot[0] as f32 * cov + px[idx] as f32 * (1.0 - cov)) as u8;
            px[idx + 1] = (dot[1] as f32 * cov + px[idx + 1] as f32 * (1.0 - cov)) as u8;
            px[idx + 2] = (dot[2] as f32 * cov + px[idx + 2] as f32 * (1.0 - cov)) as u8;
            px[idx + 3] = (cov * 255.0).max(px[idx + 3] as f32) as u8;
        }
    }
    px
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

/// Fetch an icon URL without the API key header.
pub async fn fetch_icon_bytes(url: &str) -> anyhow::Result<Vec<u8>> {
    let url = fetch_icon_url(url);
    let client = reqwest::Client::builder()
        .user_agent(concat!("radiopittray/", env!("CARGO_PKG_VERSION")))
        .timeout(std::time::Duration::from_secs(10))
        .build()?;
    let resp = client.get(&url).send().await?.error_for_status()?;
    Ok(resp.bytes().await?.to_vec())
}

/// Wikimedia only serves thumbnails in a fixed set of sizes (640px is not
/// one of them — it answers 400) — rewrite the requested size to 120px,
/// which is allowed and still well above the 32×32 tray icon.
pub fn fetch_icon_url(url: &str) -> String {
    let (prefix, rest) = match url.rsplit_once('/') {
        Some((p, r)) => (p, r),
        None => return url.to_string(),
    };
    let (size, file) = match rest.split_once("px-") {
        Some((s, f)) if s.parse::<u32>().is_ok() => (s, f),
        _ => return url.to_string(),
    };
    if size == "120" || !prefix.contains("wikimedia.org") || !prefix.contains("/thumb/") {
        return url.to_string();
    }
    format!("{prefix}/120px-{file}")
}

/// Fit an image into a `size`×`size` RGBA canvas, preserving the aspect
/// ratio. Station icons are not always square (and `resize` alone would
/// return e.g. 32×18 for a landscape icon, which the PNG writer then
/// rejects because the buffer no longer matches the declared size), so
/// the result is centered on a transparent canvas.
pub fn resize_square(img: &image::DynamicImage, size: u32) -> image::RgbaImage {
    let scaled = img.resize(size, size, image::imageops::FilterType::Lanczos3);
    let mut out = image::RgbaImage::new(size, size);
    let x = size.saturating_sub(scaled.width()) / 2;
    let y = size.saturating_sub(scaled.height()) / 2;
    image::imageops::overlay(&mut out, &scaled.to_rgba8(), x as i64, y as i64);
    out
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

    #[test]
    fn rewrites_wikimedia_thumbnail_size() {
        let url = "https://upload.wikimedia.org/wikipedia/commons/thumb/d/da/NPO_Radio_1_logo_2014.svg/640px-NPO_Radio_1_logo_2014.svg.png";
        assert_eq!(
            fetch_icon_url(url),
            "https://upload.wikimedia.org/wikipedia/commons/thumb/d/da/NPO_Radio_1_logo_2014.svg/120px-NPO_Radio_1_logo_2014.svg.png"
        );
    }

    #[test]
    fn leaves_other_urls_alone() {
        let url = "https://example.com/icon/640px-foo.png";
        assert_eq!(fetch_icon_url(url), url);
        let url = "https://upload.wikimedia.org/wikipedia/commons/foo.png";
        assert_eq!(fetch_icon_url(url), url);
    }

    /// Station icons are not always square (e.g. Radio Capelle's is 128×71);
    /// the result must always be an exact `size`×`size` RGBA buffer,
    /// centered.
    #[test]
    fn resizes_non_square_icons_to_full_square() {
        let img = image::DynamicImage::new_rgb8(128, 71);
        let rgba = resize_square(&img, 32);
        assert_eq!(rgba.as_raw().len(), 32 * 32 * 4);
        // Landscape icon: letterboxed, so the top row stays transparent.
        let top = &rgba.as_raw()[..32 * 4];
        assert!(top.iter().all(|&b| b == 0));
        // The middle row has content.
        let mid = &rgba.as_raw()[(16 * 32) * 4..(16 * 32 + 32) * 4];
        assert!(mid.iter().any(|&b| b > 0));
        // A square icon fills the whole canvas.
        let sq: image::DynamicImage =
            image::RgbImage::from_pixel(64, 64, image::Rgb([200, 100, 50])).into();
        let rgba = resize_square(&sq, 32);
        assert_eq!(rgba.as_raw().len(), 32 * 32 * 4);
        assert!(rgba.as_raw().iter().all(|&b| b > 0));
    }
}
