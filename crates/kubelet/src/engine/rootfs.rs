//! Rootfs source selection for the built-in engine (T4.14, Q55).
//!
//! Q55 makes HOST directories the default container root (`/`): the image
//! reference is an absolute directory path, or a name staged under
//! `<data_dir>/engine/images/<name>` (the T4.12 legacy staging under
//! `<data_dir>/images/<name>` stays a fallback). The Q53 registry-pull
//! chain becomes opt-in: the engine flag `--builtin-rootfs image`, or the
//! per-pod annotation `init-pro.io/rootfs: image`. Pure functions + plain
//! data only — the backend composes them; everything is testable without
//! root or namespaces.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Pod annotation overriding the engine-level rootfs source for one pod
/// (T4.14, Q55): `init-pro.io/rootfs: host|image`.
pub const ROOTFS_ANNOTATION: &str = "init-pro.io/rootfs";

/// CAS control directories under `<data_dir>/engine/images` — a staged
/// rootfs name may never shadow them (Q55).
const CAS_CONTROL_DIRS: [&str; 6] = ["blobs", "refs", "layers", "views", "merged", "tmp"];

/// The two rootfs sources (Q55).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootfsMode {
    /// Host directories as `/` (the Q55 default): absolute image paths
    /// and staged dirs; EnsureImage is a side-effect-free presence check.
    Host,
    /// The Q53 registry-pull chain (T4.13 semantics): CAS refs,
    /// per-container overlayfs views, and the image config as the launch
    /// source of truth.
    Image,
}

impl RootfsMode {
    /// Parse the `--builtin-rootfs` flag value (clap pre-validates; this
    /// is the defensive single point for string -> mode).
    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "host" => Ok(Self::Host),
            "image" => Ok(Self::Image),
            other => Err(format!(
                "unknown builtin rootfs mode {other:?} (expected host|image)"
            )),
        }
    }
}

/// Resolve the mode for one pod: the `init-pro.io/rootfs` annotation
/// (`host`|`image`) overrides the engine default; a malformed annotation
/// value is an error (observable at create time), never a silent default
/// (Q55).
pub fn resolve_mode(
    default: RootfsMode,
    annotations: &BTreeMap<String, String>,
) -> Result<RootfsMode, String> {
    match annotations.get(ROOTFS_ANNOTATION) {
        None => Ok(default),
        Some(v) if v == "host" => Ok(RootfsMode::Host),
        Some(v) if v == "image" => Ok(RootfsMode::Image),
        Some(v) => Err(format!(
            "invalid {ROOTFS_ANNOTATION} annotation {v:?}: expected host|image"
        )),
    }
}

/// A name usable as a staged rootfs (Q55): one plain path component, no
/// dotfiles, no CAS control name — `../escape` style references and CAS
/// shadowing are structurally rejected.
fn staged_name_ok(name: &str) -> bool {
    !name.is_empty()
        && !name.contains('/')
        && name != "."
        && name != ".."
        && !name.starts_with('.')
        && !CAS_CONTROL_DIRS.contains(&name)
}

/// The staged directory behind a non-absolute image name, if it exists
/// (Q55): `<data_dir>/engine/images/<name>` first, the T4.12 legacy
/// `<data_dir>/images/<name>` second. Absolute spellings and unsafe names
/// answer `None` (the caller treats them as not staged).
pub fn staged_dir(images_root: &Path, legacy_images: &Path, name: &str) -> Option<PathBuf> {
    if Path::new(name).is_absolute() || !staged_name_ok(name) {
        return None;
    }
    [images_root.join(name), legacy_images.join(name)]
        .into_iter()
        .find(|p| p.is_dir())
}

/// Staged rootfs names under `<data_dir>/engine/images` (Q55) — the new
/// staging face surfaced through `list_image_tags` so IfNotPresent
/// short-circuits without an EnsureImage round.
pub fn staged_names(images_root: &Path) -> Vec<String> {
    let Ok(rd) = std::fs::read_dir(images_root) else {
        return Vec::new();
    };
    rd.flatten()
        .filter(|e| e.path().is_dir())
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| staged_name_ok(n))
        .collect()
}

/// The actionable missing-image error for host mode (Q55/R1): every
/// remedy on one line — stage a directory, use an absolute path, or flip
/// the pod to the Q53 pull chain via the annotation.
pub fn host_mode_missing_image(image: &str, images_root: &Path) -> String {
    format!(
        "builtin engine (host rootfs, Q55): no root directory for image {image:?} — \
         stage one at {staged}/<name>, use an absolute image path, or annotate \
         the pod '{ROOTFS_ANNOTATION}: image' to use the registry pull chain",
        staged = images_root.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ann(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn parse_accepts_both_modes_and_rejects_the_rest() {
        assert_eq!(RootfsMode::parse("host"), Ok(RootfsMode::Host));
        assert_eq!(RootfsMode::parse("image"), Ok(RootfsMode::Image));
        assert!(RootfsMode::parse("Host").is_err());
        assert!(RootfsMode::parse("").is_err());
    }

    #[test]
    fn annotation_overrides_in_both_directions_and_errors_on_garbage() {
        let none = ann(&[]);
        assert_eq!(resolve_mode(RootfsMode::Host, &none), Ok(RootfsMode::Host));
        assert_eq!(
            resolve_mode(RootfsMode::Image, &none),
            Ok(RootfsMode::Image)
        );
        let img = ann(&[(ROOTFS_ANNOTATION, "image")]);
        assert_eq!(resolve_mode(RootfsMode::Host, &img), Ok(RootfsMode::Image));
        let host = ann(&[(ROOTFS_ANNOTATION, "host")]);
        assert_eq!(resolve_mode(RootfsMode::Image, &host), Ok(RootfsMode::Host));
        let bad = ann(&[(ROOTFS_ANNOTATION, "oci")]);
        assert!(resolve_mode(RootfsMode::Host, &bad).is_err());
    }

    #[test]
    fn staged_lookup_guards_cas_names_and_traversal() {
        let tmp = std::env::temp_dir().join(format!("init-pro-rootfs-{}", std::process::id()));
        let root = tmp.join("engine/images");
        let legacy = tmp.join("images");
        std::fs::create_dir_all(root.join("app")).unwrap();
        std::fs::create_dir_all(legacy.join("old")).unwrap();
        std::fs::create_dir_all(root.join("blobs")).unwrap(); // CAS control

        assert_eq!(staged_dir(&root, &legacy, "app"), Some(root.join("app")));
        assert_eq!(staged_dir(&root, &legacy, "old"), Some(legacy.join("old")));
        assert_eq!(staged_dir(&root, &legacy, "missing"), None);
        assert_eq!(
            staged_dir(&root, &legacy, "blobs"),
            None,
            "CAS control name"
        );
        assert_eq!(staged_dir(&root, &legacy, "../etc"), None);
        assert_eq!(staged_dir(&root, &legacy, "/abs"), None);
        let names = staged_names(&root);
        assert_eq!(names, vec!["app".to_string()]);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn missing_image_error_lists_every_remedy() {
        let msg = host_mode_missing_image("web", Path::new("/dd/engine/images"));
        assert!(msg.contains("web"));
        assert!(msg.contains("/dd/engine/images"));
        assert!(msg.contains(ROOTFS_ANNOTATION));
    }
}
