//! Minimal client for the RadioPit web API.
//!
//! The API is authenticated with an `x-api-key` header (the RadioPit
//! account key). Two endpoints are used:
//!
//! - `GET /api/playlists` -> list of playlists
//! - `GET /api/stations`  -> list of stations (each has a `playlistId`)

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use std::time::Duration;

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
        let http = reqwest::Client::builder()
            .user_agent(concat!("radiopittray/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(20))
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

#[cfg(test)]
mod tests {
    use super::*;

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
