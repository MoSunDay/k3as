//! Pure cron expression parsing + next-fire math (TODO **T7.3 Scope C**,
//! decision **Q30**).
//!
//! The CronWorkflow controller's schedule engine: the robfig `@every`
//! descriptor family (integer+unit duration pairs, `s`/`m`/`h`/`d`) and
//! the standard 5-field cron (minute hour dom month dow; `*`, lists `,`,
//! ranges `-`, steps `*/n`; dow `0-7` with `7 == Sunday`; Vixie
//! dom/dow EITHER semantics when both are restricted). `@hourly`,
//! `@daily`/`@midnight`, `@weekly`, `@monthly`, `@yearly`/`@annually`
//! alias their field forms.
//!
//! Everything is UTC, timezone-naive: `spec.timezone` is NOT honored
//! (documented v1 simplification, parked as future scope). Pure and
//! side-effect-free so the semantics are unit-testable in isolation; the
//! civil-date math is the Howard Hinnant algorithm (same one
//! `common::time` uses; private there, so a local pure copy). JSON-only
//! wire (Q10).

/// A parsed cron schedule.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CronExpr {
    /// `@every <duration>`: epoch-anchored interval (fires at unix-time
    /// multiples of the interval).
    Every { interval_secs: i64 },
    /// Standard 5-field cron, minute granularity.
    Fields {
        minute: Field,
        hour: Field,
        dom: Field,
        month: Field,
        dow: Field,
    },
}

/// One cron field: allowed values as a bitmask over `[min, max]`, plus
/// whether the field was a literal `*` (unrestricted -- the Vixie
/// dom/dow EITHER rule keys off this, not off "full range").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Field {
    pub bits: u64,
    pub star: bool,
}

impl Field {
    fn contains(&self, v: i64) -> bool {
        (0..64).contains(&v) && (self.bits >> v) & 1 == 1
    }
}

/// Candidate-minute search cap for field crons (~366 days); `None` past
/// it (e.g. `0 0 31 2 *` never matches).
const SEARCH_CAP_MINUTES: i64 = 366 * 24 * 60;

/// Parse a schedule expression; `None` when unsupported or malformed.
pub fn parse(expr: &str) -> Option<CronExpr> {
    let expr = expr.trim();
    if let Some(rest) = expr.strip_prefix("@every") {
        let interval_secs = parse_duration(rest.trim())?;
        if interval_secs <= 0 {
            return None;
        }
        return Some(CronExpr::Every { interval_secs });
    }
    let expanded = match expr {
        "@hourly" => "0 * * * *",
        "@daily" | "@midnight" => "0 0 * * *",
        "@weekly" => "0 0 * * 0",
        "@monthly" => "0 0 1 * *",
        "@yearly" | "@annually" => "0 0 1 1 *",
        other => {
            if other.starts_with('@') {
                return None; // unknown descriptor (@reboot, @daily-x, ...)
            }
            other
        }
    };
    let fields: Vec<&str> = expanded.split_whitespace().collect();
    if fields.len() != 5 {
        return None;
    }
    let dow = parse_field(fields[4], 0, 7)?;
    // dow 7 == Sunday == dow 0 (standard cron).
    let dow = Field {
        bits: (dow.bits & !(1 << 7)) | u64::from(dow.bits & (1 << 7) != 0),
        star: dow.star,
    };
    Some(CronExpr::Fields {
        minute: parse_field(fields[0], 0, 59)?,
        hour: parse_field(fields[1], 0, 23)?,
        dom: parse_field(fields[2], 1, 31)?,
        month: parse_field(fields[3], 1, 12)?,
        dow,
    })
}

/// Next fire time STRICTLY after `from_unix` (UTC). `Every` fires at
/// epoch multiples of the interval; field crons fire on minute
/// boundaries. `None` past the ~366-day search cap.
pub fn next_after(expr: &str, from_unix: i64) -> Option<i64> {
    match parse(expr)? {
        CronExpr::Every { interval_secs } => {
            if interval_secs <= 0 {
                return None;
            }
            // Strictly after: a `from_unix` on a fire instant maps to the
            // NEXT multiple (rem 0 -> +interval).
            let rem = from_unix.rem_euclid(interval_secs);
            Some(from_unix + (interval_secs - rem))
        }
        CronExpr::Fields {
            minute,
            hour,
            dom,
            month,
            dow,
        } => {
            // First candidate minute strictly after from_unix.
            let mut t = from_unix.div_euclid(60) * 60 + 60;
            for _ in 0..SEARCH_CAP_MINUTES {
                if matches(minute, hour, dom, month, dow, t) {
                    return Some(t);
                }
                t += 60;
            }
            None
        }
    }
}

