//! Shared album-art lookup and caches.
//!
//! This is the same public Apple iTunes Search API the RadioPit
//! web/Android apps query: one result, searched by "artist title". The
//! desktop notification and the tray window (now-playing art and the
//! history table) share one process-wide cache, so a track that already
//! raised a notification costs no extra Apple request.
//!
//! Two caches, both keyed so a lookup happens at most once per key:
//! - the *art cache* maps a search query to the artwork URL (or `None`
//!   when the lookup found nothing), so a miss is remembered too;
//! - the *thumbnail cache* maps an artwork URL to a decoded
//!   `RgbaImage`, filled lazily. The UI reads this synchronously
//!   ([`peek_thumb`]); a background task fills it via [`thumb`].

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use serde::Deserialize;

use crate::icon;

/// The iTunes Search API query is at most this many characters (the
/// web app slices its query to 80 as well).
const MAX_QUERY: usize = 80;
/// Number of cached entries (positive and negative) before a cache is
/// dropped wholesale.
const CACHE_MAX: usize = 512;

/// The search query for a track: "Artist Title" — the web app searches
/// the same way, from the raw "Artist - Title" string — capped so a
/// pathological title cannot bloat the request.
pub fn art_query(artist: Option<&str>, title: &str) -> String {
    let query = match artist.map(str::trim).filter(|a| !a.is_empty()) {
        Some(a) => format!("{a} {}", title.trim()),
        None => title.trim().to_string(),
    };
    query.chars().take(MAX_QUERY).collect()
}

/// The iTunes thumbnail URL rewritten to 300 px (the tray shows it at
/// larger sizes; 300 keeps it sharp on HiDPI).
fn upscale(url: &str) -> String {
    url.replace("100x100bb", "300x300bb")
}

/// Query -> artwork URL, or `None` when the lookup found nothing.
type ArtCacheMap = HashMap<String, Option<String>>;
/// Artwork URL -> decoded thumbnail (or `None` when it could not be
/// fetched/decoded), so a failed artwork is not retried every frame.
type ThumbCacheMap = HashMap<String, Option<image::RgbaImage>>;

/// Process-wide cache of art lookups (see `CACHE_MAX`).
fn art_cache() -> MutexGuard<'static, ArtCacheMap> {
    static CACHE: OnceLock<Mutex<ArtCacheMap>> = OnceLock::new();
    CACHE
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// Process-wide cache of decoded artwork thumbnails (see `CACHE_MAX`).
fn thumb_cache() -> MutexGuard<'static, ThumbCacheMap> {
    static CACHE: OnceLock<Mutex<ThumbCacheMap>> = OnceLock::new();
    CACHE
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// The artwork URL Apple reports for this query, if any. Lookups are
/// cached — positive *and* negative — so a repeated title costs no extra
/// Apple request.
pub async fn artwork_url(query: &str) -> Option<String> {
    let cached = {
        let cache = art_cache();
        cache.get(query).cloned()
    };
    match cached {
        Some(url) => url,
        None => {
            let found = itunes_art_url(query).await;
            let mut cache = art_cache();
            if cache.len() >= CACHE_MAX {
                cache.clear();
            }
            cache.insert(query.to_string(), found.clone());
            found
        }
    }
}

/// The best artwork URL in an iTunes response, rewritten to 300 px.
fn art_url_from_response(resp: &ItunesResponse) -> Option<String> {
    resp.results
        .first()
        .and_then(|r| r.artwork_url.as_ref())
        .map(|a| upscale(a))
}

