//! Kubernetes resource-quantity parsing (TODO **T3.2** originally, moved
//! here for **T4.9**): the single parser both sides of the cluster share —
//! the scheduler sums `resources.requests` for ResourceFit (Q23) and the
//! kubelet maps requests/limits into CRI cgroup knobs. Pure and
//! dependency-free (kept in `common`, not a heavier layer).

/// Parse one Kubernetes quantity (`"100m"`, `"1.5"`, `"128Mi"`, `"1e3"`,
/// `"2Ki"`) into base units (cores for cpu, bytes for memory). Returns
/// `None` for malformed values; a missing field is handled by callers,
/// not here.
pub fn parse_quantity(s: &str) -> Option<f64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    // Find the split between mantissa and suffix (last char boundary where
    // the remainder is a known unit).
    const SUFFIXES: &[(&str, f64)] = &[
        ("n", 1e-9),
        ("u", 1e-6),
        ("m", 1e-3),
        ("k", 1e3),
        ("M", 1e6),
        ("G", 1e9),
        ("T", 1e12),
        ("P", 1e15),
        ("E", 1e18),
        ("Ki", 1024.0),
        ("Mi", 1_048_576.0),
        ("Gi", 1_073_741_824.0),
        ("Ti", 1_099_511_627_776.0),
        ("Pi", 1_125_899_906_842_624.0),
        ("Ei", 1_152_921_504_606_846_976.0),
    ];
    for (suffix, mult) in SUFFIXES {
        if let Some(num) = s.strip_suffix(suffix) {
            let v: f64 = num.parse().ok()?;
            return Some(v * mult);
        }
    }
    s.parse::<f64>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn covers_si_binary_and_milli() {
        assert_eq!(parse_quantity("1"), Some(1.0));
        assert_eq!(parse_quantity("1.5"), Some(1.5));
        assert_eq!(parse_quantity("100m"), Some(0.1));
        assert_eq!(parse_quantity("2500m"), Some(2.5));
        assert_eq!(parse_quantity("128Mi"), Some(128.0 * 1024.0 * 1024.0));
        assert_eq!(parse_quantity("1Gi"), Some(1024.0 * 1024.0 * 1024.0));
        assert_eq!(parse_quantity("2k"), Some(2000.0));
        assert_eq!(parse_quantity("1e3"), Some(1000.0));
        assert_eq!(parse_quantity("500u"), Some(0.0005));
        assert_eq!(parse_quantity("garbage"), None);
        assert_eq!(parse_quantity(""), None);
    }
}
