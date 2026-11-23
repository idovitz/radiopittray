//! Desktop notifications for new tracks.
//!
//! Uses the XDG Desktop Notifications D-Bus interface
//! (`org.freedesktop.Notifications`, via notify-rust): every desktop
//! environment with a notification daemon — GNOME, Plasma, XFCE, … —
//! shows it in the standard way.
//!
//! Best effort: without a session bus / notification daemon the call
//! fails, is logged and ignored; the radio keeps playing.

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
    /// How long the notification stays up, in seconds (0 = until
    /// dismissed).
    pub duration_secs: u32,
}

/// Size of the station icon inside the notification.
const ICON_SIZE: u32 = 128;

/// Show a notification for a new track.
///
/// Layout: the station icon on top, then the station name, then the
/// title and the artist on their own lines (title first, like the tray
/// menu). A missing icon or artist simply drops out. Never fails the
/// caller: without a desktop / session bus the notification is skipped
/// and logged.
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
    if let Some(img) = station_image(np.icon_url.as_deref()).await {
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
    image::load_from_memory(&bytes)
        .ok()
        .map(|im| icon::resize_square(&im, ICON_SIZE))
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
            duration_secs: 10,
        };
        assert_eq!(texts(&np), ("Y".to_string(), "Y\nX".to_string()));
    }
}
