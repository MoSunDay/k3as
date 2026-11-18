//! T4.13 image store (Q53/Q54): OCI manifest/config pure types, registry-v2
//! pull, content-addressed CAS, layer unpack. This milestone lands the pure
//! manifest + image-config faces; pull/CAS/unpack follow in later B-M slices.

pub mod config;
pub mod manifest;

pub use config::{merge_argv, merge_env, parse_image_config, ConfigBody, ImageConfig};
pub use manifest::{
    parse_manifest, select_manifest, select_platform, sha256_digest, Descriptor, Manifest,
    ManifestIndex, ParsedManifest, Platform,
};
