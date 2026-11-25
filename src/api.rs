//! Minimal client for the RadioPit web API.
//!
//! The API is authenticated with an `x-api-key` header (the RadioPit
//! account key). Endpoints used:
//!
//! - `GET /api/auth/validate` -> `{ valid, userId, playlistToken }`
//! - `GET /api/playlists`     -> list of playlists
//! - `GET /api/stations`      -> list of stations (each has a `playlistId`)

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use std::time::Duration;

use crate::urls;

#[derive(Debug, Clone, Deserialize)]
pub struct Playlist {
    #[serde(rename = "_id")]
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub order: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Station {
    #[serde(rename = "_id")]
    pub id: String,
    pub name: String,
    /// Direct stream URL (mp3/aac/hls/...).
    pub url: String,
    #[serde(default)]
    pub order: i64,
    /// Playlist this station belongs to, if any.
    #[serde(default, rename = "playlistId")]
    pub playlist_id: Option<String>,
    #[serde(default, rename = "iconUrl")]
    pub icon_url: Option<String>,
}

/// A playlist together with its ordered stations.
#[derive(Debug, Clone)]
pub struct PlaylistWithStations {
    pub playlist: Playlist,
    pub stations: Vec<Station>,
}

#[derive(Debug, Clone, Default)]
pub struct Library {
    pub playlists: Vec<PlaylistWithStations>,
    /// Stations not belonging to any playlist.
    pub unassigned: Vec<Station>,
}

impl Library {
    pub fn all_stations(&self) -> impl Iterator<Item = &Station> {
        self.playlists
            .iter()
            .flat_map(|p| p.stations.iter())
            .chain(self.unassigned.iter())
    }

