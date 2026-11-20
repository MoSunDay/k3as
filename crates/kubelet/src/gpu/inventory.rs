//! GPU inventory source + best-effort host detection (TODO **T7.2**,
//! decision **Q61**).
//!
//! The `--gpu-inventory` agent flag selects where whole-GPU counts come
//! from: `auto` (DEFAULT — best-effort real detection, cached once per
//! heartbeat), `none` (never publish), or `fake:<resource>:<count>` for
//! hardware-free CI. Parsing is pure and total (`parse_inventory`); real
//! detection is best-effort and NEVER fails agent startup — a missing
//! driver tool or unreadable sysfs simply yields no inventory for that
//! resource. Fractional GPUs / MIG / time-slicing stay out of scope
//! (Q61): counts are whole devices.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use common::gpu::{GPU_RESOURCE_AMD, GPU_RESOURCE_NVIDIA};

/// Upper bound for `fake:<resource>:<count>` (sanity; Q61 first slice).
pub const FAKE_COUNT_MAX: u64 = 64;

/// The `--gpu-inventory` source (T7.2, Q61).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GpuInventorySource {
    /// DEFAULT: best-effort real detection (`nvidia-smi`, `rocm-smi`,
    /// sysfs PCI vendor fallback), refreshed at most once per heartbeat.
    Auto,
    /// Explicit opt-out: the node never publishes GPU inventory.
    None,
    /// Hardware-free CI inventory: `count` whole devices of `resource`
    /// (e.g. `fake:nvidia.com/gpu:1`).
    Fake { resource: String, count: u64 },
}

impl Default for GpuInventorySource {
    #[allow(clippy::derivable_impls)] // explicit: Auto is the flag default
    fn default() -> Self {
        Self::Auto
    }
}

/// Parse the `--gpu-inventory` flag value (pure, total):
/// `""`/`auto` -> [`GpuInventorySource::Auto`], `none` ->
/// [`GpuInventorySource::None`], `fake:<resource>:<count>` with a KNOWN
/// vendor resource (checked via `common::gpu`) and `count` in `1..=64`.
/// Anything else is an actionable error — the flag is explicit operator
/// intent and never silently defaults (Q61).
pub fn parse_inventory(flag: &str) -> Result<GpuInventorySource, String> {
    let flag = flag.trim();
    match flag {
        "" | "auto" => Ok(GpuInventorySource::Auto),
        "none" => Ok(GpuInventorySource::None),
        _ => {
            let rest = flag
                .strip_prefix("fake:")
                .ok_or_else(|| unknown(flag, "auto|none|fake:<resource>:<count>"))?;
            let (resource, count) = rest
                .split_once(':')
                .ok_or_else(|| unknown(flag, "fake:<resource>:<count> (missing :<count>)"))?;
            if !common::gpu::is_gpu_resource(resource) {
                return Err(format!(
                    "unknown GPU resource {resource:?} (expected one of {:?})",
                    common::gpu::GPU_RESOURCES
                ));
            }
            let count: u64 = count
                .parse()
                .map_err(|_| unknown(flag, "fake:<resource>:<count> with a numeric count"))?;
            if !(1..=FAKE_COUNT_MAX).contains(&count) {
                return Err(format!(
                    "fake GPU count {count} out of range 1..={FAKE_COUNT_MAX} (Q61 whole devices)"
                ));
            }
            Ok(GpuInventorySource::Fake {
                resource: resource.to_string(),
                count,
            })
        }
    }
}

fn unknown(flag: &str, expected: &str) -> String {
    format!("invalid --gpu-inventory value {flag:?} (expected {expected})")
}

/// Whether the auto-mode cache has a definitive measurement yet. Used to
/// distinguish "not yet measured this boot" (skip allocation quietly,
/// the heartbeat will measure) from "measured zero" (a definitive
/// insufficient-inventory error).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Totals {
    /// `auto` before the first heartbeat detection (or while a detection
    /// pass is still in flight).
    #[default]
    NotMeasured,
    /// Definitive per-resource whole-device counts (possibly empty).
    Measured(BTreeMap<String, u64>),
}

