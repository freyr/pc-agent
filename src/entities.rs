use serde_json::{Value, json};

use crate::config::Config;

/// Seconds after which Home Assistant marks a live entity unavailable when no
/// new state arrives.
const LIVE_EXPIRE_AFTER: u32 = 120;

/// The MQTT discovery component of an entity.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Component {
    Sensor,
    #[allow(dead_code)]
    BinarySensor,
}

impl Component {
    pub fn as_str(self) -> &'static str {
        match self {
            Component::Sensor => "sensor",
            Component::BinarySensor => "binary_sensor",
        }
    }
}

/// One Home Assistant entity that pc-agent can publish under
/// `homeassistant/<component>/<device name>/<name>/`. The table `ENTITIES` is
/// data: which entities the agent publishes depends on the sources that exist.
pub struct Entity {
    pub name: &'static str,
    pub component: Component,
    pub device_class: Option<&'static str>,
    pub icon: &'static str,
    pub unit: Option<&'static str>,
    pub state_class: Option<&'static str>,
    /// `payload_on` and `payload_off` of a binary sensor.
    pub payloads: Option<(&'static str, &'static str)>,
    pub expire_after: Option<u32>,
    /// A new entity (not a HASS.Agent one) sets `default_entity_id` from `entity_prefix`.
    pub default_entity_id: bool,
}

const fn sensor(name: &'static str, icon: &'static str) -> Entity {
    Entity {
        name,
        component: Component::Sensor,
        device_class: None,
        icon,
        unit: None,
        state_class: None,
        payloads: None,
        expire_after: None,
        default_entity_id: false,
    }
}

pub const SESSIONSTATE: Entity = sensor("sessionstate", "mdi:lock");

// No source supplies this entity yet; the allow goes when a task adds it to PUBLISHED.
#[allow(dead_code)]
pub const LASTACTIVE: Entity = Entity {
    device_class: Some("timestamp"),
    ..sensor("lastactive", "mdi:clock-time-three-outline")
};

pub const LASTBOOT: Entity = Entity {
    device_class: Some("timestamp"),
    ..sensor("lastboot", "mdi:clock-time-three-outline")
};

// No source supplies this entity yet; the allow goes when a task adds it to PUBLISHED.
#[allow(dead_code)]
pub const LASTSYSTEMSTATECHANGE: Entity = sensor("lastsystemstatechange", "mdi:cog");

// No source supplies this entity yet; the allow goes when a task adds it to PUBLISHED.
#[allow(dead_code)]
pub const GPULOAD: Entity = Entity {
    unit: Some("%"),
    state_class: Some("measurement"),
    expire_after: Some(LIVE_EXPIRE_AFTER),
    default_entity_id: true,
    ..sensor("gpuload", "mdi:expansion-card")
};

// No source supplies this entity yet; the allow goes when a task adds it to PUBLISHED.
#[allow(dead_code)]
pub const GPUPOWER: Entity = Entity {
    unit: Some("W"),
    device_class: Some("power"),
    state_class: Some("measurement"),
    expire_after: Some(LIVE_EXPIRE_AFTER),
    default_entity_id: true,
    ..sensor("gpupower", "mdi:flash")
};

// No source supplies this entity yet; the allow goes when a task adds it to PUBLISHED.
#[allow(dead_code)]
pub const LOGGEDUSERS: Entity = Entity {
    expire_after: Some(LIVE_EXPIRE_AFTER),
    default_entity_id: true,
    ..sensor("loggedusers", "mdi:account-group")
};

// No source supplies this entity yet; the allow goes when a task adds it to PUBLISHED.
#[allow(dead_code)]
pub const DISPLAY: Entity = Entity {
    component: Component::BinarySensor,
    payloads: Some(("ON", "OFF")),
    expire_after: Some(LIVE_EXPIRE_AFTER),
    default_entity_id: true,
    ..sensor("display", "mdi:monitor")
};

/// The entity table of the spec.
// Read by the tests; the agent publishes the entities of PUBLISHED.
#[allow(dead_code)]
pub const ENTITIES: [Entity; 8] = [
    SESSIONSTATE,
    LASTACTIVE,
    LASTBOOT,
    LASTSYSTEMSTATECHANGE,
    GPULOAD,
    GPUPOWER,
    LOGGEDUSERS,
    DISPLAY,
];

/// The entities that have a source today. A task that adds a source adds its
/// entity here.
pub const PUBLISHED: [&Entity; 2] = [&LASTBOOT, &SESSIONSTATE];

/// The availability topic is under `sensor` for every entity, also for a binary sensor.
pub fn availability_topic(device: &str) -> String {
    format!("homeassistant/sensor/{device}/availability")
}

