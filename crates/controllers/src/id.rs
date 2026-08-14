//! Identity derivation utilities (T3.1a): pod-template hashing, random
//! name suffixes, and placeholder pod addressing. No `rand`/`chrono` (repo
//! convention): the suffix stream is seeded from the per-process
//! `RandomState` hasher; the hash is FNV-1a 64 over canonical JSON.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

/// Insert a label into `obj.metadata.labels` (creating both if needed).
pub fn add_label(obj: &mut Value, key: &str, value: &str) {
    let Some(o) = obj.as_object_mut() else { return };
    let meta = o
        .entry("metadata")
        .or_insert_with(|| Value::Object(serde_json::Map::new()));
    let Some(m) = meta.as_object_mut() else {
        return;
    };
    let labels = m
        .entry("labels")
        .or_insert_with(|| Value::Object(serde_json::Map::new()));
    if let Some(l) = labels.as_object_mut() {
        l.insert(key.to_string(), Value::String(value.to_string()));
    }
}

/// FNV-1a 64 over the canonical JSON of `template` (BTreeMap maps make
/// `serde_json::to_string` deterministic: construction order is irrelevant).
pub fn template_hash(template: &Value) -> String {
    let s = serde_json::to_string(template).unwrap_or_default();
    format!("{:016x}", fnv1a(s.as_bytes()))
}

fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in data {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

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

static SUFFIX_COUNTER: AtomicU64 = AtomicU64::new(0);

/// `n` chars of `[0-9a-z]`, seeded from the per-process `RandomState` hasher
/// over a monotonic counter + wall clock (no `rand` crate).
pub fn rand_suffix(n: usize) -> String {
    let mut h = RandomState::new().build_hasher();
    h.write_u64(SUFFIX_COUNTER.fetch_add(1, Ordering::Relaxed));
    h.write_u64(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0),
    );
    let mut state = h.finish();
    const ALPHABET: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut out = String::with_capacity(n);
    while out.len() < n {
        let mut h = RandomState::new().build_hasher();
        h.write_u64(state);
        state = h.finish();
        let mut v = state;
        for _ in 0..12 {
            if out.len() == n {
                break;
            }
            out.push(ALPHABET[(v % 36) as usize] as char);
            v /= 36;
        }
    }
    out
}

/// Deterministic `10.42.x.y` from a stable identity (v1 stand-in until
/// kubelet/CNI assign real podIPs -- T4.2/T4.3). Octets avoid .0.0.
pub fn placeholder_pod_ip(uid: &str) -> String {
    let h = fnv1a(uid.as_bytes());
    let x = (h % 254) + 1;
    let y = ((h / 254) % 254) + 1;
    format!("10.42.{x}.{y}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn template_hash_is_construction_order_independent() {
        let a = json!({"spec": {"containers": [{"name": "c", "image": "nginx"}]},
                        "metadata": {"labels": {"z": "1", "a": "2"}}});
        let mut b = serde_json::Map::new();
        b.insert("metadata".into(), json!({"labels": {"a": "2", "z": "1"}}));
        b.insert(
            "spec".into(),
            json!({"containers": [{"image": "nginx", "name": "c"}]}),
        );
        assert_eq!(template_hash(&a), template_hash(&Value::Object(b)));
        assert_ne!(template_hash(&a), template_hash(&json!({"spec": 1})));
    }

    #[test]
    fn rand_suffix_length_and_charset() {
        for n in [1usize, 5, 10, 16] {
            let s = rand_suffix(n);
            assert_eq!(s.len(), n);
            assert!(s
                .bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()));
        }
    }

    #[test]
    fn placeholder_pod_ip_is_in_10_42_net() {
        for id in ["uid-a", "uid-b", "x"] {
            let o: Vec<u32> = placeholder_pod_ip(id)
                .split('.')
                .map(|s| s.parse().unwrap())
                .collect();
            assert_eq!(&o[..2], &[10, 42]);
            assert!((1..=254).contains(&o[2]) && (1..=254).contains(&o[3]));
        }
    }
}
