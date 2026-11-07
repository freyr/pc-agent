use chrono::{DateTime, Utc};

/// Formats seconds since the epoch as `%Y-%m-%d %H:%M:%SZ` in UTC, the
/// timestamp format that HASS.Agent publishes.
pub fn format_timestamp(epoch_secs: i64) -> String {
    DateTime::<Utc>::from_timestamp(epoch_secs, 0)
        .unwrap_or_default()
        .format("%Y-%m-%d %H:%M:%SZ")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_known_epoch() {
        // 2026-08-28T04:54:55Z, computed outside this code.
        assert_eq!(format_timestamp(1_787_892_895), "2026-08-28 04:54:55Z");
    }

    #[test]
    fn pads_single_digit_fields() {
        // 1970-01-02T03:04:05Z
        assert_eq!(format_timestamp(97_445), "1970-01-02 03:04:05Z");
    }
}
