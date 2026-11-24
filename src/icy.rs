//! ICY metadata: a sideband HTTP connection to the stream that delivers
//! `StreamTitle=Artist - Title` metadata blocks.
//!
//! Shoutcast ICY protocol: the client asks for metadata with the
//! `Icy-MetaData: 1` request header. The server then interleaves the audio
//! with metadata: after every `icy-metaint` audio bytes (16000 by default)
//! there is a 1-byte "metaflag"; a nonzero value N means the next N*16
//! bytes are a UTF-8 metadata string.
//!
//! The task runs on the tokio runtime and writes directly into `Shared`.
//! It reconnects with a short backoff while a stream is wanted; a
//! connection that delivers no title within the deadline is considered
//! non-ICY (e.g. HLS) and the task exits with the info lines cleared.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::notify::{self, NowPlaying};
use crate::state::Shared;

/// Commands for the ICY task.
#[derive(Debug)]
pub enum IcyCmd {
    /// Start (or restart) metadata tracking for this stream url.
    Play(String),
    /// Stop metadata tracking.
    Stop,
}

/// Owns the ICY task; send commands through `tx`.
pub struct IcyHandle {
    tx: tokio::sync::mpsc::UnboundedSender<IcyCmd>,
}

impl IcyHandle {
    /// Spawn the long-running dispatcher task.
    pub fn spawn(handle: tokio::runtime::Handle, shared: Arc<Mutex<Shared>>) -> Self {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<IcyCmd>();
        let inner = handle.clone();
        handle.spawn(async move {
            let mut current: Option<tokio::task::JoinHandle<()>> = None;
            while let Some(cmd) = rx.recv().await {
                if let Some(h) = current.take() {
                    h.abort();
                }
                match cmd {
                    IcyCmd::Play(url) => {
                        current = Some(inner.spawn(icy_loop(url, shared.clone())));
                    }
                    IcyCmd::Stop => {}
                }
            }
            if let Some(h) = current.take() {
                h.abort();
            }
        });
        Self { tx }
    }

    pub fn send(&self, cmd: IcyCmd) {
        if let Err(e) = self.tx.send(cmd) {
            tracing::debug!("icy task gone: {e}");
        }
    }
}

/// Retry backoff between (re)connection attempts.
const RECONNECT_DELAY: Duration = Duration::from_secs(2);
/// A connection that produced no title within this window is not ICY.
const NO_TITLE_DEADLINE: Duration = Duration::from_secs(45);
/// Read timeout: a stalled connection is dropped and reconnected.
const READ_TIMEOUT: Duration = Duration::from_secs(30);

async fn icy_loop(url: String, shared: Arc<Mutex<Shared>>) {
    tracing::debug!("icy: tracking metadata for {url}");
    set_info(&shared, None, None);

    loop {
        match track_once(&url, &shared).await {
            TrackResult::HasTitle => {
                // The stream provides ICY metadata. Keep the connection
                // alive as long as it keeps delivering; on a drop, retry.
            }
            TrackResult::NoIcy => {
                // Stream does not provide ICY metadata (HLS & friends):
                // retrying would just wait the deadline again.
                tracing::debug!("icy: stream has no ICY metadata");
                return;
            }
            TrackResult::Failed => {
                // Connection problem; the player retries the audio on its
                // own, so give the metadata a few tries as well.
                tokio::time::sleep(RECONNECT_DELAY).await;
            }
        }
    }
}

/// Outcome of one metadata connection.
enum TrackResult {
    /// At least one title was delivered; the connection then ended.
    HasTitle,
    /// The connection was healthy but no metadata ever arrived.
    NoIcy,
    /// The connection failed (HTTP error, network, stall).
    Failed,
}

