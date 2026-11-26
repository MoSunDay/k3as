//! T4.13 image store (Q53/Q54, M3): layer unpack with REAL whiteout
//! semantics. Layers unpack once into shared per-layer dirs, consumed as
//! per-container overlayfs views (kernel char-device whiteouts +
//! `trusted.overlay.opaque` xattrs, containerd-style) or the fused-copy
//! fallback (logical whiteout application, pure fs ops). Image contents
//! are never trusted: every entry is traversal-guarded first -- lexically
//! (no absolute paths, no `..`) AND on disk (no write may travel through a
//! symlink that leaves the layer root, see [`guard_ancestors`]).

use std::fs::Permissions;
use std::io::Read;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Component, Path, PathBuf};

/// How `.wh.` markers in a layer are applied (Q53 M3).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum WhiteoutMode {
    /// containerd-style: `.wh.<name>` becomes a kernel overlayfs whiteout
    /// (char device 0:0, honored in any lowerdir), `.wh..wh..opq` becomes
    /// `trusted.overlay.opaque="y"` on the parent. mknod/setxattr need
    /// root: non-root callers get a clear Err.
    Kernel,
    /// Logical merge: `.wh.<name>` deletes `<name>` from dest, opaque
    /// markers clear the dir's contents. Pure fs ops, no privileges.
    Logical,
}

/// Layer payload encoding, decided by the descriptor media type.
pub(crate) enum Compression {
    Gzip,
    Zstd,
    Raw,
}

/// Map a docker/OCI layer media type to its decompression.
pub(crate) fn decompress_stream(media_type: &str) -> Result<Compression, String> {
    match media_type {
        "application/vnd.docker.image.rootfs.diff.tar.gzip"
        | "application/vnd.oci.image.layer.v1.tar+gzip"
        | "application/vnd.oci.image.layer.nondistributable.v1.tar+gzip" => Ok(Compression::Gzip),
        "application/vnd.oci.image.layer.v1.tar+zstd"
        | "application/vnd.oci.image.layer.nondistributable.v1.tar+zstd" => Ok(Compression::Zstd),
        "application/vnd.docker.image.rootfs.diff.tar"
        | "application/vnd.oci.image.layer.v1.tar"
        | "application/vnd.oci.image.layer.nondistributable.v1.tar" => Ok(Compression::Raw),
        other => Err(format!("unsupported layer mediaType {other}")),
    }
}

/// Unpack one CAS blob into its OWN per-layer dir `dest` with kernel
/// whiteout translation ([`WhiteoutMode::Kernel`]). The dir is then
/// immutable and shared by every container using the layer (Q53).
pub(crate) fn unpack_layer(blob: &Path, dest: &Path, media_type: &str) -> Result<(), String> {
    apply_archive(
        blob,
        dest,
        decompress_stream(media_type)?,
        WhiteoutMode::Kernel,
    )
}

/// Logically fuse `blobs` (manifest order, bottom first) into ONE `dest`:
/// later layers overwrite earlier files, whiteouts applied logically. The
/// fallback when overlayfs is unavailable (Q53).
pub(crate) fn fused_layers(
    blobs: &[PathBuf],
    media_types: &[String],
    dest: &Path,
) -> Result<(), String> {
    if blobs.len() != media_types.len() {
        return Err(format!(
            "fused_layers: {} blobs but {} media types",
            blobs.len(),
            media_types.len()
        ));
    }
    for (blob, media_type) in blobs.iter().zip(media_types) {
        apply_archive(
            blob,
            dest,
            decompress_stream(media_type)?,
            WhiteoutMode::Logical,
        )?;
    }
    Ok(())
}

/// Open + decode one blob, run the guarded entry loop over dest.
fn apply_archive(
    blob: &Path,
    dest: &Path,
    comp: Compression,
    mode: WhiteoutMode,
) -> Result<(), String> {
    // `dest` must exist: the on-disk symlink guard resolves the layer root
    // canonically, and a root that does not exist yet cannot be resolved.
    std::fs::create_dir_all(dest).map_err(|e| format!("create layer dir: {e}"))?;
    let file = std::fs::File::open(blob).map_err(|e| format!("open layer: {e}"))?;
    let mut arch: tar::Archive<Box<dyn Read>> = match comp {
        Compression::Gzip => tar::Archive::new(Box::new(flate2::read::GzDecoder::new(file))),
        Compression::Zstd => tar::Archive::new(Box::new(
            zstd::stream::read::Decoder::new(file).map_err(|e| format!("zstd layer: {e}"))?,
        )),
        Compression::Raw => tar::Archive::new(Box::new(file)),
    };
    let entries = arch.entries().map_err(|e| format!("read layer: {e}"))?;
    for entry in entries {
        let mut entry = entry.map_err(|e| format!("read layer entry: {e}"))?;
        let name = entry
            .path()
            .map_err(|e| format!("entry path: {e}"))?
            .into_owned();
        apply_entry(dest, &name, &mut entry, mode)
            .map_err(|e| format!("entry {}: {e}", name.display()))?;
    }
    Ok(())
}

