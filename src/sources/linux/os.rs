use std::path::Path;

/// The Omarchy version file.
pub const VERSION_PATH: &str = "/usr/share/omarchy/version";

/// The device model: the trimmed content of the version file, or `Linux`.
pub fn read_model(path: &Path) -> String {
    match std::fs::read_to_string(path) {
        Ok(text) if !text.trim().is_empty() => text.trim().to_string(),
        _ => "Linux".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn model_is_the_trimmed_content_of_the_version_file() {
        let dir = std::env::temp_dir().join(format!("pc-agent-os-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("version");
        std::fs::write(&file, "3.1.0\n").unwrap();
        assert_eq!(read_model(&file), "3.1.0");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn model_is_linux_without_the_file() {
        assert_eq!(
            read_model(Path::new("/nonexistent/pc-agent/version")),
            "Linux"
        );
    }
}