/// Connect once, follow the metadata stream until it ends.
async fn track_once(url: &str, shared: &Arc<Mutex<Shared>>) -> TrackResult {
    let client = reqwest::Client::builder()
        .user_agent(concat!("radiopittray/", env!("CARGO_PKG_VERSION")))
        // No overall timeout: this is a long-lived connection.
        .redirect(crate::urls::safe_redirect_policy())
        .build()
        .expect("http client");

    let resp = match client
        .get(url)
        .header("Icy-MetaData", "1")
        .header("Icy-Connection", "live")
        .header("Accept", "*/*")
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::debug!("icy: connect: {e:#}");
            return TrackResult::Failed;
        }
    };
    if !resp.status().is_success() {
        tracing::debug!("icy: connect -> HTTP {}", resp.status());
        return TrackResult::Failed;
    }

    // Audio bytes between metadata blocks. A server without an `icy-metaint`
    // header does not provide metadata at all — parsing anyway would
    // misread audio bytes as metadata (binary garbage in the menu).
    let metaint = resp
        .headers()
        .get("icy-metaint")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|v| *v > 0);
    let Some(metaint) = metaint else {
        tracing::debug!("icy: no icy-metaint header, stream has no metadata");
        set_info(shared, None, None);
        return TrackResult::NoIcy;
    };

    let mut got_title = false;
    let deadline = std::time::Instant::now() + NO_TITLE_DEADLINE;

    // Interleave state machine.
    let mut audio_left = metaint;
    let mut meta_left: usize = 0;
    let mut meta_buf = Vec::new();

    let mut resp = resp;
    loop {
        let chunk = match tokio::time::timeout(READ_TIMEOUT, resp.chunk()).await {
            Ok(Ok(Some(c))) => c,
            Ok(Ok(None)) => break, // clean end of stream
            Ok(Err(e)) => {
                tracing::debug!("icy: read: {e:#}");
                return TrackResult::Failed;
            }
            Err(_) => {
                tracing::debug!("icy: read stalled, reconnecting");
                return TrackResult::Failed;
            }
        };

        for &b in chunk.iter() {
            if meta_left > 0 {
                meta_buf.push(b);
                meta_left -= 1;
                if meta_left == 0 {
                    if let Some((artist, title)) = parse_stream_title(&meta_buf) {
                        got_title = true;
                        tracing::debug!("icy: {artist:?} - {title}");
                        set_info(shared, artist, Some(title));
                    }
                    meta_buf.clear();
                    audio_left = metaint;
                }
            } else if audio_left > 0 {
                audio_left -= 1;
            } else {
                // The next byte is the metaflag.
                let n = b as usize;
                if n == 0 {
                    audio_left = metaint;
                } else {
                    meta_left = n * 16;
                    meta_buf.clear();
                }
                continue;
            }
        }

        // Give up on streams that connect fine but never deliver metadata
        // (HLS & friends stream forever without ICY).
        if !got_title && std::time::Instant::now() > deadline {
            tracing::debug!("icy: no metadata within 45s, assuming non-ICY stream");
            set_info(shared, None, None);
            return TrackResult::NoIcy;
        }
    }

    if got_title {
        TrackResult::HasTitle
    } else {
        set_info(shared, None, None);
        TrackResult::NoIcy
    }
}

/// Split an ICY `StreamTitle` metadata block into (artist, title).
///
/// The block arrives as `StreamTitle=...;` padded with NUL bytes to a
/// multiple of 16. Most stations send "Artist - Title" inside; some wrap
/// it in quotes and put the station name on a first line.
fn parse_stream_title(raw: &[u8]) -> Option<(Option<String>, String)> {
    // Drop the NUL padding.
    let raw = raw.split(|&b| b == 0).next().unwrap_or(&[]);
    let s = String::from_utf8_lossy(raw);
    let s = s
        .trim()
        .strip_prefix("StreamTitle=")
        .unwrap_or(s.trim())
        .trim_end_matches(';')
        .trim()
        .to_string();
    // Some streams append extra fields after the title, e.g. Radio Paradise:
    // "Artist - Title';StreamUrl='http://cover.jpg';" — cut at the first
    // `';` (falling back to a plain `;`).
    let s = match s.find("';") {
        Some(i) => s[..i].to_string(),
        None => match s.find(';') {
            Some(i) => s[..i].to_string(),
            None => s,
        },
    };
    // Quote handling: some servers quote the whole title
    // ('Artist - Title', NPO), others quote only the start
    // ('Artist - Title, SomaFM and Radio Paradise). Strip one leading
    // quote; strip a trailing quote only when a leading one was
    // stripped, so a title that legitimately ends in an apostrophe
    // (e.g. "Rockin'") is left alone.
    let s = match s.as_bytes().first() {
        Some(b'\'' | b'"') => {
            let inner = &s[1..];
            let inner = match inner.as_bytes().last() {
                Some(b'\'' | b'"') => &inner[..inner.len() - 1],
                _ => inner,
            };
            inner.to_string()
        }
        _ => s,
    };
    // Multi-line titles: the track info is the last non-empty line.
    let line = s
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim()
        .to_string();
    if line.is_empty() {
        return None;
    }
    match line.find(" - ") {
        Some(pos) => {
            let artist = line[..pos].trim().to_string();
            let title = line[pos + 3..].trim().to_string();
            Some((
                if artist.is_empty() {
                    None
                } else {
                    Some(artist)
                },
                title,
            ))
        }
        None => Some((None, line)),
    }
}

