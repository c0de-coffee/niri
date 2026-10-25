//! `/etc/remote-dm/config.toml`. Everything has a default; the file is optional.

use std::path::Path;

use anyhow::Context as _;
use serde::Deserialize;

pub const DEFAULT_PATH: &str = "/etc/remote-dm/config.toml";

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub greeter: Greeter,
    pub session: UserSession,
}

/// The login screen each remote connection starts on.
#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct Greeter {
    /// Greeter sessions run as system users named with this prefix, one per remote connection at
    /// a time (`rdm-greeter-1`, `rdm-greeter-2`, ...): concurrent sessions of the same user would
    /// share its session bus and systemd user instance. The number of such users is the number
    /// of connections that can be at the login screen at once.
    pub user_prefix: String,
    /// PAM service for greeter sessions.
    pub pam_service: String,
    /// Command run in greeter sessions: a headless compositor running `grd --handover`, a greetd
    /// greeter, and `remote-dm session-manager`.
    pub command: String,
}

/// The user's session after logging in.
#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct UserSession {
    /// PAM service for user sessions.
    pub pam_service: String,
    /// PAM service for users that gnome-remote-desktop already authenticated (Kerberos single
    /// sign-on): they skip the greeter. Empty: always go through the greeter.
    pub preauthenticated_pam_service: String,
    /// Command run in user sessions: a headless compositor and `grd --handover`. The greeter's
    /// session command is ignored, it's meant for local sessions.
    pub command: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            greeter: Greeter::default(),
            session: UserSession::default(),
        }
    }
}

impl Default for Greeter {
    fn default() -> Self {
        Self {
            user_prefix: "rdm-greeter-".to_owned(),
            pam_service: "remote-dm-greeter".to_owned(),
            command: "niri --headless --session -c /etc/remote-dm/greeter.kdl".to_owned(),
        }
    }
}

impl Default for UserSession {
    fn default() -> Self {
        Self {
            pam_service: "remote-dm".to_owned(),
            preauthenticated_pam_service: "remote-dm-autologin".to_owned(),
            command: "/usr/lib/gnome-remote-desktop-daemon --handover & \
                      exec niri --headless --session"
                .to_owned(),
        }
    }
}

impl Config {
    /// Loads the config. A missing file at the default path just means defaults.
    pub fn load(path: &Path, explicit: bool) -> anyhow::Result<Self> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound && !explicit => {
                tracing::info!("no {path:?}, using defaults");
                return Ok(Self::default());
            }
            Err(err) => return Err(err).with_context(|| format!("error reading {path:?}")),
        };
        toml::from_str(&text).with_context(|| format!("error parsing {path:?}"))
    }
}
