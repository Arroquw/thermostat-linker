use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Default location of the configuration file, relative to the user's config dir.
pub const DEFAULT_CONFIG_RELPATH: &str = "thermostat-linker/Config.toml";
/// Older location, still honoured so existing installs keep working.
pub const LEGACY_CONFIG_RELPATH: &str = "thermostat-linkerd/Config.toml";

/// Environment variable consulted when the config names no token source at all.
pub const FALLBACK_TOKEN_ENV: &str = "HA_TOKEN";

#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Entity id of the main (heat pump) thermostat, e.g. `climate.keuken_warmtepomp`.
    pub main: String,

    /// Amount added to the main thermostat's own measured temperature to derive
    /// the setpoint
    pub temp_increment: f64,

    /// Preset mode to hold the main thermostat in, e.g. `PermanentHold`.
    ///
    /// A Lyric on a schedule treats API writes as a `TemporaryHold` and reverts
    /// them at the next scheduled setpoint change
    #[serde(default)]
    pub preset_mode: Option<String>,

    /// Margin around each zone thermostat's setpoint, in degrees.
    #[serde(default = "default_hysteresis")]
    pub hysteresis: f64,

    #[serde(default)]
    pub dry_run: bool,

    #[serde(default = "default_poll_interval")]
    pub poll_interval: u64,

    /// Protects the Honeywell cloud API from being hammered when a
    /// thermostat refuses to accept a command.
    #[serde(default = "default_min_write_interval")]
    pub min_write_interval: u64,

    pub home_assistant: HaConfig,

    /// Map of pump name -> zone thermostat entity ids.
    #[serde(default)]
    pub zones: HashMap<String, Vec<String>>,
}

fn default_hysteresis() -> f64 {
    0.2
}
fn default_poll_interval() -> u64 {
    30
}
fn default_min_write_interval() -> u64 {
    300
}

#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct HaConfig {
    /// Base URL of the Home Assistant instance, e.g. `http://10.0.1.1:8123`.
    pub baselink: String,

    /// Name of an environment variable holding the long-lived access token.
    #[serde(default)]
    pub token_env: Option<String>,

    /// Shell command, stdout of which is the token, e.g. `pass show home-assistant/token`.
    #[serde(default)]
    pub token_command: Option<String>,

    /// Path to a file containing the token. Must not be group/world readable.
    #[serde(default)]
    pub token_file: Option<String>,

    /// Token inline in the config file. Discouraged: warning on use.
    #[serde(default)]
    pub token: Option<String>,
}

impl Config {
    /// Resolve which config file to read: an explicit `--config` path if given,
    /// otherwise the default location
    pub fn resolve_path(explicit: Option<PathBuf>) -> Result<PathBuf> {
        if let Some(p) = explicit {
            return Ok(expand_tilde(&p));
        }

        let base = dirs::config_dir()
            .ok_or_else(|| anyhow!("cannot determine user config directory; pass --config"))?;

        let preferred = base.join(DEFAULT_CONFIG_RELPATH);
        if preferred.exists() {
            return Ok(preferred);
        }

        let legacy = base.join(LEGACY_CONFIG_RELPATH);
        if legacy.exists() {
            eprintln!(
                "warning: reading config from legacy path {}; \
                 move it to {} (the legacy path will keep working)",
                legacy.display(),
                preferred.display()
            );
            return Ok(legacy);
        }

        // Neither exists: report the preferred path, so the error names the
        // location the user is supposed to create.
        Ok(preferred)
    }

    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading config file {}", path.display()))?;
        let config: Config = toml::from_str(&raw)
            .with_context(|| format!("parsing config file {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        if self.poll_interval == 0 {
            bail!("poll_interval must be greater than 0");
        }
        if self.hysteresis < 0.0 {
            bail!("hysteresis must not be negative");
        }
        if self.temp_increment <= 0.0 {
            bail!(
                "temp_increment must be greater than 0, otherwise the main \
                 thermostat is never asked for a temperature above the room"
            );
        }
        if !self.main.starts_with("climate.") {
            bail!(
                "main = \"{}\" does not look like a climate entity id (expected `climate.<name>`)",
                self.main
            );
        }
        if self.zones.is_empty() {
            eprintln!("warning: no [zones] configured; nothing will call for heat");
        }
        Ok(())
    }
}

