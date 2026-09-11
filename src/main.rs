mod config;
mod ha;

use anyhow::{Context, Result};
use clap::Parser;
use config::Config;
use ha::{Entity, HomeAssistant, WriteThrottle};
use std::collections::HashMap;
use std::path::PathBuf;
use std::thread::sleep;
use std::time::Duration;

/// Tolerance when comparing thermostat setpoints, in degrees.
const TEMP_EPSILON: f64 = 0.05;

#[derive(Parser, Debug)]
#[command(
    name = "thermostat-linkerd",
    version,
    about = "Link Livisi floor heating zones to a Honeywell Lyric controlled heat pump via Home Assistant"
)]
struct Cli {
    /// Path to the configuration file
    /// [default: ~/.config/thermostat-linker/Config.toml]
    #[arg(short, long, value_name = "FILE")]
    config: Option<PathBuf>,

    /// Log what would be done without calling any Home Assistant service
    #[arg(long)]
    dry_run: bool,

    /// Run a single poll and exit, instead of looping
    #[arg(long)]
    once: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let path = Config::resolve_path(cli.config)?;
    let config = Config::load(&path)?;
    println!("Loaded config from {}", path.display());

    let token = config.home_assistant.resolve_token()?;
    let dry_run = config.dry_run || cli.dry_run;
    let ha = HomeAssistant::new(&config.home_assistant.baselink, token, dry_run)?;

    ha.ping()
        .with_context(|| format!("connecting to {}", config.home_assistant.baselink))?;

    if dry_run {
        println!("Running in dry-run mode; no services will be called.");
    }

    // Hysteresis is a margin, so each zone thermostat's previous decision has
    // to be remembered between polls.
    let mut calling: HashMap<String, bool> = HashMap::new();
    let mut throttle = WriteThrottle::new(config.min_write_interval);

    loop {
        println!("===============================================");

        match ha.states() {
            Ok(states) => {
                let demand = apply_zones(&ha, &config, &states, &mut calling, &mut throttle)?;
                apply_main_thermostat(&ha, &config, &states, demand, &mut throttle)?;
            }
            // A transient Home Assistant outage should not kill the daemon.
            Err(e) => eprintln!("error: could not read Home Assistant states: {e:#}"),
        }

        if cli.once {
            return Ok(());
        }

        println!("------------------------------------------------");
        println!("Sleeping {}s...", config.poll_interval);
        sleep(Duration::from_secs(config.poll_interval));
    }
}

fn apply_zones(
    ha: &HomeAssistant,
    config: &Config,
    states: &HashMap<String, Entity>,
    calling: &mut HashMap<String, bool>,
    throttle: &mut WriteThrottle,
) -> Result<bool> {
    let mut any_zone_active = false;

    let mut pumps: Vec<&String> = config.zones.keys().collect();
    pumps.sort();

    for pump in pumps {
        let thermostats = &config.zones[pump];
        let mut zone_active = false;

        for pattern in thermostats {
            let matches = resolve(states, pattern);
            if matches.is_empty() {
                eprintln!("{pattern:<45} no entity matches");
                continue;
            }

            for ent in matches {
                let id = &ent.entity_id;
                if !ent.is_available() {
                    eprintln!("{id:<45} unavailable ({})", ent.state);
                    continue;
                }

                let (Some(curr), Some(setp)) = (
                    ent.attr_f64("current_temperature"),
                    ent.attr_f64("temperature"),
                ) else {
                    eprintln!("{id:<45} missing current_temperature/temperature");
                    continue;
                };

                // Turn on below setpoint-h, off above
                // setpoint+h, and hold the previous decision in between.
                let prev = calling.get(id).copied().unwrap_or(false);
                let now = calls_for_heat(curr, setp, config.hysteresis, prev);
                calling.insert(id.clone(), now);

                if now {
                    zone_active = true;
                }

                let label = match (now, prev) {
                    (true, _) => "demand",
                    (false, true) => "idle (satisfied)",
                    (false, false) => "idle",
                };
                println!("{id:<45} curr {curr:>5.2}°C  set {setp:>5.2}°C  {label}");
            }
        }

        apply_pump(ha, states, pump, zone_active, throttle);

        any_zone_active |= zone_active;
        println!();
    }

    Ok(any_zone_active)
}

