//! Persisted playback history.
//!
//! Every track that starts playing is appended to a bounded log in the
//! config directory, so the tray window can show a "history" list that
//! survives restarts. Only plain, non-secret data is stored (station,
//! artist, title, time and the resolved artwork URL) — never the API key.
//!
//! Newest first: the log is kept in display order so the UI can render
//! it straight away.

use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::config;

/// Maximum number of entries kept; older ones are dropped.
pub const MAX_ENTRIES: usize = 200;

/// One played track.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HistoryEntry {
    pub station: String,
    #[serde(default)]
    pub artist: Option<String>,
    pub title: String,
    /// Wall-clock time the track started, in seconds since the epoch.
    pub ts: u64,
    /// Resolved iTunes artwork URL, filled in once the lookup succeeds;
    /// persisted so a thumbnail can be refetched after a restart.
    #[serde(default)]
    pub artwork_url: Option<String>,
}

impl HistoryEntry {
    pub fn new(station: String, artist: Option<String>, title: String) -> Self {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        Self {
            station,
            artist,
            title,
            ts,
            artwork_url: None,
        }
    }
}

/// Path to the history file in the config directory.
pub fn path() -> PathBuf {
    config::ensure_config_dir().join("history.json")
}

/// Load the history, newest first. A missing, empty or unreadable file
/// yields an empty history — a broken history must never stop the app.
pub fn load() -> Vec<HistoryEntry> {
    let text = match fs::read_to_string(path()) {
        Ok(t) => t,
        Err(_) => return Vec::new(),
    };
    match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("reading history: {e}");
            Vec::new()
        }
    }
}

/// Save the history atomically (write to a temp file, then rename), so a
/// crash mid-write cannot leave a half-written log.
pub fn save(entries: &[HistoryEntry]) -> anyhow::Result<()> {
    let path = path();
    let tmp = path.with_extension("json.tmp");
    let data = serde_json::to_string_pretty(entries)?;
    let mut f = fs::File::create(&tmp)?;
    f.write_all(data.as_bytes())?;
    f.flush()?;
    fs::rename(&tmp, &path)?;
    Ok(())
}

/// Prepend a new entry (newest first) and enforce the size cap.
///
/// If this exact track (station + artist + title) is already in the list,
/// it is moved to the front instead of added a second time, so the list
/// never shows duplicates. The moved entry's resolved artwork URL is carried
/// over to the refreshed one.
pub fn push(entries: &mut Vec<HistoryEntry>, mut entry: HistoryEntry) {
    if let Some(i) = entries.iter().position(|e| {
        e.station == entry.station && e.title == entry.title && e.artist == entry.artist
    }) {
        let old = entries.remove(i);
        if entry.artwork_url.is_none() {
            entry.artwork_url = old.artwork_url;
        }
    }
    entries.insert(0, entry);
    if entries.len() > MAX_ENTRIES {
        entries.truncate(MAX_ENTRIES);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_prepends_and_caps() {
        let mut v = Vec::new();
        for i in 0..MAX_ENTRIES + 10 {
            push(&mut v, HistoryEntry {
                station: "s".into(),
                artist: None,
                title: format!("{i}"),
                ts: i as u64,
                artwork_url: None,
            });
        }
        assert_eq!(v.len(), MAX_ENTRIES);
        // Newest first, and the oldest entries were dropped.
        assert_eq!(v[0].title, format!("{}", MAX_ENTRIES + 9));
        assert_eq!(v[MAX_ENTRIES - 1].title, "10");
    }

    #[test]
    fn round_trips() {
        let entries = vec![
            HistoryEntry {
                station: "NPO Radio 1".into(),
                artist: Some("Daft Punk".into()),
                title: "One More Time".into(),
                ts: 1234567890,
                artwork_url: Some("https://x/y.jpg".into()),
            },
            HistoryEntry {
                station: "X".into(),
                artist: None,
                title: "Y".into(),
                ts: 1,
                artwork_url: None,
            },
        ];
        let json = serde_json::to_string(&entries).unwrap();
        let back: Vec<HistoryEntry> = serde_json::from_str(&json).unwrap();
        assert_eq!(entries, back);
    }

    #[test]
    fn push_deduplicates_a_replay() {
        let mut v = Vec::new();
        push(
            &mut v,
            HistoryEntry {
                station: "s".into(),
                artist: Some("A".into()),
                title: "T".into(),
                ts: 1,
                artwork_url: Some("u".into()),
            },
        );
        // The same track played again: moved to the front, not duplicated.
        push(
            &mut v,
            HistoryEntry {
                station: "s".into(),
                artist: Some("A".into()),
                title: "T".into(),
                ts: 2,
                artwork_url: None,
            },
        );
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].ts, 2);
        // The resolved artwork from the older entry was carried over.
        assert_eq!(v[0].artwork_url.as_deref(), Some("u"));
        // A different artist is a different track, not a duplicate.
        push(
            &mut v,
            HistoryEntry {
                station: "s".into(),
                artist: Some("B".into()),
                title: "T".into(),
                ts: 3,
                artwork_url: None,
            },
        );
        assert_eq!(v.len(), 2);
    }
}