/// `dest.join(name)` after rejecting traversal: the entry must be a
/// relative path with no `..` component, the join must stay under `dest`,
/// and no already-existing component of the joined path may be a symlink
/// pointing out of the layer root. Image bytes are untrusted (Q54
/// consequence).
fn safe_join(dest: &Path, name: &Path) -> Result<PathBuf, String> {
    let raw = name.to_string_lossy();
    let escapes = raw.is_empty()
        || name.is_absolute()
        || name.components().any(|c| {
            matches!(
                c,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        });
    let joined = dest.join(name);
    if escapes || !joined.starts_with(dest) {
        return Err(format!("layer entry escapes root: {raw}"));
    }
    guard_ancestors(dest, &joined)?;
    Ok(joined)
}

/// Canonical layer root, or the path itself when it does not exist (the
/// callers create `dest` first, so the fallback is a belt-and-braces case).
fn canonical_root(dest: &Path) -> PathBuf {
    std::fs::canonicalize(dest).unwrap_or_else(|_| dest.to_path_buf())
}

/// On-disk half of the traversal guard (C1, Q54): every ALREADY-existing
/// component between `dest` and the leaf is inspected with
/// `std::fs::symlink_metadata` -- never followed. A symlink is tolerated
/// only while it resolves back INSIDE the layer root: an earlier layer (or
/// an earlier entry of the same archive) can plant `evil -> /etc`, and a
/// later `evil/passwd` write would then follow it and overwrite an
/// arbitrary host file. A DANGLING link is tolerated (nothing can be
/// written through a target that does not exist, and the entry arms that
/// land on a leaf link remove it first). Legitimate images keep working:
/// relative links that stay inside the layer, hardlinks and whiteouts are
/// unaffected.
fn guard_ancestors(dest: &Path, joined: &Path) -> Result<(), String> {
    let Ok(rel) = joined.strip_prefix(dest) else {
        return Err(format!(
            "layer entry escapes root: {} is outside {}",
            joined.display(),
            dest.display()
        ));
    };
    let root = canonical_root(dest);
    let mut cur = dest.to_path_buf();
    for comp in rel.components() {
        cur.push(comp);
        let meta = match std::fs::symlink_metadata(&cur) {
            // Nothing on this level: no deeper component can exist either.
            Err(_) => return Ok(()),
            Ok(m) => m,
        };
        if !meta.file_type().is_symlink() {
            continue;
        }
        // A dangling (or unresolvable) link has no target to escape to:
        // every write below it fails on its own.
        let Ok(real) = std::fs::canonicalize(&cur) else {
            continue;
        };
        if !real.starts_with(&root) {
            return Err(format!(
                "layer entry escapes root: {} is a symlink to {}",
                cur.display(),
                real.display()
            ));
        }
    }
    Ok(())
}

/// The entry's dir inside dest, treating a root-level entry as dest.
fn entry_dir(dest: &Path, name: &Path) -> Result<PathBuf, String> {
    match name.parent().filter(|p| !p.as_os_str().is_empty()) {
        Some(p) => safe_join(dest, p),
        None => Ok(dest.to_path_buf()),
    }
}

/// Handle one tar entry. Whiteout markers never appear in dest as files.
fn apply_entry<R: Read>(
    dest: &Path,
    name: &Path,
    entry: &mut tar::Entry<'_, R>,
    mode: WhiteoutMode,
) -> Result<(), String> {
    let base = name
        .file_name()
        .map(|b| b.to_string_lossy().into_owned())
        .unwrap_or_default();
    if base == ".wh..wh..opq" {
        return apply_opaque(&entry_dir(dest, name)?, mode);
    }
    if let Some(hidden) = base.strip_prefix(".wh.").filter(|h| !h.is_empty()) {
        // The hidden name is a LEAF: `.wh...` would otherwise name `..` and
        // turn a whiteout into a delete of the layer root's parent (C1).
        if hidden == "." || hidden == ".." || hidden.contains('/') {
            return Err(format!(
                "layer entry escapes root: whiteout {} names {}",
                name.display(),
                hidden
            ));
        }
        let target = entry_dir(dest, name)?.join(hidden);
        return apply_whiteout(&target, mode);
    }
    write_entry(dest, name, entry, mode)
}

/// Opaque dir marker: hide everything below this dir in lower layers.
fn apply_opaque(dir: &Path, mode: WhiteoutMode) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("opaque dir: {e}"))?;
    match mode {
        WhiteoutMode::Kernel => {
            if !nix::unistd::Uid::effective().is_root() {
                return Err("whiteout needs root (opaque xattr)".to_string());
            }
            xattr::set(dir, "trusted.overlay.opaque", b"y")
                .map_err(|e| format!("opaque xattr {}: {e}", dir.display()))
        }
        // Keep the dir itself, drop everything inside it.
        WhiteoutMode::Logical => {
            for child in list_children(dir)? {
                remove_any(&dir.join(child))?;
            }
            Ok(())
        }
    }
}

