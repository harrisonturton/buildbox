//! Formatting values for people to read.

use std::time::Duration;

const BYTE_UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];

/// Format a size, e.g. "4.2 GiB".
pub fn bytes(bytes: u64) -> String {
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < BYTE_UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }

    if unit == 0 {
        return format!("{bytes} B");
    }
    format!("{value:.1} {}", BYTE_UNITS[unit])
}

/// Format a duration coarsely, e.g. "850ms", "4.2s", "3m 5s" or "2h 10m".
pub fn duration(duration: Duration) -> String {
    let secs = duration.as_secs();
    if secs < 1 {
        return format!("{}ms", duration.as_millis());
    }
    if secs < 60 {
        return format!("{:.1}s", duration.as_secs_f64());
    }
    if secs < 60 * 60 {
        return format!("{}m {}s", secs / 60, secs % 60);
    }
    format!("{}h {}m", secs / 3600, secs / 60 % 60)
}

/// Shorten `text` to at most `max` characters, marking where it was cut.
pub fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let kept: String = text.chars().take(max.saturating_sub(1)).collect();
    format!("{kept}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_bytes() {
        assert_eq!(bytes(0), "0 B");
        assert_eq!(bytes(1023), "1023 B");
        assert_eq!(bytes(1536), "1.5 KiB");
        assert_eq!(bytes(5 * 1024 * 1024 * 1024), "5.0 GiB");
    }

    #[test]
    fn formats_durations() {
        assert_eq!(duration(Duration::from_millis(850)), "850ms");
        assert_eq!(duration(Duration::from_millis(4250)), "4.2s");
        assert_eq!(duration(Duration::from_secs(185)), "3m 5s");
        assert_eq!(duration(Duration::from_secs(2 * 3600 + 10 * 60 + 5)), "2h 10m");
    }

    #[test]
    fn truncates_long_text() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("exactly10!", 10), "exactly10!");
        assert_eq!(truncate("this is too long", 10), "this is t…");
    }
}
