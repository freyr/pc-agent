use serde_json::{Value, json};

use crate::config::Config;

/// A Home Assistant sensor that pc-agent publishes under
/// `homeassistant/sensor/<device name>/<name>/`.
pub struct Sensor {
    pub name: &'static str,
    pub device_class: Option<&'static str>,
    pub icon: &'static str,
}

pub const LASTBOOT: Sensor = Sensor {
    name: "lastboot",
    device_class: Some("timestamp"),
    icon: "mdi:clock-time-three-outline",
};

pub const SESSIONSTATE: Sensor = Sensor {
    name: "sessionstate",
    device_class: None,
    icon: "mdi:lock",
};

pub const SENSORS: [&Sensor; 2] = [&LASTBOOT, &SESSIONSTATE];

pub fn availability_topic(device: &str) -> String {
    format!("homeassistant/sensor/{device}/availability")
}

pub fn config_topic(device: &str, name: &str) -> String {
    format!("homeassistant/sensor/{device}/{name}/config")
}

pub fn state_topic(device: &str, name: &str) -> String {
    format!("homeassistant/sensor/{device}/{name}/state")
}

/// The MQTT discovery configuration of one sensor.
pub fn discovery_payload(cfg: &Config, sensor: &Sensor) -> Value {
    let device = &cfg.device.name;
    let mut payload = json!({
        "name": sensor.name,
        "unique_id": cfg.unique_id(sensor.name),
        "state_topic": state_topic(device, sensor.name),
        "availability_topic": availability_topic(device),
        "icon": sensor.icon,
        "device": {
            "identifiers": cfg.device.identifier,
            "name": device,
            "manufacturer": "pc-agent",
            "sw_version": env!("CARGO_PKG_VERSION"),
        },
    });
    if let Some(class) = sensor.device_class {
        payload["device_class"] = json!(class);
    }
    payload
}

#[cfg(test)]
mod tests {
    use super::*;

    const CFG: &str = r#"
[mqtt]
host = "broker.example"
port = 1883
username = "u"
password = "p"

[device]
name = "JAHURTX-TEST"
identifier = "pc-agent-JAHURTX-TEST"
entity_prefix = "jahurtx_test"

[unique_ids]
sessionstate = "e6996b71-cdff-4e58-8f48-455f5670f921"

[gpu]
enabled = false
"#;

    fn keys(v: &Value) -> Vec<&str> {
        let mut k: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        k.sort();
        k
    }

    fn assert_device(v: &Value) {
        let d = &v["device"];
        assert_eq!(
            keys(d),
            ["identifiers", "manufacturer", "name", "sw_version"]
        );
        assert_eq!(d["identifiers"], "pc-agent-JAHURTX-TEST");
        assert_eq!(d["name"], "JAHURTX-TEST");
        assert_eq!(d["manufacturer"], "pc-agent");
        assert_eq!(d["sw_version"], env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn lastboot_payload() {
        let cfg = Config::parse(CFG).unwrap();
        let v = discovery_payload(&cfg, &LASTBOOT);
        assert_eq!(
            keys(&v),
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
        assert_eq!(v["name"], "lastboot");
        assert_eq!(v["unique_id"], "pc-agent-JAHURTX-TEST-lastboot");
        assert_eq!(
            v["state_topic"],
            "homeassistant/sensor/JAHURTX-TEST/lastboot/state"
        );
        assert_eq!(
            v["availability_topic"],
            "homeassistant/sensor/JAHURTX-TEST/availability"
        );
        assert_eq!(v["device_class"], "timestamp");
        assert_eq!(v["icon"], "mdi:clock-time-three-outline");
        assert_device(&v);
    }

    #[test]
    fn sessionstate_payload() {
        let cfg = Config::parse(CFG).unwrap();
        let v = discovery_payload(&cfg, &SESSIONSTATE);
        assert_eq!(
            keys(&v),
            [
                "availability_topic",
                "device",
                "icon",
                "name",
                "state_topic",
                "unique_id"
            ]
        );
        assert_eq!(v["name"], "sessionstate");
        assert_eq!(v["unique_id"], "e6996b71-cdff-4e58-8f48-455f5670f921");
        assert_eq!(
            v["state_topic"],
            "homeassistant/sensor/JAHURTX-TEST/sessionstate/state"
        );
        assert_eq!(
            v["availability_topic"],
            "homeassistant/sensor/JAHURTX-TEST/availability"
        );
        assert_eq!(v["icon"], "mdi:lock");
        assert_device(&v);
    }

    #[test]
    fn topics() {
        assert_eq!(
            config_topic("D", "lastboot"),
            "homeassistant/sensor/D/lastboot/config"
        );
    }
}
