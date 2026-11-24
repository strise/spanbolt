//! The `version_retention_period` database option: validation, and the period it sets.

use std::time::Duration;

use crate::DdlError;
use crate::schema::Schema;

pub const VERSION_RETENTION_PERIOD: &str = "version_retention_period";
/// Spanner's retention when the option isn't set.
pub const DEFAULT_VERSION_RETENTION: Duration = Duration::from_secs(3600);
const MIN: Duration = Duration::from_secs(3600);
const MAX: Duration = Duration::from_secs(7 * 24 * 3600);

/// `<n>s`, `<n>m`, `<n>h` or `<n>d`, the forms Spanner and the emulator take.
fn parse_period(text: &str) -> Option<Duration> {
    let unit = text.chars().last()?;
    let digits = &text[..text.len() - unit.len_utf8()];
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let n: u64 = digits.parse().ok()?;
    let secs = match unit {
        's' => 1,
        'm' => 60,
        'h' => 3600,
        'd' => 86_400,
        _ => return None,
    };
    n.checked_mul(secs).map(Duration::from_secs)
}

/// The text of a quoted option value as the parser stores it (`'2h'`), or `None` for anything
/// else (a number, `NULL`).
fn unquote(value: &str) -> Option<&str> {
    value.strip_prefix('\'')?.strip_suffix('\'')
}

/// Checks a `version_retention_period` value in `ALTER DATABASE ... SET OPTIONS`. `NULL` resets
/// it. Production takes `[1h, 7d]`; the emulator (`emulator`) takes anything up to 7 days.
pub(crate) fn check_option(stmt: &str, value: &str, emulator: bool) -> Result<(), DdlError> {
    if value == "NULL" {
        return Ok(());
    }
    let Some(text) = unquote(value).filter(|t| !t.is_empty()) else {
        let expected = if emulator {
            "Supported values are non-empty strings only."
        } else {
            "Supported option value is string."
        };
        return Err(DdlError::Syntax(format!(
            "Error parsing Spanner DDL statement: {stmt} : Unexpected value for option: \
             {VERSION_RETENTION_PERIOD}. {expected}"
        )));
    };
    let period = parse_period(text);
    if emulator {
        return match period {
            Some(p) if p.is_zero() => Err(invalid_emulator(text)),
            Some(p) if p > MAX => Err(DdlError::Syntax(format!(
                "Retention period must be less than 7 days: {text}"
            ))),
            Some(_) => Ok(()),
            None => Err(invalid_emulator(text)),
        };
    }
    match period {
        Some(p) if (MIN..=MAX).contains(&p) => Ok(()),
        Some(_) => Err(DdlError::Syntax(
            "Option must be a value in the range [`1h`, `7d`].".into(),
        )),
        None => Err(DdlError::Syntax(format!(
            "Error parsing Spanner DDL statement: {stmt} : Cannot parse '{text}' as a valid \
             timestamp"
        ))),
    }
}

fn invalid_emulator(text: &str) -> DdlError {
    DdlError::Syntax(format!("Invalid retention period: {text}"))
}

impl Schema {
    /// The `version_retention_period` option as written (`'2h'` gives `2h`), if set.
    pub fn version_retention_option(&self) -> Option<&str> {
        self.options
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(VERSION_RETENTION_PERIOD))
            .and_then(|(_, v)| unquote(v))
    }

    /// How long old versions stay readable: the option, else Spanner's default of one hour.
    pub fn version_retention(&self) -> Duration {
        self.version_retention_option()
            .and_then(parse_period)
            .unwrap_or(DEFAULT_VERSION_RETENTION)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn periods() {
        assert_eq!(parse_period("3600s"), Some(Duration::from_secs(3600)));
        assert_eq!(parse_period("7d"), Some(MAX));
        assert_eq!(parse_period("59m"), Some(Duration::from_secs(59 * 60)));
        for bad in ["", "h", "1H", "1d2h", "abc", "1.5h", "-1h", "1w"] {
            assert_eq!(parse_period(bad), None, "{bad}");
        }
    }

    #[test]
    fn production_and_emulator_ranges() {
        let s = "stmt";
        assert!(check_option(s, "'1h'", false).is_ok());
        assert!(check_option(s, "'59m'", false).is_err());
        assert!(check_option(s, "'59m'", true).is_ok());
        assert!(check_option(s, "'8d'", true).is_err());
        assert!(check_option(s, "'0d'", true).is_err());
        assert!(check_option(s, "NULL", false).is_ok());
        assert!(check_option(s, "7", false).is_err());
    }
}
