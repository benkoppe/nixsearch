use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::error::{ConfigError, Result};
use crate::validation::validate_non_empty;

const MIN_SCHEDULE_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerConfig {
    pub listen: String,
    pub bootstrap: bool,
    pub schedule: ScheduleConfig,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen: "127.0.0.1:3000".to_owned(),
            bootstrap: true,
            schedule: ScheduleConfig::default(),
        }
    }
}

impl ServerConfig {
    pub(crate) fn validate(&self) -> Result<()> {
        validate_non_empty("server.listen", &self.listen)?;
        self.schedule.validate()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ScheduleConfig {
    pub enabled: bool,
    pub interval: String,
}

impl Default for ScheduleConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            interval: "24h".to_owned(),
        }
    }
}

impl ScheduleConfig {
    pub fn parse_interval(&self) -> std::result::Result<Duration, ConfigError> {
        parse_duration(&self.interval).map_err(|message| {
            ConfigError::Validation(format!("server.schedule.interval: {message}"))
        })
    }

    fn validate(&self) -> Result<()> {
        self.parse_interval()?;
        Ok(())
    }
}

pub(crate) fn parse_duration(s: &str) -> std::result::Result<Duration, String> {
    let s = s.trim();

    if s.is_empty() {
        return Err("interval must not be empty".to_owned());
    }

    let split_pos = s
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(s.len());

    let (number, unit) = s.split_at(split_pos);

    if number.is_empty() {
        return Err(format!("invalid number in {s:?}"));
    }

    let value: f64 = number
        .parse()
        .map_err(|_| format!("invalid number in {s:?}"))?;

    if !value.is_finite() || value <= 0.0 {
        return Err("interval must be positive".to_owned());
    }

    let seconds = match unit.trim() {
        "s" | "sec" | "secs" => value,
        "m" | "min" | "mins" => value * 60.0,
        "h" | "hr" | "hrs" | "hour" | "hours" => value * 3600.0,
        "d" | "day" | "days" => value * 86400.0,
        other => return Err(format!("unknown time unit {other:?}; use s, m, h, or d")),
    };

    if !seconds.is_finite() {
        return Err("interval is out of range".to_owned());
    }

    let duration = std::time::Duration::try_from_secs_f64(seconds)
        .map_err(|_| "interval is out of range".to_owned())?;

    if duration < MIN_SCHEDULE_INTERVAL {
        return Err(format!(
            "interval must be at least {}s",
            MIN_SCHEDULE_INTERVAL.as_secs()
        ));
    }

    Ok(duration)
}