async fn itunes_art_url(query: &str) -> Option<String> {
    let url = format!(
        "https://itunes.apple.com/search?term={}&media=music&limit=1",
        url::form_urlencoded::byte_serialize(query.as_bytes()).collect::<String>()
    );
    let client = reqwest::Client::builder()
        .user_agent(concat!("radiopittray/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(10))
        .build()
        .ok()?;
    let resp: ItunesResponse = client.get(&url).send().await.ok()?.json().await.ok()?;
    art_url_from_response(&resp)
}

/// Fetch and cache a `size`×`size` thumbnail for an artwork URL, then
/// return it. A URL that cannot be fetched or decoded is cached as
/// `None` so it is not retried.
pub async fn thumb(url: &str, size: u32) -> Option<image::RgbaImage> {
    let key = thumb_key(url, size);
    let cached = {
        let cache = thumb_cache();
        cache.get(&key).cloned()
    };
    if let Some(img) = cached {
        return img;
    }
    let fetched = match icon::fetch_icon_bytes(url).await {
        Ok(bytes) => icon::load_icon_image(&bytes).map(|img| icon::resize_square(&img, size)),
        Err(_) => None,
    };
    let mut cache = thumb_cache();
    if cache.len() >= CACHE_MAX {
        cache.clear();
    }
    cache.insert(key, fetched.clone());
    fetched
}

/// Synchronous read of a cached thumbnail (the UI's entry point; it must
/// not block on the network). Returns `None` until [`thumb`] has filled
/// it, or when the artwork is known to have failed.
pub fn peek_thumb(url: &str, size: u32) -> Option<image::RgbaImage> {
    thumb_cache().get(&thumb_key(url, size)).cloned().flatten()
}

/// Whether a fetch/lookup for this exact key is already known-done
/// (cached, success or failure). Used to avoid spawning duplicate
/// background fetches.
pub fn thumb_known(url: &str, size: u32) -> bool {
    thumb_cache().contains_key(&thumb_key(url, size))
}

/// Cache key combining the URL and the requested size, so the same
/// artwork can be kept at several resolutions.
fn thumb_key(url: &str, size: u32) -> String {
    format!("{url}#{size}")
}

/// One iTunes Search API response (only the fields we use).
#[derive(Debug, Deserialize)]
struct ItunesResponse {
    #[serde(default)]
    results: Vec<ItunesResult>,
}

#[derive(Debug, Deserialize)]
struct ItunesResult {
    #[serde(default, rename = "artworkUrl100")]
    artwork_url: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn art_query_combines_artist_and_title() {
        assert_eq!(
            art_query(Some("Daft Punk"), "One More Time"),
            "Daft Punk One More Time"
        );
    }

    #[test]
    fn art_query_without_artist_uses_the_title() {
        assert_eq!(art_query(None, "Solo"), "Solo");
        assert_eq!(art_query(Some("  "), "Solo"), "Solo");
        assert_eq!(art_query(Some("X"), "  A  "), "X A");
    }

    #[test]
    fn art_query_is_capped() {
        let q = art_query(None, &"a".repeat(100));
        assert_eq!(q.chars().count(), MAX_QUERY);
        // On a char boundary, not a byte boundary.
        let q = art_query(None, &"é".repeat(100));
        assert_eq!(q.chars().count(), MAX_QUERY);
    }

    #[test]
    fn upscales_the_itunes_thumbnail() {
        let u = "https://is1-ssl.mzstatic.com/image/thumb/xyz/100x100bb.jpg";
        assert_eq!(upscale(u), "https://is1-ssl.mzstatic.com/image/thumb/xyz/300x300bb.jpg");
        // A URL without the marker is left alone.
        assert_eq!(upscale("https://x/y.jpg"), "https://x/y.jpg");
    }

    #[test]
    fn parses_an_itunes_response() {
        let v = serde_json::json!({
            "resultCount": 1,
            "results": [ { "artworkUrl100": "https://x/y.jpg/100x100bb.jpg" } ]
        });
        let r: ItunesResponse = serde_json::from_value(v).unwrap();
        assert_eq!(
            art_url_from_response(&r),
            Some("https://x/y.jpg/300x300bb.jpg".into())
        );
        // No results (or no artwork field): no art, not an error.
        let empty: ItunesResponse =
            serde_json::from_value(serde_json::json!({ "resultCount": 0, "results": [] })).unwrap();
        assert_eq!(art_url_from_response(&empty), None);
        let no_art: ItunesResponse =
            serde_json::from_value(serde_json::json!({ "results": [ {} ] })).unwrap();
        assert_eq!(art_url_from_response(&no_art), None);
    }
}