/// Field-cron match for one minute-aligned instant.
fn matches(minute: Field, hour: Field, dom: Field, month: Field, dow: Field, t: i64) -> bool {
    let (m, h, d, mon, wd) = decompose(t);
    let dom_ok = dom.contains(d);
    let dow_ok = dow.contains(wd);
    // Vixie: when BOTH day fields are restricted, EITHER may match;
    // otherwise both must (a `*` field contains everything anyway).
    let day_ok = if !dom.star && !dow.star {
        dom_ok || dow_ok
    } else {
        dom_ok && dow_ok
    };
    minute.contains(m) && hour.contains(h) && month.contains(mon) && day_ok
}

/// `(minute, hour, dom, month, dow)` for a unix timestamp (UTC).
fn decompose(unix: i64) -> (i64, i64, i64, i64, i64) {
    let days = unix.div_euclid(86_400);
    let secs = unix.rem_euclid(86_400);
    let (_year, month, dom) = civil_from_days(days);
    let dow = (days + 4).rem_euclid(7); // 1970-01-01 was a Thursday
    ((secs / 60) % 60, secs / 3600, dom, month, dow)
}

/// Hinnant `civil_from_days`: `(year, month, day)` from days since the
/// epoch (pure local copy -- the `common::time` original is private).
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (y + i64::from(m <= 2), m as i64, d as i64)
}

/// `@every` duration grammar: integer+unit pairs (`90s`, `2m`, `1m30s`),
/// units `s`/`m`/`h`/`d`; the total must be positive.
fn parse_duration(s: &str) -> Option<i64> {
    if s.is_empty() {
        return None;
    }
    let mut total = 0i64;
    let mut digits = String::new();
    let mut pairs = 0usize;
    for c in s.chars() {
        if c.is_ascii_digit() {
            digits.push(c);
            continue;
        }
        let mult = match c {
            's' => 1,
            'm' => 60,
            'h' => 3600,
            'd' => 86_400,
            _ => return None,
        };
        let n: i64 = digits.parse().ok()?;
        total += n.checked_mul(mult)?;
        digits.clear();
        pairs += 1;
    }
    if !digits.is_empty() || pairs == 0 || total <= 0 {
        return None;
    }
    Some(total)
}

/// One 5-field cron field: comma-separated `*` / `n` / `a-b` ranges with
/// optional `/step` (step 0 rejected; values bounds-checked).
fn parse_field(field: &str, min: u32, max: u32) -> Option<Field> {
    if field.is_empty() {
        return None;
    }
    let star = field == "*";
    let mut bits = 0u64;
    for part in field.split(',') {
        if part.is_empty() {
            return None;
        }
        let (range, step) = match part.split_once('/') {
            None => (part, 1u32),
            Some((r, s)) => (r, parse_u32(s)?),
        };
        if step == 0 {
            return None;
        }
        let (lo, hi) = if range == "*" {
            (min, max)
        } else {
            match range.split_once('-') {
                None => {
                    let v = parse_u32(range)?;
                    (v, v)
                }
                Some((a, b)) => (parse_u32(a)?, parse_u32(b)?),
            }
        };
        if lo < min || hi > max || lo > hi {
            return None;
        }
        let mut v = lo;
        while v <= hi {
            bits |= 1 << v;
            v += step;
        }
    }
    if bits == 0 {
        return None;
    }
    Some(Field { bits, star })
}