pub fn config_topic(device: &str, entity: &Entity) -> String {
    format!(
        "homeassistant/{}/{device}/{}/config",
        entity.component.as_str(),
        entity.name
    )
}

pub fn state_topic(device: &str, entity: &Entity) -> String {
    format!(
        "homeassistant/{}/{device}/{}/state",
        entity.component.as_str(),
        entity.name
    )
}

/// The MQTT discovery configuration of one entity. `model` is the device model
/// that the OS module of `sources` gives.
pub fn discovery_payload(cfg: &Config, entity: &Entity, model: &str) -> Value {
    let device = &cfg.device.name;
    let mut payload = json!({
        "name": entity.name,
        "unique_id": cfg.unique_id(entity.name),
        "state_topic": state_topic(device, entity),
        "availability_topic": availability_topic(device),
        "icon": entity.icon,
        "device": {
            "identifiers": cfg.device.identifier,
            "name": device,
            "manufacturer": "pc-agent",
            "model": model,
            "sw_version": env!("CARGO_PKG_VERSION"),
        },
    });
    if let Some(class) = entity.device_class {
        payload["device_class"] = json!(class);
    }
    if let Some(unit) = entity.unit {
        payload["unit_of_measurement"] = json!(unit);
    }
    if let Some(class) = entity.state_class {
        payload["state_class"] = json!(class);
    }
    if let Some((on, off)) = entity.payloads {
        payload["payload_on"] = json!(on);
        payload["payload_off"] = json!(off);
    }
    if let Some(seconds) = entity.expire_after {
        payload["expire_after"] = json!(seconds);
    }
    if entity.default_entity_id {
        payload["default_entity_id"] = json!(format!(
            "{}.{}_{}",
            entity.component.as_str(),
            cfg.device.entity_prefix,
            entity.name
        ));
    }
    payload
}

#[cfg(test)]
mod tests {
    use super::*;

    const JAHURTX: &str = r#"
[mqtt]
host = "broker.example"
port = 1883
username = "u"
password = "p"

[device]
name = "JAHURTX"
identifier = "hass.agent-JAHURTX"
entity_prefix = "jahu_rtx"

[unique_ids]
sessionstate = "e6996b71-cdff-4e58-8f48-455f5670f921"
lastactive = "3e006c78-3ee9-4b91-b9d9-59f25c9ab5ce"
lastboot = "376b6cb4-4e28-4b98-b968-29c6e8fa820f"
lastsystemstatechange = "6e2d5760-f4a1-4351-9548-299067464890"
"#;

    const MODEL: &str = "3.1.0";

    fn payload(name: &str) -> Value {
        let cfg = Config::parse(JAHURTX).unwrap();
        let entity = ENTITIES.iter().find(|e| e.name == name).unwrap();
        discovery_payload(&cfg, entity, MODEL)
    }

    fn keys(v: &Value) -> Vec<&str> {
        let mut k: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        k.sort();
        k
    }

    #[test]
    fn table_has_the_eight_entities_in_order() {
        let names: Vec<&str> = ENTITIES.iter().map(|e| e.name).collect();
        assert_eq!(
            names,
            [
                "sessionstate",
                "lastactive",
                "lastboot",
                "lastsystemstatechange",
                "gpuload",
                "gpupower",
                "loggedusers",
                "display"
            ]
        );
    }

    #[test]
    fn hass_agent_sensors_keep_the_hass_agent_contract() {
        // name, unique_id, device_class, icon: research-hass-agent-contract.
        let expected = [
            (
                "sessionstate",
                "e6996b71-cdff-4e58-8f48-455f5670f921",
                None,
                "mdi:lock",
            ),
            (
                "lastactive",
                "3e006c78-3ee9-4b91-b9d9-59f25c9ab5ce",
                Some("timestamp"),
                "mdi:clock-time-three-outline",
            ),
            (
                "lastboot",
                "376b6cb4-4e28-4b98-b968-29c6e8fa820f",
                Some("timestamp"),
                "mdi:clock-time-three-outline",
            ),
            (
                "lastsystemstatechange",
                "6e2d5760-f4a1-4351-9548-299067464890",
                None,
                "mdi:cog",
            ),
        ];
        for (name, unique_id, class, icon) in expected {
            let v = payload(name);
            assert_eq!(v["unique_id"], unique_id, "{name}");
            assert_eq!(
                v["state_topic"],
                format!("homeassistant/sensor/JAHURTX/{name}/state"),
                "{name}"
            );
            assert_eq!(
                v["availability_topic"], "homeassistant/sensor/JAHURTX/availability",
                "{name}"
            );
            assert_eq!(
                v.get("device_class").and_then(Value::as_str),
                class,
                "{name}"
            );
            assert_eq!(v["icon"], icon, "{name}");
            assert!(v.get("default_entity_id").is_none(), "{name}");
            assert!(v.get("expire_after").is_none(), "{name}");
        }
    }

