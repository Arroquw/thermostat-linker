# thermostat-linker

Link pump-based floor heating thermostats to a Honeywell lyric (heat pump) thermostat

## Why

Old floor heating thermostats control a heater through a 230 VAC socket,
and the boiler fired whenever any zone wanted heat.
A heat pump cannot be switched like that. Usually it gets its own thermostat.
Thermostat-linker aims to link two of those incompatible systems together.

It polls Home Assistant, works out whether any Livisi zone is calling for heat,
switches that zone's circulation pump, and drives the Lyric's HVAC mode
to `heat` (or `off`) to match overall demand.

## How it works

Every `poll_interval` seconds:

1. Read all entity states from `/api/states`.
2. For each zone thermostat, decide whether it calls for heat using a margin around its setpoint.
3. Switch each zone's pump on while any of its thermostats calls for heat.
4. Drive the main (Lyric) thermostat: hold preset, then mode `heat` followed by a
   setpoint of current temp + configurable increment — or, with no pump on, a
   setpoint of current temp followed by mode `off`.

Every write is compared against the state Home Assistant actually reports, so a
command the thermostat quietly dropped is retried on a later poll.

## Requirements

- Rust 1.82 or newer, and a C compiler. Verified on Debian 12 with cargo 1.91.
- A Home Assistant instance with the Lyric and Livisi (or other pump based thermostat system) integrations set up, and a
  [long-lived access token](https://www.home-assistant.io/docs/authentication/#your-account-profile).

## Install

```sh
git clone https://github.com/Arroquw/thermostat-linker
cd thermostat-linker
cargo build --release
sudo install -m 0755 target/release/thermostat-linkerd /usr/local/bin/
```

## Configure

```sh
mkdir -p ~/.config/thermostat-linker
cp Config.example.toml ~/.config/thermostat-linker/Config.toml
$EDITOR ~/.config/thermostat-linker/Config.toml
```

The config file is read from `~/.config/thermostat-linker/Config.toml` by
default; pass `--config <FILE>` to use another path.

See [`Config.example.toml`](Config.example.toml) for every option.

Keys that impact functionality the most:

| Key | Meaning |
| --- | --- |
| `main` | Entity id of the Lyric thermostat that controls the heat pump. |
| `temp_increment` | Added to `main`'s own measured temperature while heating. |
| `preset_mode` | Hold to keep `main` in, normally `PermanentHold`. See below. |
| `hysteresis` | Deadband around each zone setpoint, in degrees. |
| `min_write_interval` | Interval to attempt to re-send failed commands. Default 5 min. |
| `[zones]` | Pump → list of zone thermostats it serves. |

### Entity patterns

Any entity id in the config may be a pattern, in which `*` matches any run of
characters. Patterns are matched case-insensitively against both the entity id
and the friendly name:

```toml
[zones]
    "switch.*washroom*" = ["climate.kitchen_kitchen*", "climate.hall*"]
```

A pump pattern must resolve to exactly one entity, or it is skipped with an
error rather than risk switching the wrong pump. A thermostat pattern may match
several, and all of them are evaluated — so keep them tight enough not to catch
a neighbour (`climate.bathroom*` also matches `climate.bathroom_upstairs_*`).
A zone key without a dot is taken as `switch.<key>`.

### Finding your entity ids

```sh
curl -s -H "Authorization: Bearer $HA_TOKEN" \
  http://homeassistant.local:8123/api/states \
  | jq -r '.[] | select(.entity_id|startswith("climate.")) | .entity_id'
```

## The access token

The token is resolved from the first of these that yields a value:

1. `token_env` — name of an environment variable holding the token.
2. `token_command` — a command whose stdout is the token.
3. `token_file` — path to a file containing it; must be mode `0600`.
4. `token` — inline in the config. Warns on startup; avoid it.

If none is configured, `$HA_TOKEN` is used when set.

Listing more than one is fine and lets a single config work both under systemd
and by hand:

```toml
[home_assistant]
baselink   = "http://192.168.1.85:8123"
token_env  = "HA_TOKEN"
token_file = "~/.config/thermostat-linker/token"
```

With a password manager:

```toml
token_command = "pass show home-assistant/token"
```

## Running

```sh
thermostat-linkerd                       # default config path
thermostat-linkerd -c /etc/thermostat-linker/Config.toml
thermostat-linkerd --dry-run --once      # one poll, no service calls
```

`--dry-run` logs every service call it would make without making it, and
`--once` runs a single poll and exits. Together they are the safe way to check a
config change.

### As a systemd service

```sh
sudo useradd --system --no-create-home --shell /usr/sbin/nologin thermostat
sudo install -d -m 0750 -o root -g thermostat /etc/thermostat-linker
sudo install -m 0640 -o root -g thermostat Config.example.toml \
     /etc/thermostat-linker/Config.toml
sudo $EDITOR /etc/thermostat-linker/Config.toml

# Put the token in the environment file rather than the config
printf 'HA_TOKEN=%s\n' 'dhKltNurQn...' | sudo tee /etc/thermostat-linker/env >/dev/null
sudo chown root:thermostat /etc/thermostat-linker/env
sudo chmod 0640 /etc/thermostat-linker/env

sudo cp contrib/thermostat-linkerd.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now thermostat-linkerd
journalctl -u thermostat-linkerd -f
```

## The main setpoint

The main thermostat is only ever in one of two states, and both are expressed
relative to the temperature it measures itself:

| | Setpoint | HVAC mode | Preset |
| --- | --- | --- | --- |
| Any pump on | measured + `temp_increment` | `heat` | `preset_mode` |
| No pump on | measured | `off` | `preset_mode` |

## Honeywell Lyric quirks

The Lyric is a cloud device, and it does not behave like a local thermostat.
Three things make it look like it is ignoring commands:

- **It resumes its own schedule.** If the thermostat still has a schedule, every
  setpoint written through the API lands as a `TemporaryHold` and is discarded at
  the next scheduled change. Setting `preset_mode = "PermanentHold"` makes the
  hold stick. You can see the current behaviour in the entity's `preset_mode`
  attribute and in `sensor.<name>_setpoint_status`.
- **A setpoint written while it is off is ignored.** When the Lyric is `off`,
  Home Assistant reports `temperature: null` and the setpoint cannot be changed.
  `thermostat-linkerd` therefore sets the HVAC mode to `heat` first and the
  setpoint second, and when going idle writes the setpoint *before* switching
  off.
- **The cloud API rate limits.** `min_write_interval` (default 300 s) stops the
  daemon re-sending a command the thermostat keeps refusing.

If a thermostat is in a dual-setpoint mode (`heat_cool` / `auto`), Home Assistant
reports `temperature: null` and exposes `target_temp_low` / `target_temp_high`
instead. The daemon warns and falls back to `target_temp_low`; setting the
thermostat to heat-only is more predictable.

## Development

```sh
cargo fmt --all
cargo clippy --all-targets -- -D warnings
cargo test --all
```

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).