/// Resolve the definitive totals for a source (pure): `fake` and `none`
/// are static; `auto` adopts the latest detection (or
/// [`Totals::NotMeasured`] when none ran yet).
pub fn resolve_totals(source: &GpuInventorySource, detected: &Totals) -> Totals {
    match source {
        GpuInventorySource::Auto => detected.clone(),
        GpuInventorySource::None => Totals::Measured(BTreeMap::new()),
        GpuInventorySource::Fake { resource, count } => {
            Totals::Measured(BTreeMap::from([(resource.clone(), *count)]))
        }
    }
}

/// Per-heartbeat detection cache: pure struct, refreshed through
/// [`TotalsCache::refresh`] with an injected clock so tests stay
/// deterministic.
#[derive(Debug, Default)]
pub struct TotalsCache {
    totals: Totals,
    at: Option<Instant>,
}

impl TotalsCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Return the cached totals, running `detect` only when the TTL
    /// expired (once per heartbeat; Q61 "cache per heartbeat").
    pub fn refresh(
        &mut self,
        ttl: Duration,
        now: Instant,
        detect: impl FnOnce() -> BTreeMap<String, u64>,
    ) -> Totals {
        if self.at.is_none() || now.duration_since(self.at.unwrap()) >= ttl {
            self.totals = Totals::Measured(detect());
            self.at = Some(now);
        }
        self.totals.clone()
    }

    /// The current cached value without refreshing.
    pub fn peek(&self) -> &Totals {
        &self.totals
    }
}

/// sysfs PCI vendor IDs (best-effort fallback when vendor CLIs are absent).
const VENDOR_ID_NVIDIA: &str = "0x10de";
const VENDOR_ID_AMD: &str = "0x1002";

/// Map one sysfs vendor-id file body to its GPU resource name (pure).
pub fn classify_vendor_id(body: &str) -> Option<&'static str> {
    match body.trim() {
        VENDOR_ID_NVIDIA => Some(GPU_RESOURCE_NVIDIA),
        VENDOR_ID_AMD => Some(GPU_RESOURCE_AMD),
        _ => None,
    }
}

/// Count non-empty output lines (one `nvidia-smi --query-gpu=index` line
/// per GPU). Pure; used by both the tool path and its tests.
pub fn count_lines(stdout: &str) -> u64 {
    stdout.lines().filter(|l| !l.trim().is_empty()).count() as u64
}

/// Count `rocm-smi --showproductname --csv` data rows: non-empty lines
/// that are not the header row (the header is the only line carrying
/// "Card,"/"card" column labels; data rows start with a card id).
pub fn count_rocm_rows(stdout: &str) -> u64 {
    stdout
        .lines()
        .filter(|l| {
            let t = l.trim();
            !t.is_empty() && !t.to_ascii_lowercase().starts_with("card,")
        })
        .count() as u64
}

/// Best-effort real detection (async; every failure degrades to
/// "resource absent", never an error): NVIDIA via `nvidia-smi`, AMD via
/// `rocm-smi` with the sysfs `/sys/class/drm/card*/device/vendor` scan as
/// the fallback for both vendors.
pub async fn detect_host_gpus() -> BTreeMap<String, u64> {
    let mut out: BTreeMap<String, u64> = BTreeMap::new();
    if let Some(n) = run_tool(
        &["nvidia-smi", "--query-gpu=index", "--format=csv,noheader"],
        count_lines,
    )
    .await
    .filter(|n| *n > 0)
    {
        out.insert(GPU_RESOURCE_NVIDIA.to_string(), n);
    }
    let amd_tool = run_tool(&["rocm-smi", "--showproductname", "--csv"], count_rocm_rows).await;
    match amd_tool.filter(|n| *n > 0) {
        Some(n) => {
            out.insert(GPU_RESOURCE_AMD.to_string(), n);
        }
        None => {
            // sysfs fallback covers both vendors when the CLIs are absent.
            for (res, n) in sysfs_vendor_counts() {
                out.entry(res).or_insert(n);
            }
        }
    }
    out
}

