//! Hand-rolled RFC3339 (T3.1a): fixed-width UTC `YYYY-MM-DDTHH:MM:SSZ`
//! format/parse using the Howard Hinnant civil-date algorithms (no chrono
//! dependency, per repo convention). Lease renew-time math (Q18) and object
//! `creationTimestamp` values flow through here.
//!
//! T3.1b: this module moved from `controllers` to `common` so the apiserver
//! (finalizer-gated DELETE `deletionTimestamp` values) and the controllers
//! (namespace lifecycle) share one implementation; `common` stays pure std.

/// UTC `"YYYY-MM-DDTHH:MM:SSZ"` from unix seconds (no chrono; Hinnant
/// `civil_from_days`).
pub fn now_rfc3339(unix_secs: u64) -> String {
    let days = (unix_secs / 86_400) as i64;
    let rem = unix_secs % 86_400;
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z",
        h = rem / 3600,
        mi = (rem / 60) % 60,
        s = rem % 60
    )
}

/// Parse the exact fixed-width format emitted by [`now_rfc3339`]. Anything
/// else (including offset forms) is rejected.
pub fn parse_rfc3339(s: &str) -> Option<u64> {
    let b = s.as_bytes();
    let sep = |i: usize, c: u8| b.get(i) == Some(&c);
    if b.len() != 20
        || !sep(4, b'-')
        || !sep(7, b'-')
        || !sep(10, b'T')
        || !sep(13, b':')
        || !sep(16, b':')
        || !sep(19, b'Z')
    {
        return None;
    }
    let num = |r: std::ops::Range<usize>| -> Option<u64> {
        let t = s.get(r)?;
        t.bytes()
            .all(|c| c.is_ascii_digit())
            .then(|| t.parse().ok())?
    };
    let (y, m, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (hh, mm, ss) = (num(11..13)?, num(14..16)?, num(17..19)?);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) || hh > 23 || mm > 59 || ss > 59 {
        return None;
    }
    Some(days_from_civil(y as i64, m, d) as u64 * 86_400 + hh * 3600 + mm * 60 + ss)
}

/// Hinnant: days since 1970-01-01 from a civil date.
fn days_from_civil(y: i64, m: u64, d: u64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe as i64 - 719_468
}

/// Hinnant: civil date from days since 1970-01-01.
fn civil_from_days(z: i64) -> (i64, u64, u64) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (y + i64::from(m <= 2), m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_known_instants_and_roundtrip() {
        assert_eq!(now_rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(now_rfc3339(1_000_000_000), "2001-09-09T01:46:40Z");
        assert_eq!(now_rfc3339(951_782_400), "2000-02-29T00:00:00Z");
        for t in [
            0u64,
            1,
            951_782_400,
            1_000_000_000,
            1_700_000_000,
            4_294_967_296,
        ] {
            assert_eq!(parse_rfc3339(&now_rfc3339(t)), Some(t));
        }
    }
    #[test]
    fn parse_rfc3339_rejects_garbage() {
        for bad in [
            "garbage",
            "2001-9-09T01:46:40Z",
            "2001-09-09 01:46:40Z",
            "2001-09-09T01:46:40+00:00",
            "2001-13-09T01:46:40Z",
            "2001-09-09T25:46:40Z",
            "",
        ] {
            assert_eq!(parse_rfc3339(bad), None, "must reject {bad:?}");
        }
    }
}
