//! unpack.rs tests (T4.13 M3, Q53): traversal guards, whiteout
//! semantics for both the kernel-overlay and fused-copy paths.
use std::io::Write as _;

use super::*;

const GZ: &str = "application/vnd.docker.image.rootfs.diff.tar.gzip";

fn temp_dir(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "init-pro-image-unpack-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&root).unwrap();
    root
}

/// Deterministic gzip layer over `name -> contents` regular files.
fn gz_blob(files: &[(&str, &str)]) -> PathBuf {
    gz_bytes(
        files
            .iter()
            .map(|(n, c)| (*n, c.to_string()))
            .collect::<Vec<_>>()
            .as_slice(),
    )
}

fn gz_bytes(files: &[(&str, String)]) -> PathBuf {
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    let mut c = std::io::Cursor::new(Vec::new());
    {
        let mut b = tar::Builder::new(&mut c);
        for (name, content) in files {
            let mut h = tar::Header::new_gnu();
            h.set_size(content.len() as u64);
            h.set_mode(0o644);
            h.set_cksum();
            b.append_data(&mut h, name, std::io::Cursor::new(content.as_bytes()))
                .unwrap();
        }
        b.finish().unwrap();
    }
    enc.write_all(c.get_ref()).unwrap();
    let blob = temp_dir("blob").join("layer.tar.gz");
    std::fs::write(&blob, enc.finish().unwrap()).unwrap();
    blob
}

/// A gzip blob wrapping RAW tar bytes (no tar-rs setters involved).
fn gz_raw(tar: Vec<u8>) -> PathBuf {
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    enc.write_all(&tar).unwrap();
    let blob = temp_dir("blob").join("layer.tar.gz");
    std::fs::write(&blob, enc.finish().unwrap()).unwrap();
    blob
}

/// A hand-rolled tar entry storing `name` VERBATIM: tar-rs' setters
/// refuse `..` paths, but a hostile archive bypasses them — so must
/// the traversal test (Q53: contents are never trusted).
fn hostile_entry(name: &str, content: &str) -> Vec<u8> {
    let mut h = vec![0u8; 512];
    h[..name.len()].copy_from_slice(name.as_bytes());
    h[100..108].copy_from_slice(b"0000644\0");
    h[108..116].copy_from_slice(b"0000000\0");
    h[116..124].copy_from_slice(b"0000000\0");
    h[124..136].copy_from_slice(format!("{:011o}\0", content.len()).as_bytes());
    h[136..148].copy_from_slice(b"00000000000\0");
    h[156] = b'0'; // regular file
    h[148..156].copy_from_slice(b"        ");
    let sum: u32 = h.iter().map(|&b| u32::from(b)).sum();
    h[148..156].copy_from_slice(format!("{:06o}\0 ", sum).as_bytes());
    h.extend_from_slice(content.as_bytes());
    h.resize(h.len() + (512 - content.len() % 512) % 512, 0);
    h
}

