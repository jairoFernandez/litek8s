//! Kubernetes resource quantities (`500m`, `1.5`, `128Mi`, `2G`, `1e3`).

/// Parse a quantity into its value in base units, scaled by 1000 so CPU
/// millicores survive (`"500m"` → 500, `"2"` → 2000, `"1Ki"` → 1_024_000).
pub fn parse_milli(raw: &str) -> Option<i128> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let split = raw
        .find(|c: char| !(c.is_ascii_digit() || c == '.' || c == '+' || c == '-'))
        .unwrap_or(raw.len());
    let (number, suffix) = raw.split_at(split);
    let value: f64 = number.parse().ok()?;
    let factor: f64 = match suffix {
        "" => 1.0,
        "m" => 1e-3,
        "u" => 1e-6,
        "n" => 1e-9,
        "k" => 1e3,
        "M" => 1e6,
        "G" => 1e9,
        "T" => 1e12,
        "P" => 1e15,
        "E" => 1e18,
        "Ki" => 1024.0,
        "Mi" => 1024f64.powi(2),
        "Gi" => 1024f64.powi(3),
        "Ti" => 1024f64.powi(4),
        "Pi" => 1024f64.powi(5),
        "Ei" => 1024f64.powi(6),
        s if s.starts_with('e') || s.starts_with('E') => 10f64.powi(s[1..].parse().ok()?),
        _ => return None,
    };
    Some((value * factor * 1000.0).round() as i128)
}

/// A quantity as an integer number of base units (bytes, cores rounded up).
pub fn parse_units(raw: &str) -> Option<i128> {
    parse_milli(raw).map(|m| (m + 999).div_euclid(1000))
}

/// A quantity from a JSON value, which may be a string or a number.
pub fn milli_of(value: &serde_json::Value) -> Option<i128> {
    match value {
        serde_json::Value::String(s) => parse_milli(s),
        serde_json::Value::Number(n) => n.as_f64().map(|f| (f * 1000.0).round() as i128),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_common_forms() {
        assert_eq!(parse_milli("500m"), Some(500));
        assert_eq!(parse_milli("2"), Some(2000));
        assert_eq!(parse_milli("1.5"), Some(1500));
        assert_eq!(parse_units("128Mi"), Some(128 * 1024 * 1024));
        assert_eq!(parse_units("1G"), Some(1_000_000_000));
        assert_eq!(parse_units("1e3"), Some(1000));
        assert_eq!(parse_milli("x"), None);
    }
}