fn apply_pump(
    ha: &HomeAssistant,
    states: &HashMap<String, Entity>,
    pump: &str,
    want_on: bool,
    throttle: &mut WriteThrottle,
) {
    let pattern = if pump.contains('.') {
        pump.to_string()
    } else {
        format!("switch.{pump}")
    };

    let matches = resolve(states, &pattern);
    let ent = match matches.len() {
        1 => matches[0],
        0 => {
            eprintln!(
                " -> Pump {pattern}: want {}, but it matches no entity in Home Assistant",
                if want_on { "ON" } else { "off" }
            );
            return;
        }
        _ => {
            eprintln!(
                "    {pattern} is ambiguous, matches {} entities: {}",
                matches.len(),
                matches
                    .iter()
                    .map(|e| e.entity_id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            return;
        }
    };

    let entity_id = ent.entity_id.clone();
    println!(
        " -> Pump {entity_id}: {}",
        if want_on { "ON" } else { "off" }
    );

    if !ent.is_available() {
        eprintln!("    {entity_id} unavailable ({})", ent.state);
        return;
    }

    let is_on = ent.state == "on";
    let key = format!("{entity_id}:{want_on}");

    if is_on == want_on {
        throttle.clear(&key);
        return;
    }

    if throttle.attempted(&key) {
        eprintln!("    {entity_id} did not apply the previous command; retrying");
    }
    if !throttle.allow(&key) {
        if let Some(d) = throttle.retry_in(&key) {
            println!(
                "    {entity_id} write throttled, retrying in {}s",
                d.as_secs()
            );
        }
        return;
    }

    println!(
        "    {entity_id}: {} -> {}",
        ent.state,
        if want_on { "on" } else { "off" }
    );
    if let Err(e) = ha.switch(&entity_id, want_on) {
        eprintln!("    error: {e:#}");
    }
}

/// Drive the main heat pump thermostat to match overall demand.
///
/// Both the mode and the setpoint are compared against what Home Assistant
/// currently reports for the entity, so a command the thermostat quietly
/// dropped is retried on a later poll
fn apply_main_thermostat(
    ha: &HomeAssistant,
    config: &Config,
    states: &HashMap<String, Entity>,
    demand: bool,
    throttle: &mut WriteThrottle,
) -> Result<()> {
    let matches = resolve(states, &config.main);
    let ent = match matches.len() {
        1 => matches[0],
        0 => {
            eprintln!(
                "error: main thermostat {} matches no entity in Home Assistant",
                config.main
            );
            return Ok(());
        }
        _ => {
            eprintln!(
                "error: main thermostat {} is ambiguous, matches: {}",
                config.main,
                matches
                    .iter()
                    .map(|e| e.entity_id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            return Ok(());
        }
    };
    let entity_id = &ent.entity_id;

    if !ent.is_available() {
        eprintln!(
            "warning: main thermostat {entity_id} is {}; skipping",
            ent.state
        );
        return Ok(());
    }

    let actual_mode = ent.hvac_mode().to_string();
    let current = ent.attr_f64("current_temperature");
    let (_, actual_setpoint) = setpoint_field(ent);

    println!(
        "Main thermostat {entity_id}: mode {actual_mode}, preset {}, current {}, setpoint {}",
        ent.attr_str("preset_mode").unwrap_or("n/a"),
        fmt_temp(current),
        fmt_temp(actual_setpoint)
    );
    println!(
        "Overall heat demand: {} -> want mode {}",
        if demand { "YES" } else { "NO" },
        if demand { "heat" } else { "off" }
    );

    if demand && current.is_none() {
        eprintln!(
            "  warning: {entity_id} reports no current_temperature; \
             cannot derive a setpoint this poll"
        );
    }

    for action in plan_main(ent, config, demand) {
        match action {
            Action::Preset(preset) => ensure_preset_mode(ha, ent, &preset, throttle),
            Action::HvacMode(mode) => {
                if !ensure_hvac_mode(ha, entity_id, &actual_mode, &mode, throttle) {
                    break;
                }
            }
            Action::Setpoint { field, value } => {
                ensure_setpoint(ha, entity_id, field, actual_setpoint, value, throttle)
            }
        }
    }

    Ok(())
}

/// One command to send to the main thermostat.
#[derive(Debug, Clone, PartialEq)]
enum Action {
    Preset(String),
    HvacMode(String),
    Setpoint { field: &'static str, value: f64 },
}

/// Decide which commands to send to the main thermostat, **in order**.
///
/// - The hold comes first. On a thermostat that still has a schedule, a
///   setpoint written under `TemporaryHold` is discarded at the next scheduled
///   change, so the hold has to be in place before anything else is written.
/// - Turning on: mode before setpoint. A setpoint written while the thermostat
///   is off is rejected, and Home Assistant reports a null temperature then.
/// - Turning off: setpoint before mode, for the same reason in reverse — an
///   idle setpoint can only be written while the thermostat is still on.
fn plan_main(ent: &Entity, config: &Config, demand: bool) -> Vec<Action> {
    let mut actions = Vec::new();

    if let Some(preset) = &config.preset_mode {
        actions.push(Action::Preset(preset.clone()));
    }

    let (field, _) = setpoint_field(ent);
    let setpoint = ent
        .attr_f64("current_temperature")
        .map(|current| Action::Setpoint {
            field,
            value: target_setpoint(current, config, demand),
        });

    if demand {
        actions.push(Action::HvacMode("heat".to_string()));
        actions.extend(setpoint);
    } else {
        if ent.hvac_mode() != "off" {
            actions.extend(setpoint);
        }
        actions.push(Action::HvacMode("off".to_string()));
    }

    actions
}

fn setpoint_field(ent: &Entity) -> (&'static str, Option<f64>) {
    if let Some(t) = ent.attr_f64("temperature") {
        return ("temperature", Some(t));
    }
    // In a dual-setpoint mode (heat_cool / auto) Home Assistant reports null for
    // `temperature` and exposes target_temp_low/high instead
    if let Some(low) = ent.attr_f64("target_temp_low") {
        eprintln!(
            "warning: {} is in a dual-setpoint mode; using target_temp_low. \
             Set the thermostat to heat-only for reliable control.",
            ent.entity_id
        );
        return ("target_temp_low", Some(low));
    }
    ("temperature", None)
}

fn ensure_hvac_mode(
    ha: &HomeAssistant,
    entity_id: &str,
    actual: &str,
    desired: &str,
    throttle: &mut WriteThrottle,
) -> bool {
    let key = format!("{entity_id}:hvac_mode:{desired}");

    if actual == desired {
        throttle.clear(&key);
        return true;
    }

    if throttle.attempted(&key) {
        eprintln!(
            "  warning: {entity_id} is still in mode `{actual}` after an earlier \
             set_hvac_mode to `{desired}`; retrying"
        );
    }
    if !throttle.allow(&key) {
        if let Some(d) = throttle.retry_in(&key) {
            println!("  set_hvac_mode throttled, retrying in {}s", d.as_secs());
        }
        return false;
    }

    println!("  set_hvac_mode: {actual} -> {desired}");
    match ha.set_hvac_mode(entity_id, desired) {
        Ok(()) => true,
        Err(e) => {
            eprintln!("  error: {e:#}");
            false
        }
    }
}

fn ensure_setpoint(
    ha: &HomeAssistant,
    entity_id: &str,
    field: &str,
    actual: Option<f64>,
    desired: f64,
    throttle: &mut WriteThrottle,
) {
    let key = format!("{entity_id}:{field}:{desired:.1}");

    if actual.is_some_and(|a| (a - desired).abs() <= TEMP_EPSILON) {
        throttle.clear(&key);
        return;
    }

    if throttle.attempted(&key) {
        eprintln!(
            "  warning: {entity_id} {field} is still {} after an earlier \
             set_temperature to {desired:.1}°C; retrying",
            fmt_temp(actual)
        );
    }
    if !throttle.allow(&key) {
        if let Some(d) = throttle.retry_in(&key) {
            println!("  set_temperature throttled, retrying in {}s", d.as_secs());
        }
        return;
    }

    println!(
        "  set_temperature: {field} {} -> {desired:.1}°C",
        fmt_temp(actual)
    );
    if let Err(e) = ha.set_temperature(entity_id, field, desired) {
        eprintln!("  error: {e:#}");
    }
}

fn fmt_temp(v: Option<f64>) -> String {
    match v {
        Some(v) => format!("{v:.2}°C"),
        None => "n/a".to_string(),
    }
}

/// Hold the thermostat in a preset (typically `PermanentHold`)
fn ensure_preset_mode(
    ha: &HomeAssistant,
    ent: &Entity,
    desired: &str,
    throttle: &mut WriteThrottle,
) {
    let entity_id = &ent.entity_id;
    let supported = ent.preset_modes();
    if !supported.is_empty() && !supported.contains(&desired) {
        eprintln!(
            "warning: {entity_id} does not support preset `{desired}` (supports: {}); \
             ignoring preset_mode",
            supported.join(", ")
        );
        return;
    }

    let actual = ent.attr_str("preset_mode").unwrap_or("unknown");
    let key = format!("{entity_id}:preset_mode:{desired}");

    if actual == desired {
        throttle.clear(&key);
        return;
    }

    if throttle.attempted(&key) {
        eprintln!(
            "  warning: {entity_id} preset is still `{actual}` after an earlier \
             set_preset_mode to `{desired}`; retrying"
        );
    }
    if !throttle.allow(&key) {
        if let Some(d) = throttle.retry_in(&key) {
            println!("  set_preset_mode throttled, retrying in {}s", d.as_secs());
        }
        return;
    }

    println!("  set_preset_mode: {actual} -> {desired}");
    if let Err(e) = ha.set_preset_mode(entity_id, desired) {
        eprintln!("  error: {e:#}");
    }
}

/// Starts calling below `setpoint - hysteresis`, stops above
/// `setpoint + hysteresis`, and holds `previous` in between
fn calls_for_heat(current: f64, setpoint: f64, hysteresis: f64, previous: bool) -> bool {
    if current < setpoint - hysteresis {
        true
    } else if current > setpoint + hysteresis {
        false
    } else {
        previous
    }
}

/// Setpoint to write to the main thermostat.
///
/// Calling for heat: its own measured temperature plus `temp_increment`, which
/// puts the target above the room and keeps the heat pump running.
/// Idle: its own measured temperature, meaning do not heat
fn target_setpoint(current: f64, config: &Config, demand: bool) -> f64 {
    if demand {
        current + config.temp_increment
    } else {
        current
    }
}

fn glob_match(pattern: &str, text: &str) -> bool {
    let pattern = pattern.to_ascii_lowercase();
    let text = text.to_ascii_lowercase();

    if !pattern.contains('*') {
        return pattern == text;
    }

    let parts: Vec<&str> = pattern.split('*').collect();
    let last = parts.len() - 1;
    let mut pos = 0usize;

    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        if i == 0 {
            if !text.starts_with(part) {
                return false;
            }
            pos = part.len();
        } else if i == last {
            return text.len() >= pos + part.len() && text[pos..].ends_with(part);
        } else {
            match text[pos..].find(part) {
                Some(idx) => pos += idx + part.len(),
                None => return false,
            }
        }
    }
    true
}

/// Resolve a configured entity pattern to the entities it matches.
///
/// A pattern without `*` is an exact entity id. Otherwise it is matched against
/// both the entity id and the friendly name
fn resolve<'a>(states: &'a HashMap<String, Entity>, pattern: &str) -> Vec<&'a Entity> {
    if !pattern.contains('*') {
        return states.get(pattern).into_iter().collect();
    }

    let mut hits: Vec<&Entity> = states
        .values()
        .filter(|e| {
            glob_match(pattern, &e.entity_id)
                || e.attr_str("friendly_name")
                    .is_some_and(|n| glob_match(pattern, n))
        })
        .collect();
    hits.sort_by(|a, b| a.entity_id.cmp(&b.entity_id));
    hits
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn config_with(temp_increment: f64) -> Config {
        toml::from_str(&format!(
            r#"
            main = "climate.test"
            temp_increment = {temp_increment}
            [home_assistant]
            baselink = "http://localhost:8123"
            "#
        ))
        .expect("test config should parse")
    }

    // --- zone margin -----------------------------------------------------

    #[test]
    fn margin_turns_on_well_below_setpoint() {
        assert!(calls_for_heat(18.0, 20.0, 0.2, false));
    }

    #[test]
    fn margin_turns_off_well_above_setpoint() {
        assert!(!calls_for_heat(22.0, 20.0, 0.2, true));
    }

    #[test]
    fn margin_holds_previous_state_inside_the_band() {
        // Inside [19.8, 20.2] the previous decision must be preserved, which is
        // what stops the pump chattering at the setpoint.
        assert!(calls_for_heat(19.9, 20.0, 0.2, true));
        assert!(!calls_for_heat(19.9, 20.0, 0.2, false));
        assert!(calls_for_heat(20.1, 20.0, 0.2, true));
        assert!(!calls_for_heat(20.1, 20.0, 0.2, false));
    }

    #[test]
    fn margin_is_symmetric_around_the_setpoint() {
        assert!(!calls_for_heat(19.8, 20.0, 0.2, false));
        assert!(calls_for_heat(19.79, 20.0, 0.2, false));
        assert!(calls_for_heat(20.2, 20.0, 0.2, true));
        assert!(!calls_for_heat(20.21, 20.0, 0.2, true));
    }

    #[test]
    fn zero_hysteresis_is_a_plain_threshold() {
        assert!(calls_for_heat(19.9, 20.0, 0.0, false));
        assert!(!calls_for_heat(20.1, 20.0, 0.0, true));
    }

    // --- main setpoint -----------------------------------------------------

    #[test]
    fn calling_for_heat_targets_measured_plus_increment() {
        let cfg = config_with(0.5);
        assert_eq!(target_setpoint(20.0, &cfg, true), 20.5);
        assert_eq!(target_setpoint(21.4, &cfg, true), 21.9);
    }

    #[test]
    fn idle_targets_the_measured_temperature_itself() {
        // Idling is just asking the thermostat for exactly what it already has.
        let cfg = config_with(0.5);
        assert_eq!(target_setpoint(20.0, &cfg, false), 20.0);
        assert_eq!(target_setpoint(21.4, &cfg, false), 21.4);
    }

    // --- command ordering (the Lyric bug) ---

    fn lyric(mode: &str, preset: &str) -> Entity {
        Entity {
            entity_id: "climate.test".to_string(),
            state: mode.to_string(),
            attributes: json!({
                "min_temp": 5.0,
                "max_temp": 35.0,
                "current_temperature": 20.0,
                "preset_mode": preset,
                "preset_modes": ["NoHold", "PermanentHold", "TemporaryHold"],
            }),
        }
    }

    fn config_held(temp_increment: f64) -> Config {
        toml::from_str(&format!(
            r#"
            main = "climate.test"
            temp_increment = {temp_increment}
            preset_mode = "PermanentHold"
            [home_assistant]
            baselink = "http://localhost:8123"
            "#
        ))
        .expect("test config should parse")
    }

    #[test]
    fn turning_on_holds_then_sets_mode_then_setpoint() {
        // The original bug: the setpoint was written first, while the
        // thermostat was still off, and was therefore discarded.
        let plan = plan_main(&lyric("off", "TemporaryHold"), &config_held(0.5), true);
        assert_eq!(
            plan,
            vec![
                Action::Preset("PermanentHold".to_string()),
                Action::HvacMode("heat".to_string()),
                Action::Setpoint {
                    field: "temperature",
                    value: 20.5
                },
            ]
        );
    }

    #[test]
    fn going_idle_sets_the_measured_temperature_then_switches_off() {
        // Mirror image: the setpoint only applies while the thermostat is still
        // on, and idling means asking it for exactly what it already measures.
        let plan = plan_main(&lyric("heat", "PermanentHold"), &config_held(0.5), false);
        assert_eq!(
            plan,
            vec![
                Action::Preset("PermanentHold".to_string()),
                Action::Setpoint {
                    field: "temperature",
                    value: 20.0
                },
                Action::HvacMode("off".to_string()),
            ]
        );
    }

    #[test]
    fn already_off_only_keeps_the_hold() {
        let plan = plan_main(&lyric("off", "PermanentHold"), &config_held(0.5), false);
        assert_eq!(
            plan,
            vec![
                Action::Preset("PermanentHold".to_string()),
                Action::HvacMode("off".to_string()),
            ]
        );
    }

    #[test]
    fn no_preset_configured_emits_no_preset_action() {
        let cfg = config_with(0.5);
        let plan = plan_main(&lyric("heat", "TemporaryHold"), &cfg, true);
        assert!(!plan.iter().any(|a| matches!(a, Action::Preset(_))));
        assert_eq!(plan[0], Action::HvacMode("heat".to_string()));
    }

    // --- entity patterns ---------------------------------------------------

    #[test]
    fn glob_without_star_is_an_exact_match() {
        assert!(glob_match("switch.pomp_washok", "switch.pomp_washok"));
        assert!(!glob_match("switch.pomp_washok", "switch.pomp_washok_none"));
    }

    #[test]
    fn glob_matches_renamed_entities() {
        assert!(glob_match("switch.pomp_washok*", "switch.pomp_washok_none"));
        assert!(glob_match("switch.*washok*", "switch.pomp_washok_none"));
        assert!(glob_match(
            "switch.*achterhuis*pomp*",
            "switch.stekkerschakelaar_achterhuis_pomp_none"
        ));
    }

    #[test]
    fn glob_is_case_insensitive_and_anchored() {
        assert!(glob_match("SWITCH.*Washok*", "switch.pomp_washok_none"));
        assert!(!glob_match("switch.*washok", "switch.pomp_washok_none"));
        assert!(!glob_match("*boven*", "switch.pomp_washok_none"));
        assert!(glob_match("*", "anything.at.all"));
    }

    #[test]
    fn resolve_matches_entity_id_and_friendly_name() {
        let mut states = HashMap::new();
        states.insert(
            "switch.abc123".to_string(),
            Entity {
                entity_id: "switch.abc123".to_string(),
                state: "off".to_string(),
                attributes: json!({ "friendly_name": "Pomp washok" }),
            },
        );

        assert_eq!(resolve(&states, "*washok*").len(), 1);
        assert_eq!(resolve(&states, "switch.abc123").len(), 1);
        assert_eq!(resolve(&states, "*boven*").len(), 0);
    }
}
