//! The DRM connectors in sysfs: the display fallback without Hyprland.

use std::path::Path;

use super::rules::Connector;

/// The DRM class directory.
pub const DRM_DIR: &str = "/sys/class/drm";

/// The connectors (`card*-*`) that have readable `status` and `dpms` files.
pub fn connectors(drm_dir: &Path) -> Vec<Connector> {
    let Ok(entries) = std::fs::read_dir(drm_dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|e| {
            let name = e.file_name();
            let name = name.to_string_lossy();
            name.starts_with("card") && name.contains('-')
        })
        .filter_map(|e| {
            let read = |file: &str| {
                std::fs::read_to_string(e.path().join(file))
                    .ok()
                    .map(|s| s.trim().to_string())
            };
            Some(Connector {
                name: e.file_name().to_string_lossy().to_string(),
                status: read("status")?,
                dpms: read("dpms")?,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sources::linux::rules::display_from_drm;

    fn connector(dir: &Path, name: &str, status: &str, dpms: &str) {
        let path = dir.join(name);
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join("status"), format!("{status}\n")).unwrap();
        std::fs::write(path.join("dpms"), format!("{dpms}\n")).unwrap();
    }

    #[test]
    fn reads_the_connectors_of_the_drm_directory() {
        let dir = std::env::temp_dir().join(format!("pc-agent-drm-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        connector(&dir, "card1-DP-1", "connected", "Off");
        connector(&dir, "card1-HDMI-A-1", "disconnected", "On");
        // The card itself is no connector.
        connector(&dir, "card1", "connected", "On");

        let mut list = connectors(&dir);
        list.sort_by(|a, b| a.status.cmp(&b.status));
        assert_eq!(
            list,
            [
                Connector {
                    name: "card1-DP-1".into(),
                    status: "connected".into(),
                    dpms: "Off".into()
                },
                Connector {
                    name: "card1-HDMI-A-1".into(),
                    status: "disconnected".into(),
                    dpms: "On".into()
                },
            ]
        );
        assert_eq!(display_from_drm(&list), Some(false));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn no_drm_directory_gives_no_connector() {
        assert!(connectors(Path::new("/nonexistent/pc-agent/drm")).is_empty());
    }
}