/// `.wh.<name>`: the named path must vanish from this layer's view.
fn apply_whiteout(target: &Path, mode: WhiteoutMode) -> Result<(), String> {
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("whiteout dir: {e}"))?;
    }
    remove_any(target)?;
    match mode {
        WhiteoutMode::Kernel => mknod_whiteout(target),
        WhiteoutMode::Logical => Ok(()), // deletion above IS the whiteout
    }
}

/// Create a kernel overlayfs whiteout: char device 0:0 (root-only).
fn mknod_whiteout(target: &Path) -> Result<(), String> {
    if !nix::unistd::Uid::effective().is_root() {
        return Err("whiteout needs root (mknod)".to_string());
    }
    nix::sys::stat::mknod(
        target,
        nix::sys::stat::SFlag::S_IFCHR,
        nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
        0,
    )
    .map_err(|e| format!("whiteout mknod {}: {e}", target.display()))
}

/// Plain entries: dirs/files/symlinks/hardlinks. Modes are preserved
/// (executable bits), mtime and packed xattrs are not (Q53).
fn write_entry<R: Read>(
    dest: &Path,
    name: &Path,
    entry: &mut tar::Entry<'_, R>,
    mode: WhiteoutMode,
) -> Result<(), String> {
    let target = safe_join(dest, name)?;
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir: {e}"))?;
    }
    let header = entry.header();
    let perm = header.mode().unwrap_or(0o644) & 0o7777;
    match header.entry_type() {
        tar::EntryType::Directory => {
            std::fs::create_dir_all(&target).map_err(|e| format!("mkdir: {e}"))?;
            let _ = std::fs::set_permissions(&target, Permissions::from_mode(perm));
            Ok(())
        }
        tar::EntryType::Regular => {
            replace_any(&target)?;
            let mut out = std::fs::File::create(&target).map_err(|e| format!("create: {e}"))?;
            std::io::copy(entry, &mut out).map_err(|e| format!("write: {e}"))?;
            let _ = std::fs::set_permissions(&target, Permissions::from_mode(perm));
            Ok(())
        }
        tar::EntryType::Symlink | tar::EntryType::Link => {
            let link = entry
                .link_name()
                .ok()
                .flatten()
                .unwrap_or_default()
                .into_owned();
            if header.entry_type() == tar::EntryType::Symlink && link.is_absolute() {
                // An absolute link target leaves the layer root by
                // construction (C1): the link itself is the escape hatch a
                // later layer would write through.
                return Err(format!(
                    "layer entry escapes root: symlink {} -> {}",
                    target.display(),
                    link.display()
                ));
            }
            replace_any(&target)?;
            if header.entry_type() == tar::EntryType::Link {
                let source = safe_join(dest, &link)?;
                std::fs::hard_link(&source, &target)
                    .map_err(|e| format!("hardlink {}: {e}", target.display()))
            } else {
                std::os::unix::fs::symlink(&link, &target)
                    .map_err(|e| format!("symlink {}: {e}", target.display()))
            }
        }
        // Char/block devices: a 0:0 char node IS a kernel whiteout (kept
        // in Kernel mode); anything else an image ships is skipped.
        tar::EntryType::Char | tar::EntryType::Block => {
            let zeroed = matches!(header.device_major(), Ok(Some(0)))
                && matches!(header.device_minor(), Ok(Some(0)));
            if mode == WhiteoutMode::Kernel && header.entry_type() == tar::EntryType::Char && zeroed
            {
                return mknod_whiteout(&target);
            }
            Ok(())
        }
        _ => Ok(()), // fifos/sockets/etc: images should not ship them
    }
}

fn list_children(dir: &Path) -> Result<Vec<String>, String> {
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .filter_map(|e| e.file_name().into_string().ok())
                .collect()
        })
        .map_err(|e| format!("opaque read {}: {e}", dir.display()))
}

/// Remove a file OR a whole tree, whatever is there (whiteouts are
/// idempotent: a missing target is fine).
fn remove_any(path: &Path) -> Result<(), String> {
    match std::fs::symlink_metadata(path) {
        Err(_) => Ok(()),
        Ok(m) if m.is_dir() => {
            std::fs::remove_dir_all(path).map_err(|e| format!("rm {}: {e}", path.display()))
        }
        Ok(_) => {
            let _ = std::fs::remove_file(path);
            Ok(())
        }
    }
}

/// A path may be re-created by a later layer: clear whatever sits there.
fn replace_any(path: &Path) -> Result<(), String> {
    match std::fs::symlink_metadata(path) {
        Err(_) => Ok(()),
        Ok(m) if m.is_dir() => {
            std::fs::remove_dir_all(path).map_err(|e| format!("rm {}: {e}", path.display()))
        }
        Ok(_) => std::fs::remove_file(path).map_err(|e| format!("rm {}: {e}", path.display())),
    }
}

#[cfg(test)]
#[cfg(test)]
mod tests;
