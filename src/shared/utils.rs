use std::time::Duration;

/// Parse a human-readable duration string (e.g., "30s", "5m", "1h") into a `Duration`.
/// Returns the default if the input is empty or unparseable.
pub fn parse_duration_or_default(input: &str, default: Duration) -> Duration {
    let value = input.trim();
    if value.is_empty() {
        return default;
    }

    let Some(unit) = value.chars().last() else {
        return default;
    };

    let number = &value[..value.len() - 1];
    let Ok(amount) = number.parse::<u64>() else {
        return default;
    };

    match unit {
        's' => Duration::from_secs(amount),
        'm' => Duration::from_secs(amount.saturating_mul(60)),
        'h' => Duration::from_secs(amount.saturating_mul(3600)),
        _ => default,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_seconds() {
        assert_eq!(
            parse_duration_or_default("30s", Duration::ZERO),
            Duration::from_secs(30)
        );
    }

    #[test]
    fn parse_minutes() {
        assert_eq!(
            parse_duration_or_default("5m", Duration::ZERO),
            Duration::from_secs(300)
        );
    }

    #[test]
    fn parse_hours() {
        assert_eq!(
            parse_duration_or_default("2h", Duration::ZERO),
            Duration::from_secs(7200)
        );
    }

    #[test]
    fn parse_empty_returns_default() {
        assert_eq!(
            parse_duration_or_default("", Duration::from_secs(42)),
            Duration::from_secs(42)
        );
    }

    #[test]
    fn parse_invalid_returns_default() {
        assert_eq!(
            parse_duration_or_default("abc", Duration::from_secs(1)),
            Duration::from_secs(1)
        );
    }
}
