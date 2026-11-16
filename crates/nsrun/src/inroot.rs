//! Path lookup scoped to the container root, as runc's securejoin does.
//!
//! Adapted from kiri `crates/kiri-init/src/linux/inroot.rs` @ 7846fe0
//! (Apache-2.0). The child mounts into the container before `pivot_root`,
//! while its root is still the host's. A plain walk would follow an image's
//! absolute symlink out of the container, so every target is resolved one
//! component at a time with O_NOFOLLOW, absolute links restart at the
//! container root, and `..` stops there. The result is an O_PATH descriptor
//! used through `/proc/self/fd/N`.

use std::collections::VecDeque;
use std::ffi::{CString, OsStr, OsString};
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Component, Path};

const MAX_LINKS: u32 = 40;

/// What to make when the last component is missing.
// `Exists`/`Node` are used by the P2 image/device staging paths.
#[allow(dead_code)]
#[derive(Clone, Copy)]
pub enum Last {
    Exists,
    Dir,
    File,
    Node(libc::mode_t, libc::dev_t),
}

/// A target found (or made) inside the root.
pub struct Found {
    pub fd: OwnedFd,
    /// True when the final component was created by this call.
    #[allow(dead_code)] // read by the P2 image staging paths
    pub created: bool,
}

impl Found {
    /// `/proc/self/fd/N`: resolves to exactly this object for mount(2).
    pub fn proc_path(&self) -> CString {
        CString::new(format!("/proc/self/fd/{}", self.fd.as_raw_fd())).expect("no NUL")
    }
}

