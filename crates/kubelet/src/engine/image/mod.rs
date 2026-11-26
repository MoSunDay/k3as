//! T4.13 image store (Q53/Q54): OCI manifest/config pure types, registry-v2
//! pull, content-addressed CAS, per-layer unpack + whiteouts. M1 landed
//! the pure manifest + image-config faces; M2 added the CAS (`cas`) and the
//! registry-v2 pull chain (`pull`); M3 adds shared per-layer dirs with
//! kernel/logical whiteouts (`unpack`, `layers`) feeding per-container
//! overlayfs views or the fused-copy fallback (Q53).

pub mod cas;
pub mod config;
pub mod disk;
pub mod gc;
pub mod layers;
pub mod manifest;
pub mod probe;
pub mod pull;
pub mod reference;
pub mod stage;
pub mod unpack;

pub use cas::{ImageStore, PulledImage};
pub use config::{merge_argv, merge_env, parse_image_config, ConfigBody, ImageConfig};
pub use disk::{
    classify, DiskPressure, Entry, GcPlan, InUse, RefEntry, CRITICAL_MIN_AVAILABLE,
    CRITICAL_RATIO_PERCENT, GRACE, PRESSURE_MIN_AVAILABLE, PRESSURE_RATIO_PERCENT,
    REF_VICTIM_LIMIT,
};
pub(crate) use layers::is_staging_name;
pub use layers::ViewPaths;
pub use manifest::{
    parse_manifest, select_manifest, select_platform, sha256_digest, Descriptor, Manifest,
    ManifestIndex, ParsedManifest, Platform,
};
pub use pull::{Puller, MAX_MANIFEST_BYTES};
pub use reference::parse_ref;
