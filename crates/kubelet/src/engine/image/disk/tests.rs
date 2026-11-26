//! C3 (bug sweep) tests for `super` -- the pure disk-pressure decision
//! and GC selection rules. No filesystem, no clock: every case is a
//! threshold table or an mtime list. Split out of `disk.rs` at the
//! 400-line rule.

use super::*;

const GIB: u64 = 1024 * 1024 * 1024;

fn at(seconds_ago: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(10_000 - seconds_ago)
}

fn now() -> SystemTime {
    at(0)
}

fn ref_entry(reference: &str, age_secs: u64) -> RefEntry {
    RefEntry {
        reference: reference.to_string(),
        digest: format!("sha256:{reference}"),
        modified: at(age_secs),
    }
}

#[test]
fn zero_total_is_healthy() {
    assert_eq!(classify(0, 0), DiskPressure::Healthy);
    assert_eq!(classify(1_000, 0), DiskPressure::Healthy);
}

#[test]
fn healthy_above_both_thresholds() {
    // 20 GiB free on a 100 GiB disk: above 10% and above 2 GiB.
    assert_eq!(classify(20 * GIB, 100 * GIB), DiskPressure::Healthy);
    // Exact boundaries are healthy -- both thresholds are strict "<".
    assert_eq!(classify(10 * GIB, 100 * GIB), DiskPressure::Healthy);
    assert_eq!(classify(2 * GIB, 20 * GIB), DiskPressure::Healthy);
    // A nearly full disk is still healthy when the byte floor holds:
    // 11 GiB free of a 100 GiB disk is 11%.
    assert_eq!(classify(11 * GIB, 100 * GIB), DiskPressure::Healthy);
}

#[test]
fn pressure_under_ratio_or_under_two_gib() {
    // 9.99 GiB free on a 100 GiB disk: under 10%, still above 5%.
    assert_eq!(
        classify(9_999 * GIB / 1000, 100 * GIB),
        DiskPressure::Pressure
    );
    // Ratio fine (7.5% of 20 GiB) but under the 2 GiB byte floor.
    assert_eq!(classify(3 * GIB / 2, 20 * GIB), DiskPressure::Pressure);
    // Exactly 2 GiB with a fine ratio is healthy, not pressure.
    assert_eq!(classify(2 * GIB, 20 * GIB), DiskPressure::Healthy);
}

#[test]
fn critical_under_ratio_or_under_one_gib() {
    // 4% of a 100 GiB disk.
    assert_eq!(classify(4 * GIB, 100 * GIB), DiskPressure::Critical);
    // Under the 1 GiB floor.
    assert_eq!(
        classify(512 * 1024 * 1024, 100 * GIB),
        DiskPressure::Critical
    );
    // 1 GiB on a 100 GiB disk is 1%: Critical, not Pressure.
    assert_eq!(classify(GIB, 100 * GIB), DiskPressure::Critical);
    // Exactly 1 GiB at exactly 5% (20 GiB disk): Pressure, both
    // critical thresholds are strict.
    assert_eq!(classify(GIB, 20 * GIB), DiskPressure::Pressure);
    // Level ordering holds for every reading.
    assert!(DiskPressure::Healthy < DiskPressure::Pressure);
    assert!(DiskPressure::Pressure < DiskPressure::Critical);
}

#[test]
fn levels_have_stable_spellings() {
    assert_eq!(DiskPressure::Healthy.as_str(), "Healthy");
    assert_eq!(DiskPressure::Pressure.as_str(), "Pressure");
    assert_eq!(DiskPressure::Critical.as_str(), "Critical");
}

#[test]
fn in_use_normalizes_every_spelling() {
    let set = InUse::new(
        ["nginx", "registry.local:5000/nginx:latest", "busybox"],
        "registry.local:5000",
    );
    assert_eq!(set.len(), 2, "`nginx` and its spelled-out form are ONE key");
    assert!(set.needs("registry.local:5000/nginx:latest", "sha256:zzz"));
    assert!(set.needs("registry.local:5000/busybox:latest", "sha256:zzz"));
    assert!(!set.needs("registry.local:5000/library/nginx:1", "sha256:zzz"));
    // An unparseable spelling is kept verbatim (conservative).
    let raw = InUse::new(["A/B"], "registry.local:5000");
    assert!(raw.needs("A/B", "sha256:zzz"));
}

