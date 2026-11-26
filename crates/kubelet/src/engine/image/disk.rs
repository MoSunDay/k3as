//! C3 (bug sweep): disk-pressure decision + GC victim selection for the
//! T4.13 image store (Q53/Q54).
//!
//! PURE module: no I/O, no clock. The caller measures the image root
//! filesystem with `nix::sys::statvfs::statvfs`, reads the candidate
//! mtimes, and feeds both in here -- this module only answers "how bad is
//! it" ([`classify`]) and "what may be deleted now" ([`plan`]). Every
//! threshold and every ordering rule lives in this file so the behavior is
//! reviewable (and unit-testable) without a filesystem.
//!
//! Policy (C3):
//! - `Healthy`  -- available >= max(2 GiB, 10% of the filesystem).
//! - `Pressure` -- below that: delete ORPHANS only (blobs / per-layer dirs
//!   / fused `merged/` dirs no pull record owns), grace-filtered.
//! - `Critical` -- below max(1 GiB, 5%): orphans PLUS the oldest pull
//!   records that no live pod or container needs (bounded per pass).
//!
//! The grace age is the load-bearing safety rule: the pull chain commits
//! blobs into the CAS and unpacks `layers/` BEFORE `refs/<key>.json`
//! exists, so a young unreferenced digest may be seconds away from
//! becoming a running image's rootfs. Anything younger than [`GRACE`] is
//! therefore untouchable, whatever the level.

use std::collections::BTreeSet;
use std::time::{Duration, SystemTime};

use super::reference::{normalized, parse_ref};

/// `Pressure` when available is under this share of the filesystem.
pub const PRESSURE_RATIO_PERCENT: u64 = 10;
/// ... or under this many free bytes.
pub const PRESSURE_MIN_AVAILABLE: u64 = 2 * 1024 * 1024 * 1024;
/// `Critical` when available is under this share of the filesystem.
pub const CRITICAL_RATIO_PERCENT: u64 = 5;
/// ... or under this many free bytes.
pub const CRITICAL_MIN_AVAILABLE: u64 = 1024 * 1024 * 1024;
/// Minimum age of a deletable entry (an in-flight pull is never swept).
pub const GRACE: Duration = Duration::from_secs(10 * 60);
/// Critical-stage budget: pull records dropped per pass (the next sync
/// cycle continues; one cycle never walks the whole store).
pub const REF_VICTIM_LIMIT: usize = 4;

/// Pressure level of the image root filesystem. Ordered
/// `Healthy < Pressure < Critical` so callers can compare levels; the
/// default is `Healthy` (a stats struct starts out as "nothing to do").
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum DiskPressure {
    #[default]
    Healthy,
    Pressure,
    Critical,
}

impl DiskPressure {
    /// Stable spelling for logs and tests.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Healthy => "Healthy",
            Self::Pressure => "Pressure",
            Self::Critical => "Critical",
        }
    }
}

/// Classify one measurement of the image root filesystem.
///
/// Total (an unmeasurable or zero-sized filesystem) reads `Healthy`: a
/// blind sweep is worse than a full disk. Exact-boundary readings are
/// healthy -- the thresholds are strict "less than".
pub fn classify(available_bytes: u64, total_bytes: u64) -> DiskPressure {
    if total_bytes == 0 {
        return DiskPressure::Healthy;
    }
    if under_ratio(available_bytes, total_bytes, CRITICAL_RATIO_PERCENT)
        || available_bytes < CRITICAL_MIN_AVAILABLE
    {
        DiskPressure::Critical
    } else if under_ratio(available_bytes, total_bytes, PRESSURE_RATIO_PERCENT)
        || available_bytes < PRESSURE_MIN_AVAILABLE
    {
        DiskPressure::Pressure
    } else {
        DiskPressure::Healthy
    }
}

/// `available * 100 < total * percent`, on `u128` so neither the
/// comparison nor the multiplication can overflow or lose precision.
fn under_ratio(available_bytes: u64, total_bytes: u64, percent: u64) -> bool {
    u128::from(available_bytes) * 100 < u128::from(total_bytes) * u128::from(percent)
}

/// The images a pressure pass must NEVER delete: every spelling the
/// kubelet's desired pod set and the engine's live registry still need,
/// keyed the way [`super::cas::PulledImage::reference`] records them,
/// plus any `@sha256:` digest spelling (`repo@sha256:...` lookups).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct InUse {
    references: BTreeSet<String>,
    digests: BTreeSet<String>,
}

