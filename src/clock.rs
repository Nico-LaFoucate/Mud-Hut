// SPDX-License-Identifier: Apache-2.0
//! Wall-clock times as RFC 3339 UTC strings ("2026-10-16T19:00:00Z"), without a date crate.

use std::time::{SystemTime, UNIX_EPOCH};

pub fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Days since 1970-01-01 -> (year, month, day). Howard Hinnant's civil_from_days.
fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

pub fn rfc3339(secs: u64) -> String {
    let (y, m, d) = civil((secs / 86_400) as i64);
    let t = secs % 86_400;
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", t / 3600, (t % 3600) / 60, t % 60)
}

pub fn now() -> String {
    rfc3339(now_secs())
}

/// A compact id-safe stamp: "20261016T190012Z".
pub fn stamp(secs: u64) -> String {
    rfc3339(secs).replace(['-', ':'], "")
}

/// "6h", "30m", "1d", "45s", "0", or plain seconds.
pub fn parse_duration(s: &str) -> Result<std::time::Duration, String> {
    let s = s.trim();
    let (num, mult) = match s.chars().last() {
        Some('s') => (&s[..s.len() - 1], 1),
        Some('m') => (&s[..s.len() - 1], 60),
        Some('h') => (&s[..s.len() - 1], 3600),
        Some('d') => (&s[..s.len() - 1], 86_400),
        _ => (s, 1),
    };
    num.trim()
        .parse::<u64>()
        .map(|n| std::time::Duration::from_secs(n * mult))
        .map_err(|_| format!("not a duration: {s:?} (use e.g. 6h, 30m, 1d, 0)"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_utc() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(1_792_171_411), "2026-10-16T17:23:31Z");
        assert_eq!(stamp(1_792_171_411), "20261016T172331Z");
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("6h").unwrap().as_secs(), 21_600);
        assert_eq!(parse_duration("0").unwrap().as_secs(), 0);
        assert_eq!(parse_duration("1d").unwrap().as_secs(), 86_400);
        assert!(parse_duration("soon").is_err());
    }
}