#[test]
fn rejects_traversal_entries() {
    let root = temp_dir("traversal");
    let blob = gz_raw(hostile_entry("../evil", "x"));
    let err = unpack_layer(&blob, &root, GZ).unwrap_err();
    assert!(err.contains("layer entry escapes root"), "{err}");
    assert!(!root.join("evil").exists());
    assert!(!root.parent().unwrap().join("evil").exists());

    // An absolute path is equally refused (tar stores it verbatim).
    let blob = gz_raw(hostile_entry("/etc/passwd", "x"));
    let err = unpack_layer(&blob, &root.join("dest"), GZ).unwrap_err();
    assert!(err.contains("layer entry escapes root"), "{err}");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn fused_whiteouts_delete_and_opaque_clear() {
    let root = temp_dir("fused");
    let l1 = gz_blob(&[("a.txt", "l1"), ("del/me.txt", "x"), ("sub/old.txt", "o")]);
    let l2 = gz_blob(&[
        ("b.txt", "l2"),
        ("del/.wh.me.txt", ""),
        ("sub/.wh..wh..opq", ""),
        ("sub/new.txt", "n"),
    ]);
    let dest = root.join("merged");
    fused_layers(&[l1, l2], &[GZ.to_string(), GZ.to_string()], &dest).unwrap();
    assert_eq!(std::fs::read_to_string(dest.join("a.txt")).unwrap(), "l1");
    assert_eq!(std::fs::read_to_string(dest.join("b.txt")).unwrap(), "l2");
    assert!(!dest.join("del/me.txt").exists(), "whiteout must delete");
    assert!(!dest.join("sub/old.txt").exists(), "opaque must clear sub/");
    assert_eq!(
        std::fs::read_to_string(dest.join("sub/new.txt")).unwrap(),
        "n"
    );
    assert_eq!(
        list_children(&dest.join("sub")).unwrap(),
        vec!["new.txt".to_string()],
        "sub/ must hold ONLY new.txt"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn kernel_whiteout_translation_when_root() {
    if !nix::unistd::Uid::effective().is_root() {
        return; // mknod + trusted xattrs need root (Q53)
    }
    let root = temp_dir("kernel");
    let layer = gz_blob(&[("keep.txt", "k"), ("gone/.wh.txt", "")]);
    let dest = root.join("layer");
    unpack_layer(&layer, &dest, GZ).unwrap();
    use std::os::unix::fs::FileTypeExt as _;
    assert!(
        std::fs::symlink_metadata(dest.join("gone/txt"))
            .unwrap()
            .file_type()
            .is_char_device(),
        "whiteout must be a char 0:0 node"
    );
    assert!(!dest.join("gone/.wh.txt").exists(), "no marker file");

    let opq = gz_blob(&[("dir/.wh..wh..opq", "")]);
    let dest2 = root.join("layer2");
    unpack_layer(&opq, &dest2, GZ).unwrap();
    assert_eq!(
        xattr::get(dest2.join("dir"), "trusted.overlay.opaque")
            .unwrap()
            .as_deref(),
        Some(b"y".as_ref())
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// A hand-rolled SYMLINK entry (`typeflag 2`): the target lands VERBATIM
/// in the `linkname` field, exactly as a hostile archive would spell it.
fn symlink_entry(name: &str, target: &str) -> Vec<u8> {
    let mut h = vec![0u8; 512];
    h[..name.len()].copy_from_slice(name.as_bytes());
    h[100..108].copy_from_slice(b"0000777\0");
    h[108..116].copy_from_slice(b"0000000\0");
    h[116..124].copy_from_slice(b"0000000\0");
    h[124..136].copy_from_slice(b"00000000000\0");
    h[136..148].copy_from_slice(b"00000000000\0");
    h[156] = b'2'; // symlink
    h[157..157 + target.len()].copy_from_slice(target.as_bytes());
    h[148..156].copy_from_slice(b"        ");
    let sum: u32 = h.iter().map(|&b| u32::from(b)).sum();
    h[148..156].copy_from_slice(format!("{:06o}\0 ", sum).as_bytes());
    h
}

/// `entries` + the two zero blocks that terminate a tar stream.
fn tar_of(entries: &[Vec<u8>]) -> Vec<u8> {
    let mut v = Vec::new();
    for e in entries {
        v.extend_from_slice(e);
    }
    v.extend_from_slice(&[0u8; 1024]);
    v
}

#[test]
fn rejects_deep_traversal_and_dotted_whiteout_names() {
    // (a) `../../../etc/passwd` -- three levels up, still refused.
    let root = temp_dir("deep-traversal");
    let blob = gz_raw(hostile_entry("../../../etc/passwd", "pwned"));
    let err = unpack_layer(&blob, &root, GZ).unwrap_err();
    assert!(err.contains("layer entry escapes root"), "{err}");
    // `.wh...` names `..`: a whiteout must never delete above the root.
    let blob = gz_raw(tar_of(&[hostile_entry(".wh...", "")]));
    let err = unpack_layer(&blob, &root.join("wo"), GZ).unwrap_err();
    assert!(err.contains("layer entry escapes root"), "{err}");
    assert!(root.is_dir(), "the layer root survives a hostile whiteout");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn rejects_absolute_symlink_target() {
    // (b) `evil -> /etc/passwd`: the link itself is the escape hatch.
    let root = temp_dir("abs-symlink");
    let blob = gz_raw(tar_of(&[symlink_entry("evil", "/etc/passwd")]));
    let dest = root.join("layer");
    let err = unpack_layer(&blob, &dest, GZ).unwrap_err();
    assert!(err.contains("layer entry escapes root"), "{err}");
    assert!(err.contains("/etc/passwd"), "target named: {err}");
    assert!(
        std::fs::symlink_metadata(dest.join("evil")).is_err(),
        "the link must never land"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn rejects_write_through_a_planted_symlink() {
    // (b, fused path) layer 1 plants a link to a dir OUTSIDE the layer
    // root, layer 2 writes through it.
    let root = temp_dir("planted");
    let outside = root.join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    let l1 = gz_raw(tar_of(&[symlink_entry("evil", outside.to_str().unwrap())]));
    let l2 = gz_blob(&[("evil/passwd", "pwned")]);
    let dest = root.join("merged");
    let err = fused_layers(&[l1, l2], &[GZ.to_string(), GZ.to_string()], &dest).unwrap_err();
    assert!(err.contains("layer entry escapes root"), "{err}");
    assert!(
        std::fs::read_dir(&outside).unwrap().count() == 0,
        "nothing may be written through the planted link"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn relative_symlink_inside_the_layer_still_extracts() {
    // (c) Legit images: a relative link pointing INSIDE the layer is
    // created verbatim, and a later layer may even write through it.
    let root = temp_dir("legit-symlink");
    let l1 = gz_raw(tar_of(&[
        hostile_entry("real/.keep", ""),
        symlink_entry("sub/link", "../real"),
        symlink_entry("libfoo.so", "libfoo.so.1"),
    ]));
    let dest = root.join("layer");
    unpack_layer(&l1, &dest, GZ).unwrap();
    let link = std::fs::read_link(dest.join("libfoo.so")).unwrap();
    assert_eq!(link, PathBuf::from("libfoo.so.1"), "relative target kept");

    let l2 = gz_blob(&[("sub/link/f.txt", "through-link")]);
    fused_layers(&[l1.clone(), l2], &[GZ.to_string(), GZ.to_string()], &dest).unwrap();
    assert_eq!(
        std::fs::read_to_string(dest.join("real/f.txt")).unwrap(),
        "through-link",
        "a link that resolves inside the root stays usable"
    );
    let _ = std::fs::remove_dir_all(&root);
}