#[test]
fn in_use_matches_by_manifest_digest() {
    let set = InUse::new(
            ["registry.local:5000/a/b@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"],
            "registry.local:5000",
        );
    assert!(set.needs(
        "registry.local:5000/a/b:1",
        "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
    ));
    assert!(set.needs("registry.local:5000/a/b@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "sha256:other"));
    assert!(InUse::default().is_empty());
}

#[test]
fn orphans_need_grace_and_come_out_oldest_first() {
    let orphans = vec![
        Entry {
            key: "sha256:young".into(),
            modified: at(60),
        },
        Entry {
            key: "sha256:old".into(),
            modified: at(3_000),
        },
        Entry {
            key: "sha256:oldest".into(),
            modified: at(9_000),
        },
    ];
    assert_eq!(
        select_orphans(&orphans, now()),
        vec!["sha256:oldest".to_string(), "sha256:old".to_string()]
    );
    // A future mtime (clock skew) is never aged.
    let future = vec![Entry {
        key: "sha256:new".into(),
        modified: at(0) + Duration::from_secs(60),
    }];
    assert!(select_orphans(&future, now()).is_empty());
    // Exactly at the grace boundary is deletable.
    let edge = vec![Entry {
        key: "sha256:edge".into(),
        modified: at(600),
    }];
    assert_eq!(
        select_orphans(&edge, now()),
        vec!["sha256:edge".to_string()]
    );
}

#[test]
fn victims_skip_in_use_and_young_refs_and_are_bounded() {
    let in_use = InUse::new(["registry.local:5000/lib/busy:1"], "registry.local:5000");
    let refs = vec![
        ref_entry("registry.local:5000/lib/busy:1", 9_000),
        ref_entry("registry.local:5000/lib/young:1", 60),
        ref_entry("registry.local:5000/lib/a:1", 5_000),
        ref_entry("registry.local:5000/lib/b:1", 4_000),
        ref_entry("registry.local:5000/lib/c:1", 3_000),
        ref_entry("registry.local:5000/lib/d:1", 2_000),
        ref_entry("registry.local:5000/lib/e:1", 1_000),
    ];
    let picked = select_victims(&refs, &in_use, now());
    assert_eq!(picked.len(), REF_VICTIM_LIMIT);
    assert_eq!(
        picked,
        vec![
            "registry.local:5000/lib/a:1".to_string(),
            "registry.local:5000/lib/b:1".to_string(),
            "registry.local:5000/lib/c:1".to_string(),
            "registry.local:5000/lib/d:1".to_string(),
        ]
    );
}

#[test]
fn plan_follows_the_level() {
    let orphans = vec![Entry {
        key: "sha256:orphan".into(),
        modified: at(900),
    }];
    let refs = vec![ref_entry("registry.local:5000/lib/a:1", 900)];
    let in_use = InUse::new(["registry.local:5000/lib/zzz:1"], "registry.local:5000");

    let healthy = plan(DiskPressure::Healthy, &orphans, &refs, &in_use, now());
    assert!(healthy.is_empty(), "a healthy root deletes nothing");

    let pressure = plan(DiskPressure::Pressure, &orphans, &refs, &in_use, now());
    assert_eq!(pressure.orphans, vec!["sha256:orphan".to_string()]);
    assert!(
        pressure.refs.is_empty(),
        "Pressure never drops pull records"
    );

    let critical = plan(DiskPressure::Critical, &orphans, &refs, &in_use, now());
    assert_eq!(critical.orphans, vec!["sha256:orphan".to_string()]);
    assert_eq!(
        critical.refs,
        vec!["registry.local:5000/lib/a:1".to_string()]
    );
}

#[test]
fn plan_never_drops_an_in_use_ref_even_at_critical() {
    let in_use = InUse::new(["registry.local:5000/lib/a:1"], "registry.local:5000");
    let refs = vec![ref_entry("registry.local:5000/lib/a:1", 9_000)];
    let critical = plan(DiskPressure::Critical, &[], &refs, &in_use, now());
    assert!(critical.is_empty());
}
