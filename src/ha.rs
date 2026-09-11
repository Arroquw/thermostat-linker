use anyhow::{bail, Context, Result};
use reqwest::blocking::Client;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// A single Home Assistant entity as returned by `/api/states`.
#[derive(Deserialize, Debug, Clone)]
pub struct Entity {
    pub entity_id: String,
    pub state: String,
    #[serde(default)]
    pub attributes: Value,
}

impl Entity {
    pub fn attr_f64(&self, key: &str) -> Option<f64> {
        self.attributes.get(key)?.as_f64()
    }

    /// True when Home Assistant reports the entity as unavailable or unknown
    pub fn is_available(&self) -> bool {
        !matches!(self.state.as_str(), "unavailable" | "unknown")
    }

    pub fn attr_str(&self, key: &str) -> Option<&str> {
        self.attributes.get(key)?.as_str()
    }

    /// The preset modes the integration advertises for this entity.
    pub fn preset_modes(&self) -> Vec<&str> {
        self.attributes
            .get("preset_modes")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
            .unwrap_or_default()
    }

    /// HVAC mode for a climate entity.
    pub fn hvac_mode(&self) -> &str {
        &self.state
    }
}

pub struct HomeAssistant {
    client: Client,
    base: String,
    token: String,
    pub dry_run: bool,
}

impl HomeAssistant {
    pub fn new(base: &str, token: String, dry_run: bool) -> Result<Self> {
        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .context("building HTTP client")?;
        Ok(Self {
            client,
            base: base.trim_end_matches('/').to_string(),
            token,
            dry_run,
        })
    }

    /// Verify the base URL and token before entering the poll loop, prevents looping on 401s.
    pub fn ping(&self) -> Result<()> {
        let url = format!("{}/api/", self.base);
        let resp = self
            .client
            .get(&url)
            .bearer_auth(&self.token)
            .send()
            .with_context(|| format!("GET {url}"))?;

        if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            bail!("Home Assistant rejected the access token (401 Unauthorized) -- it is invalid or expired");
        }
        if !resp.status().is_success() {
            bail!("GET {url} returned {}", resp.status());
        }
        Ok(())
    }

    /// Fetch every entity, keyed by entity id.
    pub fn states(&self) -> Result<HashMap<String, Entity>> {
        let url = format!("{}/api/states", self.base);
        let resp = self
            .client
            .get(&url)
            .bearer_auth(&self.token)
            .send()
            .with_context(|| format!("GET {url}"))?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().unwrap_or_default();
            if status == reqwest::StatusCode::UNAUTHORIZED {
                bail!(
                    "GET {url} returned 401 Unauthorized -- the access token is invalid or expired"
                );
            }
            bail!("GET {url} returned {status}: {}", body.trim());
        }

        let entities: Vec<Entity> = resp.json().context("decoding /api/states response")?;
        Ok(entities
            .into_iter()
            .map(|e| (e.entity_id.clone(), e))
            .collect())
    }

    /// Call a Home Assistant service. Returns an error on a non-2xx response
    pub fn call_service(&self, domain: &str, service: &str, data: Value) -> Result<()> {
        if self.dry_run {
            println!("    (dry-run) would call {domain}.{service} {data}");
            return Ok(());
        }

        let url = format!("{}/api/services/{}/{}", self.base, domain, service);
        let resp = self
            .client
            .post(&url)
            .bearer_auth(&self.token)
            .json(&data)
            .send()
            .with_context(|| format!("POST {url}"))?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().unwrap_or_default();
            bail!("{domain}.{service} failed with {status}: {}", body.trim());
        }

        std::thread::sleep(Duration::from_secs(1));
        Ok(())
    }

    pub fn set_hvac_mode(&self, entity_id: &str, mode: &str) -> Result<()> {
        self.call_service(
            "climate",
            "set_hvac_mode",
            json!({ "entity_id": entity_id, "hvac_mode": mode }),
        )
    }

    pub fn set_temperature(&self, entity_id: &str, field: &str, value: f64) -> Result<()> {
        self.call_service(
            "climate",
            "set_temperature",
            json!({ "entity_id": entity_id, field: value }),
        )
    }

    pub fn set_preset_mode(&self, entity_id: &str, preset: &str) -> Result<()> {
        self.call_service(
            "climate",
            "set_preset_mode",
            json!({ "entity_id": entity_id, "preset_mode": preset }),
        )
    }

    pub fn switch(&self, entity_id: &str, on: bool) -> Result<()> {
        let service = if on { "turn_on" } else { "turn_off" };
        self.call_service("switch", service, json!({ "entity_id": entity_id }))
    }
}