    pub fn by_id(&self, id: &str) -> Option<&Station> {
        self.all_stations().find(|s| s.id == id)
    }
}

pub struct Client {
    http: reqwest::Client,
    base: String,
}

impl Client {
    pub fn new(api_url: &str) -> Self {
        if let Ok(u) = url::Url::parse(api_url) {
            if u.scheme() == "http" {
                tracing::warn!(
                    "api_url {api_url} uses plain http: the RadioPit key \
                     is sent unencrypted"
                );
            }
        }
        let http = reqwest::Client::builder()
            .user_agent(concat!("radiopittray/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(20))
            // reqwest only strips Authorization/Cookie on cross-host
            // redirects — not our custom x-api-key header — so a
            // compromised API server could leak the key via a redirect.
            // The same guard applies here as to stream/icon URLs.
            .redirect(urls::safe_redirect_policy())
            .build()
            .expect("http client");
        let base = api_url.trim_end_matches('/').to_string();
        Self { http, base }
    }

    /// Fetch the full library (playlists + stations, grouped).
    pub async fn fetch_library(&self, api_key: &str) -> Result<Library> {
        let (playlists, stations) = tokio::try_join!(
            self.get::<Vec<Playlist>>("/api/playlists", api_key),
            self.get::<Vec<Station>>("/api/stations", api_key)
        )?;

        let mut lib = Library {
            playlists: playlists
                .iter()
                .map(|p| PlaylistWithStations {
                    playlist: p.clone(),
                    stations: Vec::new(),
                })
                .collect(),
            unassigned: Vec::new(),
        };

        for st in stations {
            let pid = st.playlist_id.clone();
            if let Some(pid) = pid {
                if let Some(pl) = lib.playlists.iter_mut().find(|p| p.playlist.id == pid) {
                    pl.stations.push(st);
                    continue;
                }
            }
            lib.unassigned.push(st);
        }

        // Sort stations inside each playlist, and the playlists themselves.
        lib.playlists.sort_by_key(|p| p.playlist.order);
        for pl in lib.playlists.iter_mut() {
            pl.stations.sort_by_key(|s| s.order);
        }
        lib.unassigned.sort_by_key(|s| s.order);

        Ok(lib)
    }

    /// Validate that the key works by fetching the station list.
    pub async fn validate(&self, api_key: &str) -> Result<()> {
        self.get::<Vec<Station>>("/api/stations", api_key)
            .await
            .map(|_| ())
    }

    /// The account's internal user id, as reported by the auth endpoint.
    ///
    /// The remote-control WebSocket authenticates with `{ userId, apiKey }`,
    /// so the tray needs this id before it can connect (see remote.rs).
    pub async fn auth_user_id(&self, api_key: &str) -> Result<String> {
        #[derive(Deserialize)]
        struct Auth {
            #[serde(default)]
            valid: bool,
            #[serde(default, rename = "userId")]
            user_id: Option<String>,
        }
        let a = self.get::<Auth>("/api/auth/validate", api_key).await?;
        if !a.valid {
            return Err(anyhow!("RadioPit key reported as invalid"));
        }
        a.user_id
            .ok_or_else(|| anyhow!("auth response is missing userId"))
    }

    async fn get<T: for<'de> Deserialize<'de>>(&self, path: &str, api_key: &str) -> Result<T> {
        let url = format!("{}{}", self.base, path);
        tracing::debug!("GET {url}");
        let resp = self
            .http
            .get(&url)
            .header("x-api-key", api_key)
            .send()
            .await
            .with_context(|| format!("requesting {url}"))?;

        let status = resp.status();
        tracing::debug!("GET {url} -> HTTP {status}");
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(anyhow!("invalid RadioPit key (HTTP 401)"));
        }
        if !status.is_success() {
            return Err(anyhow!("RadioPit API error: HTTP {status} for {path}"));
        }

        let body = resp
            .text()
            .await
            .context("reading response body")?;
        serde_json::from_str(&body).with_context(|| format!("parsing {path}"))
    }
}

/// The RadioPit web server proxies third-party streams: the web app falls
/// back to `https://radiopit.fm/proxy?url=<stream>` when the direct stream
/// is unreachable (e.g. blocked by the user's firewall), and the tray app
/// does the same. The proxy lives on the web host, not the API host, so a
/// `api.` subdomain prefix is stripped when present. Needs no
/// authentication (the proxy serves any public stream URL).
pub fn proxy_stream_url(api_url: &str, stream_url: &str) -> String {
    let base = api_url.trim_end_matches('/');
    let base = base.replacen("://api.", "://", 1);
    format!(
        "{base}/proxy?url={}",
        url::form_urlencoded::byte_serialize(stream_url.as_bytes()).collect::<String>()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proxy_url_strips_the_api_prefix_and_encodes_the_stream() {
        assert_eq!(
            proxy_stream_url("https://api.radiopit.fm", "http://ro2.example.nl:8020/live"),
            "https://radiopit.fm/proxy?url=http%3A%2F%2Fro2.example.nl%3A8020%2Flive"
        );
    }

    #[test]
    fn proxy_url_uses_a_custom_api_host_as_is() {
        assert_eq!(
            proxy_stream_url("https://staging.example.com/", "http://a/b?c=d"),
            "https://staging.example.com/proxy?url=http%3A%2F%2Fa%2Fb%3Fc%3Dd"
        );
    }

    #[test]
    fn group_stations_into_playlists() {
        let playlists = vec![
            Playlist { id: "p1".into(), name: "A".into(), order: 1 },
        ];
        let stations = vec![
            Station {
                id: "s1".into(),
                name: "s1".into(),
                url: "u1".into(),
                order: 2,
                playlist_id: Some("p1".into()),
                icon_url: None,
            },
            Station {
                id: "s2".into(),
                name: "s2".into(),
                url: "u2".into(),
                order: 1,
                playlist_id: None,
                icon_url: None,
            },
        ];

        // Simulate the grouping logic.
        let mut lib = Library {
            playlists: playlists
                .iter()
                .map(|p| PlaylistWithStations {
                    playlist: p.clone(),
                    stations: Vec::new(),
                })
                .collect(),
            unassigned: Vec::new(),
        };
        for st in stations {
            let pid = st.playlist_id.clone();
            if let Some(pid) = pid {
                if let Some(pl) = lib.playlists.iter_mut().find(|p| p.playlist.id == pid) {
                    pl.stations.push(st);
                    continue;
                }
            }
            lib.unassigned.push(st);
        }
        assert_eq!(lib.playlists[0].stations.len(), 1);
        assert_eq!(lib.unassigned.len(), 1);
        assert!(lib.by_id("s1").is_some());
        assert!(lib.by_id("missing").is_none());
    }
}