/// Unsigned decimal only (no sign, no whitespace; leading zeros fine).
fn parse_u32(s: &str) -> Option<u32> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse::<u32>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_math_every_five_minutes() {
        // Epoch is 1970-01-01T00:00:00Z (Thursday): minute 5 == t=300.
        assert_eq!(next_after("*/5 * * * *", 0), Some(300));
        assert_eq!(next_after("*/5 * * * *", 299), Some(300));
        // Strictly-after: a fire instant maps to the NEXT one.
        assert_eq!(next_after("*/5 * * * *", 300), Some(600));
        assert_eq!(next_after("*/5 * * * *", 301), Some(600));
        // Steps apply inside ranges too.
        assert_eq!(next_after("10-40/30 * * * *", 0), Some(10 * 60));
        assert_eq!(next_after("10-40/30 * * * *", 10 * 60), Some(40 * 60));
    }

    #[test]
    fn day_of_week_and_hour() {
        // First Monday after the epoch: 1970-01-05T02:30 == 354_600.
        assert_eq!(next_after("30 2 * * 1", 0), Some(354_600));
        // Next Monday, same time (7 days later).
        assert_eq!(next_after("30 2 * * 1", 354_600), Some(959_400));
        // dow 7 == Sunday == dow 0 (1970-01-04 == 259_200).
        assert_eq!(next_after("0 0 * * 7", 0), Some(259_200));
        assert_eq!(next_after("0 0 * * 0", 0), Some(259_200));
        assert_eq!(parse("0 0 * * 7"), parse("0 0 * * 0"));
    }

    #[test]
    fn dom_month_and_either_day_semantics() {
        // 1970-02-01T00:00 == 2_678_400.
        assert_eq!(next_after("0 0 1 2 *", 0), Some(2_678_400));
        // dom AND dow both restricted -> EITHER: 1970-01-01 (dom 1, Thu)
        // misses, but Sunday 1970-01-04 matches dow.
        assert_eq!(next_after("0 0 1 * 0", 0), Some(259_200));
        // Only dom restricted (dow `*`): day 1 alone decides. t=0 IS
        // 1970-01-01 (a fire instant), so strictly-after is Feb 1st.
        assert_eq!(next_after("0 0 1 * *", 0), Some(2_678_400));
    }

    #[test]
    fn every_descriptor_boundaries() {
        assert_eq!(
            parse("@every 90s"),
            Some(CronExpr::Every { interval_secs: 90 })
        );
        assert_eq!(next_after("@every 90s", 0), Some(90));
        assert_eq!(next_after("@every 90s", 89), Some(90));
        // Strictly-after: firing at 90 schedules the next at 180.
        assert_eq!(next_after("@every 90s", 90), Some(180));
        assert_eq!(next_after("@every 90s", 91), Some(180));
    }

    #[test]
    fn compound_duration() {
        assert_eq!(
            parse("@every 1m30s"),
            Some(CronExpr::Every { interval_secs: 90 })
        );
        assert_eq!(
            parse("@every 2h45m"),
            Some(CronExpr::Every {
                interval_secs: 9_900
            })
        );
        assert_eq!(
            parse("@every 1d"),
            Some(CronExpr::Every {
                interval_secs: 86_400
            })
        );
        assert_eq!(next_after("@every 1m30s", 0), Some(90));
    }

    #[test]
    fn aliases_map_to_field_exprs() {
        assert_eq!(parse("@hourly"), parse("0 * * * *"));
        assert_eq!(parse("@daily"), parse("0 0 * * *"));
        assert_eq!(parse("@weekly"), parse("0 0 * * 0"));
        assert_eq!(parse("@monthly"), parse("0 0 1 * *"));
        // From t=0 the next strictly-after fires skip the epoch instant.
        assert_eq!(next_after("@hourly", 0), Some(3_600));
        assert_eq!(next_after("@daily", 0), Some(86_400));
        assert_eq!(next_after("@weekly", 0), Some(259_200));
        assert_eq!(next_after("@monthly", 0), Some(2_678_400));
    }

    #[test]
    fn invalid_expressions_rejected() {
        for bad in [
            "* * *",       // 3 fields
            "* * * * * *", // 6 fields
            "",            // empty
            "@every",      // missing duration
            "@every 0s",   // zero interval
            "@every 1x",   // unknown unit
            "@every 30",   // missing unit
            "@every m",    // missing number
            "*/0 * * * *", // zero step
            "*/x * * * *", // non-numeric step
            "61 * * * *",  // minute out of range
            "* 24 * * *",  // hour out of range
            "5-2 * * * *", // inverted range
            "0 0 0 0 *",   // month 0
            "0 0 32 * *",  // dom out of range
            "0 0 * 13 *",  // month 13
            "@reboot",     // unsupported descriptor
            "1, * * * *",  // empty list item
        ] {
            assert_eq!(parse(bad), None, "must reject {bad:?}");
        }
    }

    #[test]
    fn impossible_date_hits_search_cap() {
        // February 31st never exists: the ~366-day search ends empty.
        assert_eq!(next_after("0 0 31 2 *", 0), None);
    }

    #[test]
    fn next_is_always_strictly_after() {
        for expr in [
            "*/5 * * * *",
            "30 2 * * 1",
            "@every 90s",
            "@hourly",
            "15 10 13 * 5",
        ] {
            for t in [0i64, 1, 59, 60, 300, 354_600, 1_700_000_123] {
                let next = next_after(expr, t).unwrap_or_else(|| panic!("{expr}"));
                assert!(next > t, "{expr} at {t}: {next}");
            }
        }
        // Field crons always fire on minute boundaries; @every need not.
        for expr in ["*/5 * * * *", "30 2 * * 1", "@hourly", "15 10 13 * 5"] {
            assert_eq!(next_after(expr, 1_700_000_123).unwrap() % 60, 0, "{expr}");
        }
    }
}
