//! Desktop notifications for new tracks.
//!
//! Uses the XDG Desktop Notifications D-Bus interface
//! (`org.freedesktop.Notifications`, via notify-rust): every desktop
//! environment with a notification daemon — GNOME, Plasma, XFCE, … —
//! shows it in the standard way.
//!
//! The image is the track's album art when the `album_art` flag is
//! set: the same lookup the RadioPit web/Android apps do — a request
//! to Apple's public iTunes Search API with the track title — with
//! the station icon as fallback, and the gray note placeholder
//! (`icon::note_placeholder`) as the last resort. The art stage
//! (lookup + download) runs under a short budget so a slow lookup
//! never holds the notification back.
//!
//! Best effort: without a session bus / notification daemon the call
//! fails, is logged and ignored; the radio keeps playing.

use std::time::Duration;

use crate::artwork;
use crate::icon;
use notify_rust::Image;

/// What to put in the notification for a new track.
#[derive(Debug, Clone)]
pub struct NowPlaying {
    /// Name of the station that is playing.
    pub station: Option<String>,
    /// Artist, if the stream provides one.
    pub artist: Option<String>,
    /// Track title.
    pub title: String,
    /// URL of the station icon, if any.
    pub icon_url: Option<String>,
    /// Look up the track's album art for the notification image
    /// (iTunes Search API); when off the station icon is used.
    pub album_art: bool,
    /// How long the notification stays up, in seconds (0 = until
    /// dismissed).
    pub duration_secs: u32,
}

/// Size of the station icon inside the notification.
const ICON_SIZE: u32 = 128;
/// Size of the album art inside the notification. The art is fetched
/// at 300 px, so 256 stays sharp on HiDPI without upsampling.
const ART_SIZE: u32 = 256;
/// Total budget for the album-art stage (iTunes lookup + artwork
/// download): when it is spent, the station icon shows instead.
const ART_BUDGET: Duration = Duration::from_secs(3);

/// Show a notification for a new track.
///
/// Layout: the image on top (the track's album art when available,
/// otherwise the station icon, otherwise the gray note placeholder),
/// then the station name, then the title and the artist on their own
/// lines (title first, like the tray menu). A missing artist simply
/// drops out. Never fails the caller: without a desktop / session bus
/// the notification is skipped and logged.
pub async fn show(np: NowPlaying) {
    let (summary, body) = texts(&np);
    let mut n = notify_rust::Notification::new();
    n.appname("radiopittray");
    n.summary(&summary);
    n.body(&body);
    // De serverstandaard (~5 s) is te kort om icoon + station + titel +
    // artiest te lezen; 0 betekent "blijven totdat de gebruiker de
    // melding zelf afhandelt".
    n.timeout(notify_rust::Timeout::Milliseconds(np.duration_secs * 1000));
    // Image: the album art first (a lookup + download, so budgeted),
    // the station icon as fallback, and the gray note placeholder as
    // the last resort — the notification always carries an image.
    let mut img = if np.album_art {
        let q = artwork::art_query(np.artist.as_deref(), &np.title);
        // A slow lookup must not hold the notification back.
        tokio::time::timeout(ART_BUDGET, async {
            let url = artwork::artwork_url(&q).await?;
            artwork::thumb(&url, ART_SIZE).await
        })
        .await
        .unwrap_or_default()
    } else {
        None
    };
    if img.is_none() {
        img = station_image(np.icon_url.as_deref()).await;
    }
    if img.is_none() {
        img = Some(icon::note_placeholder(ART_SIZE));
    }
    if let Some(img) = img {
        if let Ok(i) = Image::try_from(img) {
            n.image_data(i);
        }
    }
    // Async variant: `show()` roept zbus' `block_on` aan, wat panict als
    // het vanuit een lopende tokio-taak komt (waardoor ook nog de lazy
    // runtime vergiftigd raakt en alle latere meldingen sterven).
    if let Err(e) = n.show_async().await {
        tracing::debug!("desktop notification: {e}");
    }
}

/// The station icon for the notification, if the URL is known and the
/// fetch + decode succeeds.
async fn station_image(url: Option<&str>) -> Option<image::RgbaImage> {
    let bytes = icon::fetch_icon_bytes(url?).await.ok()?;
    let img = icon::load_icon_image(&bytes)?;
    Some(icon::resize_square(&img, ICON_SIZE))
}

/// The (summary, body) texts of the notification: the station name is
/// the heading, the title and the artist each get their own line in the
/// body — title first, like the tray menu's now-playing block.
pub fn texts(np: &NowPlaying) -> (String, String) {
    let mut body = Vec::new();
    body.push(np.title.clone());
    if let Some(a) = &np.artist {
        body.push(a.clone());
    }
    let summary = np.station.clone().unwrap_or_else(|| np.title.clone());
    (summary, body.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn texts_station_artist_title() {
        let np = NowPlaying {
            station: Some("Radio 2".into()),
            artist: Some("Daft Punk".into()),
            title: "One More Time".into(),
            icon_url: None,
            album_art: false,
            duration_secs: 10,
        };
        assert_eq!(
            texts(&np),
            ("Radio 2".to_string(), "One More Time\nDaft Punk".to_string())
        );
    }

    #[test]
    fn texts_without_artist() {
        let np = NowPlaying {
            station: Some("Radio 2".into()),
            artist: None,
            title: "Solo".into(),
            icon_url: None,
            album_art: false,
            duration_secs: 10,
        };
        assert_eq!(texts(&np), ("Radio 2".to_string(), "Solo".to_string()));
    }

    #[test]
    fn texts_without_station_falls_back_to_title() {
        let np = NowPlaying {
            station: None,
            artist: Some("X".into()),
            title: "Y".into(),
            icon_url: None,
            album_art: false,
            duration_secs: 10,
        };
        assert_eq!(texts(&np), ("Y".to_string(), "Y\nX".to_string()));
    }
}