pub fn open_root(root: &Path) -> io::Result<OwnedFd> {
    let c = super::sys::cstr(root)?;
    // SAFETY: valid NUL-terminated path; the descriptor is owned right away.
    let fd = unsafe {
        libc::open(
            c.as_ptr(),
            libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fresh descriptor nothing else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

pub fn open(root: BorrowedFd<'_>, path: &Path, last: Last) -> io::Result<Found> {
    let mut todo: VecDeque<OsString> = components(path.as_os_str());
    let mut dirs: Vec<OwnedFd> = vec![root.try_clone_to_owned()?];
    let mut links = 0;
    let mut created = false;
    while let Some(name) = todo.pop_front() {
        if name == ".." {
            if dirs.len() > 1 {
                dirs.pop();
            }
            continue;
        }
        let is_last = todo.is_empty();
        let dir = dirs.last().expect("root stays").as_fd();
        let fd = match open_nofollow(dir, &name) {
            Ok(fd) => fd,
            Err(e) if e.raw_os_error() == Some(libc::ENOENT) => {
                let kind = if is_last { last } else { Last::Dir };
                make(dir, &name, kind).map_err(|e| with_path(e, path))?;
                created = is_last;
                open_nofollow(dir, &name).map_err(|e| with_path(e, path))?
            }
            Err(e) => return Err(with_path(e, path)),
        };
        let st = fstat(fd.as_fd())?;
        match st.st_mode & libc::S_IFMT {
            libc::S_IFLNK => {
                links += 1;
                if links > MAX_LINKS {
                    return Err(with_path(io::Error::from_raw_os_error(libc::ELOOP), path));
                }
                let target = readlink(fd.as_fd())?;
                if target.as_bytes().first() == Some(&b'/') {
                    dirs.truncate(1);
                }
                for c in components(&target).into_iter().rev() {
                    todo.push_front(c);
                }
                created = false;
            }
            libc::S_IFDIR if !is_last => dirs.push(fd),
            _ if is_last => return Ok(Found { fd, created }),
            _ => return Err(with_path(io::Error::from_raw_os_error(libc::ENOTDIR), path)),
        }
    }
    let fd = dirs.pop().expect("root stays");
    Ok(Found { fd, created })
}

fn components(path: &OsStr) -> VecDeque<OsString> {
    Path::new(path)
        .components()
        .filter_map(|c| match c {
            Component::Normal(n) => Some(n.to_owned()),
            Component::ParentDir => Some(OsString::from("..")),
            Component::RootDir | Component::CurDir | Component::Prefix(_) => None,
        })
        .collect()
}

fn with_path(e: io::Error, path: &Path) -> io::Error {
    io::Error::new(e.kind(), format!("{}: {e}", path.display()))
}

fn name_c(name: &OsStr) -> io::Result<CString> {
    CString::new(name.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))
}

fn open_nofollow(dir: BorrowedFd<'_>, name: &OsStr) -> io::Result<OwnedFd> {
    let c = name_c(name)?;
    // SAFETY: valid directory fd and NUL-terminated name; owned right away.
    let fd = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            c.as_ptr(),
            libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fresh descriptor nothing else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn make(dir: BorrowedFd<'_>, name: &OsStr, kind: Last) -> io::Result<()> {
    let c = name_c(name)?;
    let d = dir.as_raw_fd();
    // SAFETY: valid directory fd, NUL-terminated name, and a plain mode/dev.
    let ret = unsafe {
        match kind {
            Last::Exists => return Err(io::Error::from_raw_os_error(libc::ENOENT)),
            Last::Dir => libc::mkdirat(d, c.as_ptr(), 0o755),
            Last::File => {
                let fd = libc::openat(
                    d,
                    c.as_ptr(),
                    libc::O_WRONLY
                        | libc::O_CREAT
                        | libc::O_EXCL
                        | libc::O_NOFOLLOW
                        | libc::O_CLOEXEC,
                    0o644,
                );
                if fd >= 0 {
                    libc::close(fd);
                }
                fd
            }
            Last::Node(mode, dev) => libc::mknodat(d, c.as_ptr(), mode, dev),
        }
    };
    if ret < 0 {
        let e = io::Error::last_os_error();
        if e.raw_os_error() != Some(libc::EEXIST) {
            return Err(e);
        }
    }
    Ok(())
}

fn fstat(fd: BorrowedFd<'_>) -> io::Result<libc::stat> {
    // SAFETY: stat is plain data; fstat fills it from a valid descriptor.
    unsafe {
        let mut st: libc::stat = std::mem::zeroed();
        if libc::fstat(fd.as_raw_fd(), &mut st) < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(st)
    }
}

fn readlink(fd: BorrowedFd<'_>) -> io::Result<OsString> {
    let mut buf = vec![0u8; libc::PATH_MAX as usize];
    // SAFETY: an empty name reads the link `fd` itself; `buf` is writable.
    let n = unsafe {
        libc::readlinkat(
            fd.as_raw_fd(),
            c"".as_ptr(),
            buf.as_mut_ptr().cast(),
            buf.len(),
        )
    };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    buf.truncate(n as usize);
    Ok(OsString::from_vec(buf))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::symlink;
    use std::path::PathBuf;

    struct Tmp(PathBuf);

    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn setup(tag: &str) -> (Tmp, PathBuf, PathBuf) {
        let base = std::env::temp_dir().join(format!(
            "nsrun-inroot-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let root = base.join("root");
        let outside = base.join("outside");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&outside).unwrap();
        (Tmp(base), root, outside)
    }

    fn rel(root: &Path, f: &Found) -> PathBuf {
        let p = fs::read_link(format!("/proc/self/fd/{}", f.fd.as_raw_fd())).unwrap();
        p.strip_prefix(root).unwrap_or(&p).to_path_buf()
    }

    #[test]
    fn absolute_symlink_stays_in_root() {
        let (_t, root, _o) = setup("abs");
        fs::create_dir_all(root.join("etc")).unwrap();
        fs::write(root.join("etc/passwd"), "root").unwrap();
        symlink("/etc/passwd", root.join("link")).unwrap();
        let r = open_root(&root).unwrap();
        let f = open(r.as_fd(), Path::new("/link"), Last::Exists).unwrap();
        assert_eq!(rel(&root, &f), Path::new("etc/passwd"));
    }

    #[test]
    fn dotdot_stops_at_root() {
        let (_t, root, outside) = setup("dotdot");
        fs::write(outside.join("secret"), "x").unwrap();
        let r = open_root(&root).unwrap();
        let f = open(r.as_fd(), Path::new("../../.."), Last::Dir).unwrap();
        assert_eq!(rel(&root, &f), Path::new(""));
    }

    #[test]
    fn creates_missing_file_inside_root() {
        let (_t, root, _o) = setup("create");
        let r = open_root(&root).unwrap();
        let f = open(r.as_fd(), Path::new("/etc/hosts"), Last::File).unwrap();
        assert!(f.created);
        assert_eq!(rel(&root, &f), Path::new("etc/hosts"));
    }

    #[test]
    fn symlink_loop_is_an_error() {
        let (_t, root, _o) = setup("loop");
        symlink("/b", root.join("a")).unwrap();
        symlink("/a", root.join("b")).unwrap();
        let r = open_root(&root).unwrap();
        let e = open(r.as_fd(), Path::new("/a/x"), Last::Dir).err().unwrap();
        assert!(
            e.to_string().contains(&format!("os error {}", libc::ELOOP)),
            "{e}"
        );
    }
}
