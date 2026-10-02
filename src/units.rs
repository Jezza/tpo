//! Link property values: durations, percentages and rates, parsed from the strings used in
//! topology files and on the command line, and rendered in the form `tc` expects.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::time::Duration;

/// Splits `"20ms"` into `(20.0, "ms")`.
fn split_number(input: &str) -> Result<(f64, &str), String> {
    let s = input.trim();
    let end = s
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(s.len());
    let (num, unit) = s.split_at(end);
    if num.is_empty() {
        return Err(format!("`{input}` is not a number followed by a unit"));
    }
    let value: f64 = num
        .parse()
        .map_err(|_| format!("`{input}` is not a valid number"))?;
    Ok((value, unit.trim()))
}

/// Parses `us`, `ms`, `s` or `m` durations; a bare `0` needs no unit.
pub fn parse_duration(input: &str) -> Result<Duration, String> {
    let (value, unit) = split_number(input)?;
    let secs = match unit {
        "" if value == 0.0 => 0.0,
        "" => {
            return Err(format!(
                "`{input}` needs a unit (us, ms, s or m), e.g. `{input}ms`"
            ));
        }
        "us" => value / 1_000_000.0,
        "ms" => value / 1_000.0,
        "s" => value,
        "m" => value * 60.0,
        _ => {
            return Err(format!(
                "`{input}` has an unknown unit (use us, ms, s or m)"
            ));
        }
    };
    Ok(Duration::from_secs_f64(secs))
}

/// A percentage in `0..=100`.
#[derive(Clone, Copy, Debug, PartialEq, PartialOrd, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Percent(pub f64);

pub fn parse_percent(input: &str) -> Result<Percent, String> {
    let (value, unit) = split_number(input)?;
    match unit {
        "%" => {}
        "" if value == 0.0 => {}
        _ => return Err(format!("`{input}` must be a percentage, e.g. `2%`")),
    }
    if value > 100.0 {
        return Err(format!("`{input}` is more than 100%"));
    }
    Ok(Percent(value))
}

impl Percent {
    pub fn is_zero(self) -> bool {
        self.0 == 0.0
    }

    pub fn to_tc(self) -> String {
        format!("{}%", self.0)
    }
}

impl fmt::Display for Percent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}%", self.0)
    }
}

/// A rate in bits per second.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Rate(pub u64);

pub fn parse_rate(input: &str) -> Result<Rate, String> {
    let (value, unit) = split_number(input)?;
    let multiplier = match unit {
        "" if value == 0.0 => 0.0,
        "bit" => 1.0,
        "kbit" => 1e3,
        "mbit" => 1e6,
        "gbit" => 1e9,
        "bps" => 8.0,
        "kbps" => 8e3,
        "mbps" => 8e6,
        "gbps" => 8e9,
        _ => {
            return Err(format!(
                "`{input}` has an unknown rate unit (bit, kbit, mbit, gbit, bps, kbps, mbps, gbps)"
            ));
        }
    };
    Ok(Rate((value * multiplier).round() as u64))
}

impl Rate {
    pub fn to_tc(self) -> String {
        format!("{}bit", self.0)
    }
}

impl fmt::Display for Rate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (unit, scale) in [
            ("gbit", 1_000_000_000),
            ("mbit", 1_000_000),
            ("kbit", 1_000),
        ] {
            if self.0 >= scale && self.0.is_multiple_of(scale) {
                return write!(f, "{}{unit}", self.0 / scale);
            }
        }
        write!(f, "{}bit", self.0)
    }
}

/// Microseconds are exact for every value we accept, and every `tc` understands them.
pub fn tc_duration(d: Duration) -> String {
    format!("{}us", d.as_micros())
}

pub fn human_duration(d: Duration) -> String {
    let us = d.as_micros();
    if us == 0 {
        "0".to_owned()
    } else if us.is_multiple_of(1_000_000) {
        format!("{}s", us / 1_000_000)
    } else if us.is_multiple_of(1_000) {
        format!("{}ms", us / 1_000)
    } else {
        format!("{us}us")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn parses_durations_in_tc_units() {
        assert_eq!(parse_duration("20ms").unwrap(), Duration::from_millis(20));
        assert_eq!(parse_duration("1s").unwrap(), Duration::from_secs(1));
        assert_eq!(parse_duration("250us").unwrap(), Duration::from_micros(250));
        assert_eq!(
            parse_duration("1.5ms").unwrap(),
            Duration::from_micros(1500)
        );
        assert_eq!(parse_duration("0").unwrap(), Duration::ZERO);
        assert_eq!(parse_duration("2m").unwrap(), Duration::from_secs(120));
    }

    #[test]
    fn rejects_durations_without_a_unit() {
        let err = parse_duration("20").unwrap_err();
        assert!(err.contains("unit"), "{err}");
        assert!(parse_duration("ms").is_err());
        assert!(parse_duration("-5ms").is_err());
        assert!(parse_duration("5 parsecs").is_err());
    }

    #[test]
    fn parses_percentages() {
        assert_eq!(parse_percent("2%").unwrap(), Percent(2.0));
        assert_eq!(parse_percent("0.1%").unwrap(), Percent(0.1));
        assert_eq!(parse_percent("0").unwrap(), Percent(0.0));
        assert!(parse_percent("2").is_err());
        assert!(parse_percent("101%").is_err());
        assert!(parse_percent("-1%").is_err());
    }

    #[test]
    fn parses_rates_into_bits_per_second() {
        assert_eq!(parse_rate("1mbit").unwrap(), Rate(1_000_000));
        assert_eq!(parse_rate("100kbit").unwrap(), Rate(100_000));
        assert_eq!(parse_rate("2gbit").unwrap(), Rate(2_000_000_000));
        assert_eq!(parse_rate("500bit").unwrap(), Rate(500));
        assert_eq!(parse_rate("1mbps").unwrap(), Rate(8_000_000));
        assert_eq!(parse_rate("0").unwrap(), Rate(0));
        assert!(parse_rate("1mb").is_err());
        assert!(parse_rate("fast").is_err());
    }

    #[test]
    fn renders_values_for_tc() {
        assert_eq!(tc_duration(Duration::from_millis(20)), "20000us");
        assert_eq!(Percent(2.5).to_tc(), "2.5%");
        assert_eq!(Rate(1_000_000).to_tc(), "1000000bit");
    }

    #[test]
    fn renders_values_for_humans() {
        assert_eq!(human_duration(Duration::from_millis(20)), "20ms");
        assert_eq!(human_duration(Duration::from_micros(1500)), "1500us");
        assert_eq!(human_duration(Duration::from_secs(2)), "2s");
        assert_eq!(human_duration(Duration::ZERO), "0");
        assert_eq!(Percent(2.0).to_string(), "2%");
        assert_eq!(Rate(1_000_000).to_string(), "1mbit");
        assert_eq!(Rate(1_500_000).to_string(), "1500kbit");
    }
}
