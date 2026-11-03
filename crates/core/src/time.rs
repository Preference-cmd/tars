//! Human-readable time formatting and ID timestamp extraction.

use uuid::Uuid;

/// Format Unix milliseconds as human-readable UTC timestamp `YYYY-MM-DD HH:MM:SS UTC`.
pub fn format_timestamp_utc(millis: u64) -> String {
    let total_secs = millis / 1000;
    let seconds_in_day = (total_secs % 86400) as u32;
    let days = (total_secs / 86400) as i64;
    let hour = seconds_in_day / 3600;
    let minute = (seconds_in_day % 3600) / 60;
    let second = seconds_in_day % 60;

    // Civil date from days since Unix epoch (Howard Hinnant's algorithm).
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = (yoe as i64) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };

    format!("{y:04}-{m:02}-{d:02} {hour:02}:{minute:02}:{second:02} UTC")
}

/// Derive `created_at_ms` from the session ID if it is a UUIDv7.
///
/// Returns `None` if the ID is not a valid UUIDv7.
pub fn created_at_from_id(id: &str) -> Option<u64> {
    let uuid = Uuid::parse_str(id).ok()?;
    if uuid.get_version() == Some(uuid::Version::SortRand) {
        let bytes = uuid.as_bytes();
        let millis = (u64::from(bytes[0]) << 40)
            | (u64::from(bytes[1]) << 32)
            | (u64::from(bytes[2]) << 24)
            | (u64::from(bytes[3]) << 16)
            | (u64::from(bytes[4]) << 8)
            | u64::from(bytes[5]);
        Some(millis)
    } else {
        None
    }
}

/// Current time in milliseconds since Unix epoch.
pub(crate) fn current_time_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_timestamp_utc_known_dates() {
        // Unix epoch: 1970-01-01 00:00:00 UTC
        assert_eq!(format_timestamp_utc(0), "1970-01-01 00:00:00 UTC");
        // 1700000000_000 ms: 2023-11-14 22:13:20 UTC
        assert_eq!(
            format_timestamp_utc(1_700_000_000_000),
            "2023-11-14 22:13:20 UTC"
        );
        // Leap year: 2000-02-29 12:00:00 UTC (951825600 seconds)
        assert_eq!(
            format_timestamp_utc(951_825_600_000),
            "2000-02-29 12:00:00 UTC"
        );
        // Modern date: 2026-11-03 12:30:45 UTC
        // 2026-11-03 is 20760 days since epoch. 20760 * 86400 = 1793664000.
        // 12*3600 + 30*60 + 45 = 45045. Total secs: 1793709045.
        assert_eq!(
            format_timestamp_utc(1_793_709_045_000),
            "2026-11-03 12:30:45 UTC"
        );
    }

    #[test]
    fn created_at_from_id_validates_v7() {
        let v7 = Uuid::now_v7();
        let ms = created_at_from_id(&v7.to_string());
        assert!(ms.is_some());
        let extracted = ms.unwrap();
        // Extracted time should be very close to now
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        assert!(now >= extracted && now - extracted < 5000);

        // UUIDv4 (random) returns None
        let v4 = Uuid::new_v4();
        assert_eq!(created_at_from_id(&v4.to_string()), None);

        // Invalid string returns None
        assert_eq!(created_at_from_id("not-a-uuid"), None);
    }
}