impl HaConfig {
    /// Resolve the access token from the configured sources, in order:
    /// `token_env` -> `token_command` -> `token_file` -> inline `token`.
    pub fn resolve_token(&self) -> Result<String> {
        let mut tried: Vec<String> = Vec::new();

        if let Some(var) = &self.token_env {
            match std::env::var(var) {
                Ok(v) if !v.trim().is_empty() => return Ok(v.trim().to_string()),
                _ => tried.push(format!("token_env (${var} unset or empty)")),
            }
        }

        if let Some(cmd) = &self.token_command {
            match token_from_command(cmd) {
                Ok(v) => return Ok(v),
                Err(e) => tried.push(format!("token_command ({e:#})")),
            }
        }

        if let Some(file) = &self.token_file {
            match token_from_file(Path::new(file)) {
                Ok(v) => return Ok(v),
                Err(e) => tried.push(format!("token_file ({e:#})")),
            }
        }

        if let Some(tok) = &self.token {
            if !tok.trim().is_empty() {
                eprintln!(
                    "warning: using the inline `token` from the config file. \
                     Prefer token_env / token_command / token_file so the secret \
                     is not stored in a file you might commit."
                );
                return Ok(tok.trim().to_string());
            }
            tried.push("token (inline, empty)".to_string());
        }

        if self.token_env.is_none()
            && self.token_command.is_none()
            && self.token_file.is_none()
            && self.token.is_none()
        {
            if let Ok(v) = std::env::var(FALLBACK_TOKEN_ENV) {
                if !v.trim().is_empty() {
                    return Ok(v.trim().to_string());
                }
            }
            bail!(
                "no Home Assistant token configured. Set one of token_env, \
                 token_command or token_file under [home_assistant], or export ${FALLBACK_TOKEN_ENV}"
            );
        }

        bail!(
            "could not resolve a Home Assistant token. Tried: {}",
            tried.join("; ")
        )
    }
}

fn token_from_command(cmd: &str) -> Result<String> {
    let out = std::process::Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .output()
        .with_context(|| format!("running token_command `{cmd}`"))?;

    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        bail!(
            "token_command `{cmd}` exited with {}: {}",
            out.status,
            stderr.trim()
        );
    }

    let token = String::from_utf8(out.stdout)
        .context("token_command produced non-UTF-8 output")?
        .trim()
        .to_string();

    if token.is_empty() {
        bail!("token_command `{cmd}` produced no output");
    }
    Ok(token)
}

fn token_from_file(path: &Path) -> Result<String> {
    let path = expand_tilde(path);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let meta = std::fs::metadata(&path).with_context(|| format!("stat {}", path.display()))?;
        let mode = meta.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            bail!(
                "{} is group/world accessible (mode {:o}); run `chmod 600 {}`",
                path.display(),
                mode,
                path.display()
            );
        }
    }

    let token = std::fs::read_to_string(&path)
        .with_context(|| format!("reading {}", path.display()))?
        .trim()
        .to_string();

    if token.is_empty() {
        bail!("{} is empty", path.display());
    }
    Ok(token)
}

fn expand_tilde(path: &Path) -> PathBuf {
    let s = path.to_string_lossy();
    if let Some(rest) = s.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    path.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(body: &str) -> Result<Config> {
        let raw = format!(
            r#"
            main = "climate.test"
            {body}
            [home_assistant]
            baselink = "http://localhost:8123"
            "#
        );
        let config: Config = toml::from_str(&raw)?;
        config.validate()?;
        Ok(config)
    }

    #[test]
    fn a_minimal_config_parses() {
        let c = parse("temp_increment = 0.5").unwrap();
        assert_eq!(c.temp_increment, 0.5);
        assert_eq!(c.poll_interval, 30);
    }

    #[test]
    fn non_positive_increment_is_rejected() {
        assert!(parse("temp_increment = 0.0").is_err());
        assert!(parse("temp_increment = -1.0").is_err());
    }

    #[test]
    fn stale_keys_are_rejected() {
        // deny_unknown_fields, so a key left over from an older config is
        // reported rather than silently ignored.
        assert!(parse("temp_increment = 0.5\nheat_setpoint = 22.0").is_err());
    }
}
