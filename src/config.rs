use std::collections::HashMap;
use std::fmt;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

pub const DEFAULT_PATH: &str = "/etc/pc-agent/config.toml";

#[derive(Debug, Deserialize)]
pub struct Config {
    pub mqtt: Mqtt,
    pub device: Device,
    #[serde(default)]
    pub unique_ids: HashMap<String, String>,
    #[serde(default)]
    pub gpu: Gpu,
}

#[derive(Deserialize)]
pub struct Mqtt {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
}

// Hand-written so that the password never reaches a log line.
impl fmt::Debug for Mqtt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Mqtt")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("username", &self.username)
            .field("password", &"***")
            .finish()
    }
}

#[derive(Debug, Deserialize)]
pub struct Device {
    pub name: String,
    pub identifier: String,
    pub entity_prefix: String,
}

#[derive(Debug, Deserialize)]
pub struct Gpu {
    pub enabled: bool,
}

// The `[gpu]` section is optional; the GPU is on when it is absent.
impl Default for Gpu {
    fn default() -> Self {
        Gpu { enabled: true }
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read config file {}", path.display()))?;
        Self::parse(&text).with_context(|| format!("invalid config file {}", path.display()))
    }

    pub fn parse(text: &str) -> Result<Self> {
        Ok(toml::from_str(text)?)
    }

    /// The configured unique_id of an entity, or "pc-agent-<device name>-<name>".
    pub fn unique_id(&self, name: &str) -> String {
        match self.unique_ids.get(name) {
            Some(id) => id.clone(),
            None => format!("pc-agent-{}-{}", self.device.name, name),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL: &str = r#"
[mqtt]
host = "broker.example"
port = 1884
username = "user1"
password = "secret-pw"

[device]
name = "JAHURTX-TEST"
identifier = "pc-agent-JAHURTX-TEST"
entity_prefix = "jahurtx_test"

[unique_ids]
sessionstate = "e6996b71-cdff-4e58-8f48-455f5670f921"

[gpu]
enabled = true
"#;

    #[test]
    fn parses_packaged_example_config() {
        let c = Config::parse(include_str!("../packaging/config.example.toml")).unwrap();
        assert_eq!(c.mqtt.port, 1883);
        assert_eq!(c.device.name, "<DEVICE>-TEST");
        assert!(c.unique_ids.is_empty());
        assert!(c.gpu.enabled);
    }

    #[test]
    fn parses_full_file() {
        let c = Config::parse(FULL).unwrap();
        assert_eq!(c.mqtt.host, "broker.example");
        assert_eq!(c.mqtt.port, 1884);
        assert_eq!(c.mqtt.username, "user1");
        assert_eq!(c.mqtt.password, "secret-pw");
        assert_eq!(c.device.name, "JAHURTX-TEST");
        assert_eq!(c.device.identifier, "pc-agent-JAHURTX-TEST");
        assert_eq!(c.device.entity_prefix, "jahurtx_test");
        assert!(c.gpu.enabled);
    }

    #[test]
    fn unique_id_from_table_or_default_rule() {
        let c = Config::parse(FULL).unwrap();
        assert_eq!(
            c.unique_id("sessionstate"),
            "e6996b71-cdff-4e58-8f48-455f5670f921"
        );
        assert_eq!(c.unique_id("lastboot"), "pc-agent-JAHURTX-TEST-lastboot");
    }

    #[test]
    fn unique_ids_section_is_optional() {
        let text = FULL.replace(
            "[unique_ids]\nsessionstate = \"e6996b71-cdff-4e58-8f48-455f5670f921\"\n",
            "",
        );
        let c = Config::parse(&text).unwrap();
        assert_eq!(
            c.unique_id("sessionstate"),
            "pc-agent-JAHURTX-TEST-sessionstate"
        );
    }

    #[test]
    fn gpu_section_is_optional_and_on_by_default() {
        let text = FULL.replace("[gpu]\nenabled = true\n", "");
        let c = Config::parse(&text).unwrap();
        assert!(c.gpu.enabled);
    }

    #[test]
    fn debug_output_hides_password() {
        let c = Config::parse(FULL).unwrap();
        let text = format!("{c:?}");
        assert!(!text.contains("secret-pw"), "{text}");
    }
}