    #[test]
    fn existing_sensor_has_exactly_the_expected_keys() {
        assert_eq!(
            keys(&payload("lastboot")),
            [
                "availability_topic",
                "device",
                "device_class",
                "icon",
                "name",
                "state_topic",
                "unique_id"
            ]
        );
        assert_eq!(
            keys(&payload("sessionstate")),
            [
                "availability_topic",
                "device",
                "icon",
                "name",
                "state_topic",
                "unique_id"
            ]
        );
    }

    #[test]
    fn new_entities_have_default_entity_id_and_expire_after() {
        let expected = [
            ("gpuload", "sensor.jahu_rtx_gpuload"),
            ("gpupower", "sensor.jahu_rtx_gpupower"),
            ("loggedusers", "sensor.jahu_rtx_loggedusers"),
            ("display", "binary_sensor.jahu_rtx_display"),
        ];
        for (name, id) in expected {
            let v = payload(name);
            assert_eq!(v["default_entity_id"], id, "{name}");
            assert_eq!(v["expire_after"], 120, "{name}");
            assert_eq!(v["unique_id"], format!("pc-agent-JAHURTX-{name}"), "{name}");
        }
    }

    #[test]
    fn gpuload_keys() {
        let v = payload("gpuload");
        assert_eq!(v["unit_of_measurement"], "%");
        assert_eq!(v["state_class"], "measurement");
        assert_eq!(v["icon"], "mdi:expansion-card");
        assert!(v.get("device_class").is_none());
        assert_eq!(
            v["state_topic"],
            "homeassistant/sensor/JAHURTX/gpuload/state"
        );
    }

    #[test]
    fn gpupower_keys() {
        let v = payload("gpupower");
        assert_eq!(v["unit_of_measurement"], "W");
        assert_eq!(v["device_class"], "power");
        assert_eq!(v["state_class"], "measurement");
        assert_eq!(v["icon"], "mdi:flash");
    }

    #[test]
    fn loggedusers_keys() {
        let v = payload("loggedusers");
        assert_eq!(v["icon"], "mdi:account-group");
        assert!(v.get("unit_of_measurement").is_none());
        assert!(v.get("state_class").is_none());
    }

    #[test]
    fn display_is_a_binary_sensor_with_the_sensor_availability_topic() {
        let v = payload("display");
        assert_eq!(v["payload_on"], "ON");
        assert_eq!(v["payload_off"], "OFF");
        assert_eq!(v["icon"], "mdi:monitor");
        assert_eq!(
            v["state_topic"],
            "homeassistant/binary_sensor/JAHURTX/display/state"
        );
        assert_eq!(
            v["availability_topic"],
            "homeassistant/sensor/JAHURTX/availability"
        );
        assert_eq!(
            config_topic("JAHURTX", &DISPLAY),
            "homeassistant/binary_sensor/JAHURTX/display/config"
        );
    }

    #[test]
    fn device_block_has_the_spec_keys() {
        for e in &ENTITIES {
            let cfg = Config::parse(JAHURTX).unwrap();
            let v = discovery_payload(&cfg, e, MODEL);
            let d = &v["device"];
            assert_eq!(
                keys(d),
                ["identifiers", "manufacturer", "model", "name", "sw_version"]
            );
            assert_eq!(d["identifiers"], "hass.agent-JAHURTX");
            assert_eq!(d["name"], "JAHURTX");
            assert_eq!(d["manufacturer"], "pc-agent");
            assert_eq!(d["model"], "3.1.0");
            assert_eq!(d["sw_version"], env!("CARGO_PKG_VERSION"));
        }
    }

    #[test]
    fn no_topic_uses_hass_agent_or_button_prefixes() {
        let cfg = Config::parse(JAHURTX).unwrap();
        for e in &ENTITIES {
            let v = discovery_payload(&cfg, e, MODEL);
            let topics = [
                config_topic("JAHURTX", e),
                state_topic("JAHURTX", e),
                v["state_topic"].as_str().unwrap().to_string(),
                v["availability_topic"].as_str().unwrap().to_string(),
            ];
            for t in topics {
                assert!(t.starts_with("homeassistant/"), "{t}");
                assert!(!t.starts_with("homeassistant/button/"), "{t}");
                assert!(!t.starts_with("hass.agent/"), "{t}");
            }
        }
    }

    #[test]
    fn published_entities_are_the_ones_with_a_source() {
        let names: Vec<&str> = PUBLISHED.iter().map(|e| e.name).collect();
        assert_eq!(names, ["lastboot", "sessionstate"]);
    }
}
