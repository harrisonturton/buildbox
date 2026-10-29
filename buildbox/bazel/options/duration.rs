//! Durations, converted as Bazel's `DurationConverter` does: a whole number
//! and one unit, such as `30s` or `5m`.

use std::time::Duration;

/// The duration `text` gives, or Bazel's message saying why it isn't one.
pub fn convert_duration(text: &str) -> Result<Duration, String> {
    if text == "0" {
        return Ok(Duration::ZERO);
    }
    let illegal = || format!("Illegal duration '{text}'.");
    let digits = text.find(|c: char| !c.is_ascii_digit()).ok_or_else(illegal)?;
    let (number, unit) = text.split_at(digits);
    let number: u64 = number.parse().map_err(|_| illegal())?;
    let duration = match unit {
        "d" => Duration::from_secs(number.checked_mul(86_400).ok_or_else(illegal)?),
        "h" => Duration::from_secs(number.checked_mul(3_600).ok_or_else(illegal)?),
        "m" => Duration::from_secs(number.checked_mul(60).ok_or_else(illegal)?),
        "s" => Duration::from_secs(number),
        "ms" => Duration::from_millis(number),
        "ns" => Duration::from_nanos(number),
        _ => return Err(illegal()),
    };
    Ok(duration)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_one_number_and_unit() {
        assert_eq!(convert_duration("0"), Ok(Duration::ZERO));
        assert_eq!(convert_duration("0s"), Ok(Duration::ZERO));
        assert_eq!(convert_duration("90s"), Ok(Duration::from_secs(90)));
        assert_eq!(convert_duration("5m"), Ok(Duration::from_secs(300)));
        assert_eq!(convert_duration("2h"), Ok(Duration::from_secs(7200)));
        assert_eq!(convert_duration("1d"), Ok(Duration::from_secs(86_400)));
        assert_eq!(convert_duration("250ms"), Ok(Duration::from_millis(250)));
    }

    #[test]
    fn rejects_anything_else_as_bazel_does() {
        for text in ["", "s", "10", "1m30s", "-1s", "1.5s", "10 s", "10S"] {
            assert_eq!(convert_duration(text), Err(format!("Illegal duration '{text}'.")), "{text}");
        }
    }
}
