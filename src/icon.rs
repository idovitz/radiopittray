//! Tray/window icons. The tray icon is the procedurally drawn RadioPit
//! logo (radio waves + dot): the same icon the window shows, so tray and
//! window match. While playing, the waves brighten and the dot turns
//! green. No binary assets and no icon-theme lookup needed.

pub const ICON_SIZE: u32 = 64;

/// Idle tray icon: the procedural RadioPit logo.
pub fn idle_icon() -> tray_icon::Icon {
    tray_icon(false)
}

/// Tray icon while playing: the procedural logo, brightened with a green
/// dot.
pub fn playing_icon() -> tray_icon::Icon {
    tray_icon(true)
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

/// Upper bound for a station icon download. Icons are 32×32 in the tray
/// and 128×128 in notifications; a bigger payload is a mistake (or an
/// attack) and is cut off before it can exhaust memory.
pub const MAX_ICON_BYTES: usize = 8 * 1024 * 1024;

/// Upper bound for the decoded pixel count: a compressed image can
/// expand far beyond its download size (decompression bomb).
pub const MAX_ICON_PIXELS: u32 = 4096 * 4096;

/// Fetch an icon URL without the API key header. The URL is validated
/// (public http(s) only, see `crate::urls`) and the download is capped
/// at `MAX_ICON_BYTES`.
pub async fn fetch_icon_bytes(url: &str) -> anyhow::Result<Vec<u8>> {
    let url = fetch_icon_url(url);
    crate::urls::validate(&url)?;
    let client = reqwest::Client::builder()
        .user_agent(concat!("radiopittray/", env!("CARGO_PKG_VERSION")))
        .timeout(std::time::Duration::from_secs(10))
        .redirect(crate::urls::safe_redirect_policy())
        .build()?;
    let mut resp = client.get(&url).send().await?.error_for_status()?;
    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = resp.chunk().await? {
        buf.extend_from_slice(&chunk);
        if buf.len() > MAX_ICON_BYTES {
            anyhow::bail!("icon download is larger than {MAX_ICON_BYTES} bytes; ignoring it");
        }
    }
    Ok(buf)
}

/// Decode fetched icon bytes, refusing images whose pixel count exceeds
/// `MAX_ICON_PIXELS`. Used by both the tray icon and the notification
/// image, so the size cap applies to every decode of remote bytes.
pub fn load_icon_image(bytes: &[u8]) -> Option<image::DynamicImage> {
    let img = image::load_from_memory(bytes).ok()?;
    let pixels = img.width().saturating_mul(img.height());
    if pixels > MAX_ICON_PIXELS {
        tracing::warn!(
            "icon image too large: {}x{} (> {MAX_ICON_PIXELS} pixels)",
            img.width(),
            img.height()
        );
        return None;
    }
    Some(img)
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

/// The "no album art" placeholder: a gray square with a musical note.
/// Shown in the history table and as the last-resort notification
/// image. Drawn at 2× and downscaled, so the edges come out
/// antialiased.
pub fn note_placeholder(size: u32) -> image::RgbaImage {
    let s = size * 2;
    let big = {
        let mut img = image::RgbaImage::from_pixel(s, s, image::Rgba([48, 51, 58, 255]));
        let note = image::Rgba([172, 178, 192, 255]);
        // A quarter note: a tilted oval head with a stem rising from its
        // right side (coordinates as fractions of the canvas).
        let cx = 0.40 * s as f32;
        let cy = 0.62 * s as f32;
        let rx = 0.17 * s as f32;
        let ry = 0.12 * s as f32;
        let (sin_t, cos_t) = (0.35f32.sin(), 0.35f32.cos());
        let stem_w = 0.05 * s as f32;
        let stem_x = cx + rx * cos_t;
        let stem_top = 0.18 * s as f32;
        for y in 0..s {
            for x in 0..s {
                let (x, y) = (x as f32 + 0.5, y as f32 + 0.5);
                // Head: the point, rotated back, inside the axis-aligned
                // ellipse.
                let dx = x - cx;
                let dy = y - cy;
                let xr = dx * cos_t - dy * sin_t;
                let yr = dx * sin_t + dy * cos_t;
                let in_head = (xr / rx) * (xr / rx) + (yr / ry) * (yr / ry) <= 1.0;
                let in_stem = (x - stem_x).abs() <= stem_w / 2.0 && (stem_top..=cy + ry).contains(&y);
                if in_head || in_stem {
                    img.put_pixel(x as u32, y as u32, note);
                }
            }
        }
        img
    };
    image::DynamicImage::ImageRgba8(big)
        .resize(size, size, image::imageops::FilterType::Lanczos3)
        .to_rgba8()
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

    #[test]
    fn note_placeholder_is_a_gray_square_with_a_note() {
        let img = note_placeholder(64);
        assert_eq!((img.width(), img.height()), (64, 64));
        // The corners are the plain gray background.
        assert_eq!(img.get_pixel(2, 2), &image::Rgba([48, 51, 58, 255]));
        // The note is a lighter grey and actually present.
        assert!(img.pixels().any(|p| p[0] > 120 && p[1] > 120 && p[2] > 120));
    }

    #[test]
    fn loads_small_and_refuses_oversized_decoded_images() {
        use std::io::Cursor;

        fn png_bytes(w: u32, h: u32) -> Vec<u8> {
            let img: image::RgbaImage =
                image::RgbaImage::from_pixel(w, h, image::Rgba([255, 0, 0, 255]));
            let mut buf: Vec<u8> = Vec::new();
            image::DynamicImage::ImageRgba8(img)
                .write_to(&mut Cursor::new(&mut buf), image::ImageFormat::Png)
                .unwrap();
            buf
        }

        assert!(load_icon_image(&png_bytes(16, 16)).is_some());
        // 4097×4097 exceeds MAX_ICON_PIXELS (4096²): a decompression
        // bomb must be refused, not decoded into gigabytes.
        assert!(load_icon_image(&png_bytes(4097, 4097)).is_none());
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
