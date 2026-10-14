//! Date/time helpers and CF time units handling.

use anyhow::{bail, Context, Result};
use chrono::{DateTime, NaiveDate, NaiveDateTime, TimeZone, Utc};

/// Parse a user or metadata datetime. Accepts "now", dates, and datetimes
/// with or without time zone (UTC is assumed when missing).
pub fn parse_datetime(input: &str) -> Result<DateTime<Utc>> {
    let input = input.trim();
    if input.eq_ignore_ascii_case("now") {
        return Ok(Utc::now());
    }
    if let Ok(dt) = DateTime::parse_from_rfc3339(input) {
        return Ok(dt.with_timezone(&Utc));
    }
    let without_z = input.trim_end_matches('Z');
    for format in [
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%dT%H:%M",
        "%Y-%m-%d %H:%M",
    ] {
        if let Ok(naive) = NaiveDateTime::parse_from_str(without_z, format) {
            return Ok(Utc.from_utc_datetime(&naive));
        }
    }
    for format in ["%Y-%m-%d %H:%M:%S%.f%:z", "%Y-%m-%d %H:%M:%S%z"] {
        if let Ok(dt) = DateTime::parse_from_str(input, format) {
            return Ok(dt.with_timezone(&Utc));
        }
    }
    for format in ["%Y-%m-%d", "%Y%m%d"] {
        if let Ok(date) = NaiveDate::parse_from_str(input, format) {
            return Ok(Utc.from_utc_datetime(&date.and_hms_opt(0, 0, 0).unwrap()));
        }
    }
    if let Ok(year) = input.parse::<i32>() {
        if (1000..=9999).contains(&year) {
            return Ok(Utc.with_ymd_and_hms(year, 1, 1, 0, 0, 0).unwrap());
        }
    }
    bail!("Wrong datetime format: '{input}'")
}

/// CF convention time units: "<unit> since <reference datetime>".
#[derive(Debug, Clone)]
pub struct CfTimeUnits {
    pub milliseconds_per_unit: f64,
    pub reference: DateTime<Utc>,
}

impl CfTimeUnits {
    pub fn parse(units: &str) -> Result<Self> {
        let (unit, reference) = units
            .split_once(" since ")
            .with_context(|| format!("Unsupported time units: '{units}'"))?;
        let milliseconds_per_unit = match unit.trim().to_lowercase().as_str() {
            "milliseconds" | "millisecond" | "ms" => 1.0,
            "seconds" | "second" | "secs" | "sec" | "s" => 1_000.0,
            "minutes" | "minute" | "mins" | "min" => 60_000.0,
            "hours" | "hour" | "hrs" | "hr" | "h" => 3_600_000.0,
            "days" | "day" | "d" => 86_400_000.0,
            other => bail!("Unsupported time unit '{other}'"),
        };
        // Some datasets have suffixes such as "(no leap seconds)"
        let reference = reference.split(" (").next().unwrap_or(reference).trim();
        let reference = parse_datetime(reference)
            .or_else(|_| parse_datetime(reference.replace(" 00:00:00Z", "").trim()))?;
        Ok(Self {
            milliseconds_per_unit,
            reference,
        })
    }

    pub fn to_value(&self, datetime: &DateTime<Utc>) -> f64 {
        let delta_ms = (*datetime - self.reference).num_milliseconds() as f64;
        delta_ms / self.milliseconds_per_unit
    }

    pub fn to_datetime(&self, value: f64) -> DateTime<Utc> {
        let delta_ms = (value * self.milliseconds_per_unit).round() as i64;
        self.reference + chrono::Duration::milliseconds(delta_ms)
    }
}

pub fn size_to_mb(bytes: u64) -> f64 {
    (bytes as f64 / 1_048_576.0 * 100.0).round() / 100.0
}

/// Return a path that does not exist yet by adding "_(n)" before the extension,
/// mimicking the Python toolbox behaviour.
pub fn unique_path(path: &std::path::Path) -> std::path::PathBuf {
    if !path.exists() {
        return path.to_path_buf();
    }
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("file");
    let extension = path.extension().and_then(|s| s.to_str());
    let parent = path.parent().unwrap_or_else(|| std::path::Path::new("."));
    for i in 1.. {
        let name = match extension {
            Some(ext) => format!("{stem}_({i}).{ext}"),
            None => format!("{stem}_({i})"),
        };
        let candidate = parent.join(name);
        if !candidate.exists() {
            return candidate;
        }
    }
    unreachable!()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cf_units() {
        let units = CfTimeUnits::parse("hours since 1950-01-01").unwrap();
        let dt = parse_datetime("2024-01-01").unwrap();
        assert_eq!(units.to_value(&dt), 648_672.0);
        assert_eq!(units.to_datetime(648_672.0), dt);
        let units = CfTimeUnits::parse("milliseconds since 1970-01-01 00:00:00Z (no leap seconds)")
            .unwrap();
        assert_eq!(
            units.to_value(&parse_datetime("1970-01-02").unwrap()),
            86_400_000.0
        );
    }

    #[test]
    fn parses_datetimes() {
        for input in [
            "2024-01-01",
            "2024-01-01T00:00:00",
            "2024-01-01 00:00:00",
            "2024-01-01T00:00:00Z",
        ] {
            assert_eq!(parse_datetime(input).unwrap().timestamp(), 1_704_067_200);
        }
    }
}