impl InUse {
    /// Normalize raw image spellings (`nginx`, `host/repo:tag`,
    /// `repo@sha256:...`) against `host`, the registry bare references
    /// resolve on. An unparseable spelling is kept verbatim: an unknown
    /// key can only make the GC more conservative, never less.
    pub fn new<'a, I>(images: I, host: &str) -> Self
    where
        I: IntoIterator<Item = &'a str>,
    {
        let mut set = Self::default();
        for image in images {
            let image = image.trim();
            if image.is_empty() {
                continue;
            }
            match parse_ref(image, host) {
                Ok(pr) => {
                    set.references.insert(normalized(&pr));
                    if let Some(d) = pr.digest {
                        set.digests.insert(d);
                    }
                }
                Err(_) => {
                    set.references.insert(image.to_string());
                }
            }
        }
        set
    }

    /// Does something still need the pull record `reference` /
    /// manifest `digest`?
    pub fn needs(&self, reference: &str, digest: &str) -> bool {
        self.references.contains(reference) || self.digests.contains(digest)
    }

    /// Number of normalized keys held (tests + log fields).
    pub fn len(&self) -> usize {
        self.references.len() + self.digests.len()
    }

    /// True when nothing is protected.
    pub fn is_empty(&self) -> bool {
        self.references.is_empty() && self.digests.is_empty()
    }
}

/// One on-disk candidate offered to the selector: the caller reads the
/// filesystem, this module only reads the data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// CAS digest (`sha256:<hex>`) for orphans.
    pub key: String,
    /// mtime of the blob / dir.
    pub modified: SystemTime,
}

/// A pull record (`refs/<key>.json`) with the mtime of its file: the age
/// of the LAST PULL, which is the only use signal the store records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefEntry {
    pub reference: String,
    pub digest: String,
    pub modified: SystemTime,
}

/// What one pass decided to free. Both lists are oldest-first.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct GcPlan {
    /// Orphan digests: blob + `layers/` dir + `merged/` dir.
    pub orphans: Vec<String>,
    /// Pull records to drop (Critical only); their unique blobs become
    /// orphans for the sweep that follows.
    pub refs: Vec<String>,
}

impl GcPlan {
    /// Nothing to do at this level.
    pub fn is_empty(&self) -> bool {
        self.orphans.is_empty() && self.refs.is_empty()
    }
}

/// The whole C3 policy in one call: which orphans and which pull records
/// may be deleted at `pressure`, given the protected set `in_use`.
pub fn plan(
    pressure: DiskPressure,
    orphans: &[Entry],
    refs: &[RefEntry],
    in_use: &InUse,
    now: SystemTime,
) -> GcPlan {
    let mut plan = GcPlan::default();
    if pressure == DiskPressure::Healthy {
        return plan;
    }
    plan.orphans = select_orphans(orphans, now);
    if pressure == DiskPressure::Critical {
        plan.refs = select_victims(refs, in_use, now);
    }
    plan
}

/// Orphans old enough to delete, oldest first. The grace filter is what
/// keeps an in-flight pull safe (see the module doc).
pub fn select_orphans(orphans: &[Entry], now: SystemTime) -> Vec<String> {
    let mut picked: Vec<&Entry> = orphans.iter().filter(|e| aged(e.modified, now)).collect();
    picked.sort_by_key(|e| e.modified);
    picked.into_iter().map(|e| e.key.clone()).collect()
}

/// Critical-only second stage: the OLDEST pull records that no live pod,
/// container or sandbox needs, still grace-protected, at most
/// [`REF_VICTIM_LIMIT`] per pass.
pub fn select_victims(refs: &[RefEntry], in_use: &InUse, now: SystemTime) -> Vec<String> {
    let mut picked: Vec<&RefEntry> = refs
        .iter()
        .filter(|r| aged(r.modified, now) && !in_use.needs(&r.reference, &r.digest))
        .collect();
    picked.sort_by_key(|r| r.modified);
    picked
        .into_iter()
        .take(REF_VICTIM_LIMIT)
        .map(|r| r.reference.clone())
        .collect()
}

/// `modified + GRACE <= now`. A future or otherwise invalid mtime is
/// never aged -- unknown age means "keep".
fn aged(modified: SystemTime, now: SystemTime) -> bool {
    match now.duration_since(modified) {
        Ok(age) => age >= GRACE,
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests;
