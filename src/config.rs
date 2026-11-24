//! Configuration, stored as TOML in the user's config directory.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub const DEFAULT_API_URL: &str = "https://api.radiopit.fm";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// RadioPit account key.
    pub api_key: String,
    /// Base URL of the RadioPit API.
    pub api_url: String,
    /// Linear volume, 0.0..=1.0
    pub volume: f64,
    /// Id of the last played station, restored after a library refresh.
    pub last_station_id: Option<String>,
    /// Write/remove an autostart entry.
    pub autostart: bool,
    /// Show a desktop notification when a new track starts.
    pub notify_new_track: bool,
    /// How long that notification stays up, in seconds (0 = until
    /// dismissed).
    pub notify_duration_secs: u32,
    /// Stable id of this player on the account (remote control). The
    /// web/Android apps keep theirs in localStorage; the tray keeps it
    /// here so the "Playing on…" list recognizes this install.
    pub device_id: Option<String>,
    /// Whether other devices of the account may control this player
    /// (remote control, "incoming" direction). When off, the player still
    /// lists and controls the account's other devices, but registers as
    /// non-playable and ignores inbound commands.
    pub allow_remote_control: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            api_key: String::new(),
            api_url: DEFAULT_API_URL.into(),
            volume: 0.8,
            last_station_id: None,
            autostart: false,
            notify_new_track: true,
            notify_duration_secs: 10,
            device_id: None,
            allow_remote_control: true,
        }
    }
}

pub fn config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("radiopittray")
        .join("config.toml")
}

/// The app's config directory, created if missing and locked down to
/// owner-only (0700). It holds `config.toml` (with the RadioPit API key)
/// and the single-instance lock files; the default 0755/0644 would let
/// other local users read — and with the key, edit — the library, which
/// is how a local user could point stations at malicious stream URLs.
pub fn ensure_config_dir() -> PathBuf {
    let dir = config_path().parent().expect("config path has a parent").to_path_buf();
    let _ = std::fs::create_dir_all(&dir);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(&dir) {
            if meta.permissions().mode() & 0o7777 != 0o700 {
                let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
            }
        }
    }
    dir
}

impl Config {
    pub fn load() -> Self {
        let path = config_path();
        match std::fs::read_to_string(&path) {
            Ok(s) => toml::from_str(&s).unwrap_or_else(|e| {
                eprintln!("warning: failed to parse {}: {e}", path.display());
                Self::default()
            }),
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self) -> Result<()> {
        let dir = ensure_config_dir();
        let path = dir.join("config.toml");
        let s = toml::to_string_pretty(self).context("serializing config")?;
        std::fs::write(&path, s).with_context(|| format!("writing {}", path.display()))?;
        #[cfg(unix)]
        set_owner_only(&path);
        Ok(())
    }
}

/// The key file must be owner-only (0600) on multi-user systems.
#[cfg(unix)]
fn set_owner_only(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

const AUTOSTART_DESKTOP: &str = "\
[Desktop Entry]
Type=Application
Name=RadioPit Tray
Comment=Play your RadioPit library from the system tray
Exec=__EXEC__
Terminal=false
";

/// Enable or disable launching radiopittray at login.
pub fn set_autostart(enabled: bool) -> Result<()> {
    let dir = dirs::config_dir().context("no config dir available")?;
    let autostart_dir = dir.join("autostart");
    let path = autostart_dir.join("radiopittray.desktop");
    if enabled {
        let exe = std::env::current_exe().context("resolving executable path")?;
        let content = AUTOSTART_DESKTOP.replace("__EXEC__", &exe.to_string_lossy());
        std::fs::create_dir_all(&autostart_dir)?;
        std::fs::write(&path, content)?;
    } else {
        let _ = std::fs::remove_file(&path);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_roundtrip() {
        let mut c = Config::default();
        c.api_key = "test-key-123".into();
        c.volume = 0.42;
        c.notify_new_track = false;
        c.notify_duration_secs = 30;
        let s = toml::to_string(&c).unwrap();
        let d: Config = toml::from_str(&s).unwrap();
        assert_eq!(d.api_key, "test-key-123");
        assert!((d.volume - 0.42).abs() < f64::EPSILON);
        assert_eq!(d.api_url, DEFAULT_API_URL);
        assert!(!d.notify_new_track);
        assert_eq!(d.notify_duration_secs, 30);
    }

    #[test]
    fn missing_field_defaults_to_notify_on() {
        // Old config files (written before the option existed) must parse,
        // with the option on.
        let d: Config =
            toml::from_str("api_key = \"k\"\napi_url = \"https://api.radiopit.fm\"\n").unwrap();
        assert!(d.notify_new_track);
        assert_eq!(d.notify_duration_secs, 10);
        // Likewise, an old file without the remote-control toggle keeps
        // accepting inbound remote control (no behaviour change on upgrade).
        assert!(d.allow_remote_control);
    }

    #[cfg(unix)]
    #[test]
    fn key_file_is_written_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir =
            std::env::temp_dir().join(format!("radiopittray-perm-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("perm.toml");
        std::fs::write(&path, "x").unwrap();
        // Simulate an old, world-readable file and restore owner-only.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        set_owner_only(&path);
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
