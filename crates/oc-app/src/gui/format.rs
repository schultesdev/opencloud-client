//! Formatting helpers for sizes, durations, and timestamps.

use std::time::Duration;

/// Formats a byte count for humans (`1.2 GB`).
pub(super) fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Formats a remaining time for humans (`45 s`, `2 min 05 s`, `1 h 12 min`).
pub(super) fn format_eta(duration: Duration) -> String {
    let secs = duration.as_secs();
    if secs < 60 {
        format!("{secs} s")
    } else if secs < 3600 {
        format!("{} min {:02} s", secs / 60, secs % 60)
    } else {
        format!("{} h {:02} min", secs / 3600, (secs % 3600) / 60)
    }
}

/// Formats a unix timestamp as `TT.MM.JJJJ SS:MM:SS` (UTC).
pub(super) fn format_unix(unix: i64) -> String {
    let days = unix.div_euclid(86_400);
    let secs = unix.rem_euclid(86_400);
    let (hour, minute, second) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    // Civil-from-days (Howard Hinnant); days since 1970-01-01.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };
    format!("{day:02}.{month:02}.{year} {hour:02}:{minute:02}:{second:02}")
}

/// Describes one side of a conflict (`1.2 KB, 01.02.2026 10:00:00`).
pub(super) fn describe_side(size: Option<i64>, mtime: Option<i64>) -> String {
    let size = size
        .map(|size| format_bytes(size.max(0) as u64))
        .unwrap_or_else(|| "?".into());
    let mtime = mtime.map(format_unix).unwrap_or_else(|| "—".into());
    format!("{size}, {mtime}")
}

/// The first upper-case character of a name (avatar initials).
pub(super) fn initial_of(name: &str) -> String {
    name.trim()
        .chars()
        .next()
        .map(|character| character.to_uppercase().to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_unix_epoch_is_1970() {
        assert_eq!(format_unix(0), "01.01.1970 00:00:00");
    }

    #[test]
    fn format_unix_known_date() {
        // 2026-10-22 12:34:56 UTC.
        assert_eq!(format_unix(1_792_672_496), "22.10.2026 12:34:56");
    }

    #[test]
    fn format_bytes_scales() {
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(2048), "2.0 KB");
    }

    #[test]
    fn format_eta_scales() {
        assert_eq!(format_eta(Duration::from_secs(45)), "45 s");
        assert_eq!(format_eta(Duration::from_secs(125)), "2 min 05 s");
        assert_eq!(format_eta(Duration::from_secs(4320)), "1 h 12 min");
    }

    #[test]
    fn describe_side_handles_unknown() {
        assert_eq!(describe_side(None, None), "?, —");
    }
}
