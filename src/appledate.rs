//! Conversion of Apple Messages timestamps to UTC datetimes.
//!
//! Apple Messages stores dates as offsets from the Core Data epoch
//! (2001-01-01T00:00:00Z). Modern macOS versions store nanoseconds; releases
//! before High Sierra stored whole seconds. Zero and negative values mean
//! "no timestamp" (e.g. `date_edited` on a message that was never edited).

use chrono::{DateTime, Utc};

/// Seconds between the Unix epoch (1970-01-01) and Apple's epoch (2001-01-01).
pub const APPLE_EPOCH_OFFSET_SECS: i64 = 978_307_200;

/// Raw values above this are interpreted as nanoseconds. Expressed as seconds
/// this would be roughly the year 5170, so no plausible seconds value ever
/// crosses it, while any nanoseconds value after 2001-01-01 00:01:40 does.
const NANOSECOND_THRESHOLD: i64 = 100_000_000_000;

/// Convert a raw Apple Messages timestamp to UTC.
///
/// Returns `None` for zero, negative, or out-of-range values.
pub fn apple_time_to_utc(raw: i64) -> Option<DateTime<Utc>> {
    if raw <= 0 {
        return None;
    }
    let (apple_secs, nanos) = if raw > NANOSECOND_THRESHOLD {
        (raw / 1_000_000_000, (raw % 1_000_000_000) as u32)
    } else {
        (raw, 0)
    };
    DateTime::from_timestamp(apple_secs + APPLE_EPOCH_OFFSET_SECS, nanos)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2023-01-01T00:00:00Z in seconds since the Apple epoch.
    const APPLE_SECS_2023: i64 = 694_224_000;

    #[test]
    fn zero_means_no_timestamp() {
        assert_eq!(apple_time_to_utc(0), None);
    }

    #[test]
    fn negative_means_no_timestamp() {
        assert_eq!(apple_time_to_utc(-5), None);
        assert_eq!(apple_time_to_utc(i64::MIN), None);
    }

    #[test]
    fn nanosecond_timestamps_convert() {
        let raw = APPLE_SECS_2023 * 1_000_000_000;
        let dt = apple_time_to_utc(raw).unwrap();
        assert_eq!(dt.to_rfc3339(), "2023-01-01T00:00:00+00:00");
    }

    #[test]
    fn legacy_second_timestamps_convert() {
        let dt = apple_time_to_utc(APPLE_SECS_2023).unwrap();
        assert_eq!(dt.to_rfc3339(), "2023-01-01T00:00:00+00:00");
    }

    #[test]
    fn subsecond_precision_is_preserved() {
        let raw = APPLE_SECS_2023 * 1_000_000_000 + 123_456_789;
        let dt = apple_time_to_utc(raw).unwrap();
        assert_eq!(dt.timestamp_subsec_nanos(), 123_456_789);
    }

    #[test]
    fn small_positive_values_are_treated_as_seconds() {
        // One second after the Apple epoch.
        let dt = apple_time_to_utc(1).unwrap();
        assert_eq!(dt.to_rfc3339(), "2001-01-01T00:00:01+00:00");
    }

    #[test]
    fn absurdly_large_values_do_not_panic() {
        // i64::MAX nanoseconds is ~2293 CE; must convert, not panic.
        assert!(apple_time_to_utc(i64::MAX).is_some());
    }
}