/// Run one vendor CLI with a bounded timeout; `None` on any failure
/// (missing binary, non-zero exit, timeout, utf8 noise).
async fn run_tool(argv: &[&str], count: fn(&str) -> u64) -> Option<u64> {
    let out = tokio::time::timeout(
        Duration::from_secs(3),
        tokio::process::Command::new(argv[0])
            .args(&argv[1..])
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(count(&String::from_utf8_lossy(&out.stdout)))
}

/// Scan `/sys/class/drm/card*/device/vendor` (blocking read of a handful
/// of tiny sysfs files; best-effort, empty on any error).
pub fn sysfs_vendor_counts() -> BTreeMap<String, u64> {
    let mut out: BTreeMap<String, u64> = BTreeMap::new();
    let Ok(entries) = std::fs::read_dir("/sys/class/drm") else {
        return out;
    };
    for e in entries.flatten() {
        let vendor = e.path().join("device/vendor");
        let Ok(body) = std::fs::read_to_string(&vendor) else {
            continue;
        };
        if let Some(res) = classify_vendor_id(&body) {
            *out.entry(res.to_string()).or_insert(0) += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gpu_t72_parse_inventory_rules() {
        assert_eq!(parse_inventory(""), Ok(GpuInventorySource::Auto));
        assert_eq!(parse_inventory("auto"), Ok(GpuInventorySource::Auto));
        assert_eq!(parse_inventory(" none "), Ok(GpuInventorySource::None));
        assert_eq!(
            parse_inventory("fake:nvidia.com/gpu:1"),
            Ok(GpuInventorySource::Fake {
                resource: "nvidia.com/gpu".into(),
                count: 1
            })
        );
        assert_eq!(
            parse_inventory("fake:amd.com/gpu:64"),
            Ok(GpuInventorySource::Fake {
                resource: "amd.com/gpu".into(),
                count: 64
            })
        );
    }

    #[test]
    fn gpu_t72_parse_inventory_rejects_bad_values() {
        for bad in [
            "bogus",
            "fake",
            "fake:nvidia.com/gpu",
            "fake:cpu:1",
            "fake:nvidia.com/gpu:0",
            "fake:nvidia.com/gpu:65",
            "fake:nvidia.com/gpu:-1",
            "fake:nvidia.com/gpu:x",
        ] {
            assert!(
                parse_inventory(bad).is_err(),
                "value {bad:?} must not parse"
            );
        }
    }

    #[test]
    fn gpu_t72_resolve_totals_static_sources() {
        let e = BTreeMap::new();
        assert_eq!(
            resolve_totals(&GpuInventorySource::None, &Totals::NotMeasured),
            Totals::Measured(e.clone())
        );
        assert_eq!(
            resolve_totals(
                &GpuInventorySource::Fake {
                    resource: "nvidia.com/gpu".into(),
                    count: 2
                },
                &Totals::NotMeasured
            ),
            Totals::Measured(BTreeMap::from([("nvidia.com/gpu".into(), 2)]))
        );
        assert_eq!(
            resolve_totals(&GpuInventorySource::Auto, &Totals::NotMeasured),
            Totals::NotMeasured
        );
        let measured = Totals::Measured(BTreeMap::from([("amd.com/gpu".into(), 1)]));
        assert_eq!(
            resolve_totals(&GpuInventorySource::Auto, &measured),
            measured
        );
    }

    #[test]
    fn gpu_t72_totals_cache_refreshes_once_per_ttl() {
        let mut c = TotalsCache::new();
        let t0 = Instant::now();
        let ttl = Duration::from_secs(10);
        let mut calls = 0;
        let t = c.refresh(ttl, t0, || {
            calls += 1;
            BTreeMap::from([("nvidia.com/gpu".to_string(), 1)])
        });
        assert_eq!(
            t,
            Totals::Measured(BTreeMap::from([("nvidia.com/gpu".to_string(), 1)]))
        );
        // Within the TTL: cached, no new detection.
        let _ = c.refresh(ttl, t0 + Duration::from_secs(2), || {
            calls += 1;
            BTreeMap::new()
        });
        assert_eq!(calls, 1);
        // Past the TTL (one heartbeat later): re-detect.
        let t2 = c.refresh(ttl, t0 + ttl, || {
            calls += 1;
            BTreeMap::new()
        });
        assert_eq!(calls, 2);
        assert_eq!(t2, Totals::Measured(BTreeMap::new()));
    }

    #[test]
    fn gpu_t72_line_and_vendor_classification() {
        assert_eq!(count_lines("0\n1\n\n2\n"), 3);
        assert_eq!(
            count_rocm_rows("Card,Card series\n card0,AMD Instinct\n"),
            1
        );
        assert_eq!(classify_vendor_id("0x10de\n"), Some("nvidia.com/gpu"));
        assert_eq!(classify_vendor_id("0x1002\n"), Some("amd.com/gpu"));
        assert_eq!(classify_vendor_id("0x8086\n"), None); // i915: not a GPU vendor we track
    }
}
