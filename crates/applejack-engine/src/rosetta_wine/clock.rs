//! UTC timestamps without a calendar crate: snapshot directory names and
//! the `D3DMetal` acceptance time.

use std::time::{SystemTime, UNIX_EPOCH};

/// A broken-down UTC instant with one-second resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Utc {
    year: u64,
    month: u64,
    day: u64,
    hour: u64,
    minute: u64,
    second: u64,
}

impl Utc {
    pub(crate) fn now() -> Self {
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_secs());
        Self::from_unix(secs)
    }

    /// Civil date from days since the Unix epoch (Howard Hinnant's
    /// `civil_from_days`, valid for every date after 1970).
    pub(crate) fn from_unix(secs: u64) -> Self {
        let days = secs / 86_400;
        let rem = secs % 86_400;
        let z = days + 719_468;
        let era = z / 146_097;
        let doe = z - era * 146_097;
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let day = doy - (153 * mp + 2) / 5 + 1;
        let month = if mp < 10 { mp + 3 } else { mp - 9 };
        let year = yoe + era * 400 + u64::from(month <= 2);
        Self {
            year,
            month,
            day,
            hour: rem / 3600,
            minute: rem % 3600 / 60,
            second: rem % 60,
        }
    }

    /// `20261030T171500Z`: sorts chronologically and is a directory name.
    pub(crate) fn compact(self) -> String {
        format!(
            "{:04}{:02}{:02}T{:02}{:02}{:02}Z",
            self.year, self.month, self.day, self.hour, self.minute, self.second
        )
    }

    /// `2026-10-30T17:15:00Z`.
    pub(crate) fn rfc3339(self) -> String {
        format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
            self.year, self.month, self.day, self.hour, self.minute, self.second
        )
    }
}

#[cfg(test)]
mod tests {
    use super::Utc;

    #[test]
    fn civil_conversion_matches_known_instants() {
        assert_eq!(Utc::from_unix(0).rfc3339(), "1970-01-01T00:00:00Z");
        assert_eq!(
            Utc::from_unix(1_000_000_000).rfc3339(),
            "2001-09-09T01:46:40Z"
        );
        assert_eq!(Utc::from_unix(951_782_400).compact(), "20000229T000000Z");
        assert_eq!(Utc::from_unix(1_793_378_700).compact(), "20261030T164500Z");
    }
}