/// Store the metadata in the shared state and trigger a menu rebuild
/// (the tray's main loop picks it up on its next tick). When the
/// (artist, title) pair actually changed, the "new track" notification
/// is queued as well (if enabled in the config).
fn set_info(
    shared: &Arc<Mutex<Shared>>,
    artist: Option<String>,
    title: Option<String>,
) {
    let now_playing = {
        let mut s = shared.lock().unwrap();
        let changed =
            (artist.as_ref(), title.as_ref()) != (s.icy_artist.as_ref(), s.icy_title.as_ref());
        let notify = if changed && s.config.notify_new_track {
            s.current.as_ref().and_then(|st| {
                title.as_ref().map(|t| NowPlaying {
                    station: Some(st.name.clone()),
                    artist: artist.clone(),
                    title: t.clone(),
                    icon_url: st.icon_url.clone(),
                    duration_secs: s.config.notify_duration_secs,
                })
            })
        } else {
            None
        };
        s.icy_artist = artist;
        s.icy_title = title;
        s.menu_dirty = true;
        notify
    };
    if let Some(np) = now_playing {
        // Fire and forget: the icon fetch + D-Bus call must not stall
        // the metadata stream.
        tokio::task::spawn(async move {
            notify::show(np).await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_artist_and_title() {
        let (a, t) = parse_stream_title(b"Daft Punk - One More Time;").unwrap();
        assert_eq!(a.as_deref(), Some("Daft Punk"));
        assert_eq!(t, "One More Time");
    }

    #[test]
    fn parses_title_only() {
        let (a, t) = parse_stream_title(b"Just A Title;").unwrap();
        assert_eq!(a, None);
        assert_eq!(t, "Just A Title");
    }

    #[test]
    fn ignores_empty() {
        assert_eq!(parse_stream_title(b";"), None);
        assert_eq!(parse_stream_title(b"   ;"), None);
        assert_eq!(parse_stream_title(b"\0\0\0"), None);
    }

    #[test]
    fn parses_streamtitle_prefix_with_nul_padding() {
        let raw = b"StreamTitle=Radio 2 - De Ochtendshow;\0\0\0\0";
        let (a, t) = parse_stream_title(raw).unwrap();
        assert_eq!(a.as_deref(), Some("Radio 2"));
        assert_eq!(t, "De Ochtendshow");
    }

    #[test]
    fn parses_quoted_multiline_station_title() {
        // Real NPO format: quoted, station name on the first line.
        let raw = b"StreamTitle='NPO Radio 1\nNOS Met het Oog op Morgen - NOS';\0\0";
        let (a, t) = parse_stream_title(raw).unwrap();
        assert_eq!(a.as_deref(), Some("NOS Met het Oog op Morgen"));
        assert_eq!(t, "NOS");
    }

    #[test]
    fn parses_leading_quote_only() {
        // SomaFM / Radio Paradise: 'Artist - Title (start quoted, no
        // trailing quote).
        let raw = b"StreamTitle='Blondie - Rapture (Doomsday Deathmix);\0";
        let (a, t) = parse_stream_title(raw).unwrap();
        assert_eq!(a.as_deref(), Some("Blondie"));
        assert_eq!(t, "Rapture (Doomsday Deathmix)");
    }

    #[test]
    fn keeps_apostrophe_at_end_of_unquoted_title() {
        // No leading quote: a trailing apostrophe belongs to the title.
        let (a, t) = parse_stream_title(b"Johnny Cash - Rockin';").unwrap();
        assert_eq!(a.as_deref(), Some("Johnny Cash"));
        assert_eq!(t, "Rockin'");
    }

    #[test]
    fn quoted_title_ending_in_apostrophe() {
        // Leading + trailing quote: the inner apostrophe survives.
        let (a, t) = parse_stream_title(b"StreamTitle='Rockin'';\0").unwrap();
        assert_eq!(a, None);
        assert_eq!(t, "Rockin'");
    }

    #[test]
    fn strips_streamurl_field() {
        // Real Radio Paradise format: title followed by a StreamUrl field.
        let raw = b"StreamTitle=The Lumineers - Ophelia';StreamUrl='http://img.radioparadise.com/covers/l/11309.jpg';\0";
        let (a, t) = parse_stream_title(raw).unwrap();
        assert_eq!(a.as_deref(), Some("The Lumineers"));
        assert_eq!(t, "Ophelia");
    }
}
