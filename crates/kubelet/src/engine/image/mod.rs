//! T4.13 image store (Q53/Q54): OCI manifest/config pure types, registry-v2
//! pull, content-addressed CAS, layer unpack. M1 landed the pure manifest +
//! image-config faces; M2 adds the CAS (`cas`) and the registry-v2 pull
//! chain (`pull`) closing the loop to "an image Pod runs".

pub mod cas;
pub mod config;
pub mod manifest;
pub mod pull;
pub mod reference;

pub use cas::{ImageStore, PulledImage};
pub use config::{merge_argv, merge_env, parse_image_config, ConfigBody, ImageConfig};
pub use manifest::{
    parse_manifest, select_manifest, select_platform, sha256_digest, Descriptor, Manifest,
    ManifestIndex, ParsedManifest, Platform,
};
pub use pull::Puller;
pub use reference::parse_ref;
