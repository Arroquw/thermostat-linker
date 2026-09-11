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

/// Rate limiter for writes that Home Assistant accepted but the device did not
/// apply.
#[derive(Default)]
pub struct WriteThrottle {
    last_attempt: HashMap<String, Instant>,
    min_interval: Duration,
}

impl WriteThrottle {
    pub fn new(min_interval_secs: u64) -> Self {
        Self {
            last_attempt: HashMap::new(),
            min_interval: Duration::from_secs(min_interval_secs),
        }
    }

    /// Returns true if a write keyed by `key` may be attempted now.
    pub fn allow(&mut self, key: &str) -> bool {
        match self.last_attempt.get(key) {
            Some(t) if t.elapsed() < self.min_interval => false,
            _ => {
                self.last_attempt.insert(key.to_string(), Instant::now());
                true
            }
        }
    }

    pub fn clear(&mut self, key: &str) {
        self.last_attempt.remove(key);
    }

    pub fn retry_in(&self, key: &str) -> Option<Duration> {
        let t = self.last_attempt.get(key)?;
        self.min_interval.checked_sub(t.elapsed())
    }
}

impl WriteThrottle {
    pub fn attempted(&self, key: &str) -> bool {
        self.last_attempt.contains_key(key)
    }
}