/// Rate limiter for commands a device keeps refusing.
///
/// Re-asking for the same value is a retry and is rate limited
#[derive(Default)]
pub struct WriteThrottle {
    pending: HashMap<String, Attempt>,
    min_interval: Duration,
}

struct Attempt {
    desired: String,
    at: Instant,
}

impl WriteThrottle {
    pub fn new(min_interval_secs: u64) -> Self {
        Self {
            pending: HashMap::new(),
            min_interval: Duration::from_secs(min_interval_secs),
        }
    }

    pub fn allow(&mut self, key: &str, desired: &str) -> bool {
        match self.pending.get(key) {
            Some(a) if a.desired == desired && a.at.elapsed() < self.min_interval => false,
            _ => {
                self.pending.insert(
                    key.to_string(),
                    Attempt {
                        desired: desired.to_string(),
                        at: Instant::now(),
                    },
                );
                true
            }
        }
    }

    pub fn clear(&mut self, key: &str) {
        self.pending.remove(key);
    }

    pub fn attempted(&self, key: &str, desired: &str) -> bool {
        self.pending.get(key).is_some_and(|a| a.desired == desired)
    }

    pub fn retry_in(&self, key: &str, desired: &str) -> Option<Duration> {
        let a = self.pending.get(key)?;
        if a.desired != desired {
            return None;
        }
        self.min_interval.checked_sub(a.at.elapsed())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_same_unsatisfied_command_is_rate_limited() {
        let mut t = WriteThrottle::new(300);
        assert!(t.allow("climate.x:hvac_mode", "heat"));
        assert!(!t.allow("climate.x:hvac_mode", "heat"));
    }

    #[test]
    fn a_different_target_is_never_blocked_by_an_earlier_one() {
        let mut t = WriteThrottle::new(300);
        assert!(t.allow("switch.pump", "on"));
        assert!(t.allow("switch.pump", "off"));
        assert!(t.allow("switch.pump", "on"));
    }

    #[test]
    fn an_idle_setpoint_is_not_blocked_by_the_previous_idle_setpoint() {
        let mut t = WriteThrottle::new(300);
        assert!(t.allow("climate.lyric:temperature", "21.5"));
        assert!(t.allow("climate.lyric:temperature", "22.0"));
        assert!(t.allow("climate.lyric:temperature", "21.5"));
        assert!(!t.attempted("climate.lyric:temperature", "22.0"));
    }

    #[test]
    fn reaching_the_desired_state_resets_the_throttle() {
        let mut t = WriteThrottle::new(300);
        assert!(t.allow("climate.x:hvac_mode", "heat"));
        assert!(!t.allow("climate.x:hvac_mode", "heat"));
        t.clear("climate.x:hvac_mode");
        assert!(t.allow("climate.x:hvac_mode", "heat"));
    }

    #[test]
    fn attempted_reports_only_the_same_pending_command() {
        let mut t = WriteThrottle::new(300);
        t.allow("climate.x:hvac_mode", "heat");
        assert!(t.attempted("climate.x:hvac_mode", "heat"));
        assert!(!t.attempted("climate.x:hvac_mode", "off"));
        assert!(t.retry_in("climate.x:hvac_mode", "off").is_none());
    }
}
