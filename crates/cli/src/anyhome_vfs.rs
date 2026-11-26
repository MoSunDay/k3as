//! FSMap: the directory-VFS core of the embedded workspace NFS plane
//! (TODO **T-anyhome** / **T7.4**, decisions **Q36** and **Q66**; the
//! exported `/workflow` / `/agent` / `/pod` tree contract is Q36 and
//! T7.5 / Q43).
//!
//! Adapted from nfsserve examples/mirrorfs.rs @ c00c361 (BSD-3-Clause,
//! see NOTICE). This module carries the FSMap core of that example plus
//! the `create_object` primitive (mkdir/create/symlink materialization):
//! the `FSEntry`/`RefreshResult`/`FSMap` machinery — intaglio `SymbolTable`
//! path interning, `AtomicU64` fileid allocation, the double-stat mtime
//! invalidation refresh (every lookup/readdir re-stats the entry and its
//! cached metadata; a type change expires the id), and `delete_entry`
//! subtree recycling. The algorithm is ported line-for-line; the upstream
//! demo harness (main/HOSTPORT/mount-hint) is dropped. The
//! `NFSFileSystem` wrapper that consumes this map lives in
//! [`crate::anyhome_dirfs`].
#![forbid(unsafe_code)]

use std::collections::{BTreeSet, HashMap};
use std::ffi::{OsStr, OsString};
use std::fs::{File, Metadata};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use intaglio::osstr::SymbolTable;
use intaglio::Symbol;
use nfsserve::fs_util::{exists_no_traverse, fattr3_differ, file_setattr, metadata_to_fattr3};
use nfsserve::nfs::{fattr3, fileid3, filename3, ftype3, nfspath3, nfsstat3, sattr3};
use tracing::debug;

#[derive(Debug, Clone)]
pub(crate) struct FSEntry {
    pub(crate) name: Vec<Symbol>,
    pub(crate) fsmeta: fattr3,
    /// metadata when building the children list
    pub(crate) children_meta: fattr3,
    pub(crate) children: Option<BTreeSet<fileid3>>,
}

#[derive(Debug)]
pub(crate) struct FSMap {
    pub(crate) root: PathBuf,
    pub(crate) next_fileid: AtomicU64,
    pub(crate) intern: SymbolTable,
    pub(crate) id_to_path: HashMap<fileid3, FSEntry>,
    pub(crate) path_to_id: HashMap<Vec<Symbol>, fileid3>,
}

pub(crate) enum RefreshResult {
    /// The fileid was deleted
    Delete,
    /// The fileid needs to be reloaded. mtime has been updated, caches
    /// need to be evicted.
    Reload,
    /// Nothing has changed
    Noop,
}

/// The kind of object [`FSMap::create_object`] materializes on the real fs.
pub(crate) enum CreateFSObject {
    Directory,
    File(sattr3),
    Exclusive,
    Symlink((sattr3, nfspath3)),
}

impl FSMap {
    pub(crate) fn new(root: PathBuf) -> FSMap {
        // create root entry
        let root_entry = FSEntry {
            name: Vec::new(),
            fsmeta: metadata_to_fattr3(1, &root.metadata().unwrap()),
            children_meta: metadata_to_fattr3(1, &root.metadata().unwrap()),
            children: None,
        };
        FSMap {
            root,
            next_fileid: AtomicU64::new(1),
            intern: SymbolTable::new(),
            id_to_path: HashMap::from([(0, root_entry)]),
            path_to_id: HashMap::from([(Vec::new(), 0)]),
        }
    }

    pub(crate) async fn sym_to_path(&self, symlist: &[Symbol]) -> PathBuf {
        let mut ret = self.root.clone();
        for i in symlist.iter() {
            ret.push(self.intern.get(*i).unwrap());
        }
        ret
    }

    pub(crate) async fn sym_to_fname(&self, symlist: &[Symbol]) -> OsString {
        if let Some(x) = symlist.last() {
            self.intern.get(*x).unwrap().into()
        } else {
            "".into()
        }
    }

    pub(crate) fn collect_all_children(&self, id: fileid3, ret: &mut Vec<fileid3>) {
        ret.push(id);
        if let Some(entry) = self.id_to_path.get(&id) {
            if let Some(ref ch) = entry.children {
                for i in ch.iter() {
                    self.collect_all_children(*i, ret);
                }
            }
        }
    }

    pub(crate) fn delete_entry(&mut self, id: fileid3) {
        let mut children = Vec::new();
        self.collect_all_children(id, &mut children);
        for i in children.iter() {
            if let Some(ent) = self.id_to_path.remove(i) {
                self.path_to_id.remove(&ent.name);
            }
        }
    }

    pub(crate) fn find_entry(&self, id: fileid3) -> Result<FSEntry, nfsstat3> {
        Ok(self
            .id_to_path
            .get(&id)
            .ok_or(nfsstat3::NFS3ERR_NOENT)?
            .clone())
    }

    pub(crate) fn find_entry_mut(&mut self, id: fileid3) -> Result<&mut FSEntry, nfsstat3> {
        self.id_to_path.get_mut(&id).ok_or(nfsstat3::NFS3ERR_NOENT)
    }

    pub(crate) async fn find_child(
        &self,
        id: fileid3,
        filename: &[u8],
    ) -> Result<fileid3, nfsstat3> {
        let mut name = self
            .id_to_path
            .get(&id)
            .ok_or(nfsstat3::NFS3ERR_NOENT)?
            .name
            .clone();
        name.push(
            self.intern
                .check_interned(OsStr::from_bytes(filename))
                .ok_or(nfsstat3::NFS3ERR_NOENT)?,
        );
        Ok(*self.path_to_id.get(&name).ok_or(nfsstat3::NFS3ERR_NOENT)?)
    }

    pub(crate) async fn refresh_entry(&mut self, id: fileid3) -> Result<RefreshResult, nfsstat3> {
        let entry = self
            .id_to_path
            .get(&id)
            .ok_or(nfsstat3::NFS3ERR_NOENT)?
            .clone();
        let path = self.sym_to_path(&entry.name).await;
        if !exists_no_traverse(&path) {
            self.delete_entry(id);
            debug!("Deleting entry A {:?}: {:?}. Ent: {:?}", id, path, entry);
            return Ok(RefreshResult::Delete);
        }

        let meta = tokio::fs::symlink_metadata(&path)
            .await
            .map_err(|_| nfsstat3::NFS3ERR_IO)?;
        let meta = metadata_to_fattr3(id, &meta);
        if !fattr3_differ(&meta, &entry.fsmeta) {
            return Ok(RefreshResult::Noop);
        }
        // If we get here we have modifications
        if entry.fsmeta.ftype as u32 != meta.ftype as u32 {
            // if the file type changed ex: file->dir or dir->file
            // really the entire file has been replaced.
            // we expire the entire id
            debug!(
                "File Type Mismatch FT {:?} : {:?} vs {:?}",
                id, entry.fsmeta.ftype, meta.ftype
            );
            debug!(
                "File Type Mismatch META {:?} : {:?} vs {:?}",
                id, entry.fsmeta, meta
            );
            self.delete_entry(id);
            debug!("Deleting entry B {:?}: {:?}. Ent: {:?}", id, path, entry);
            return Ok(RefreshResult::Delete);
        }
        // inplace modification.
        // update metadata
        self.id_to_path
            .get_mut(&id)
            .ok_or(nfsstat3::NFS3ERR_NOENT)?
            .fsmeta = meta;
        debug!("Reloading entry {:?}: {:?}. Ent: {:?}", id, path, entry);
        Ok(RefreshResult::Reload)
    }

    pub(crate) async fn refresh_dir_list(&mut self, id: fileid3) -> Result<(), nfsstat3> {
        let entry = self
            .id_to_path
            .get(&id)
            .ok_or(nfsstat3::NFS3ERR_NOENT)?
            .clone();
        // if there are children and the metadata did not change
        if entry.children.is_some() && !fattr3_differ(&entry.children_meta, &entry.fsmeta) {
            return Ok(());
        }
        if !matches!(entry.fsmeta.ftype, ftype3::NF3DIR) {
            return Ok(());
        }
        let mut cur_path = entry.name.clone();
        let path = self.sym_to_path(&entry.name).await;
        let mut new_children: Vec<u64> = Vec::new();
        debug!("Relisting entry {:?}", path);
        if let Ok(mut listing) = tokio::fs::read_dir(&path).await {
            while let Some(entry) = listing
                .next_entry()
                .await
                .map_err(|_| nfsstat3::NFS3ERR_IO)?
            {
                // Batch D (C3): a name that raced away between the listing
                // and its stat (ENOENT), or failed to intern, is skipped --
                // the next lookup refreshes it in. Never a panic: that
                // would take the FSMap task (and the NFS connection) down.
                if let Some(next_id) = self
                    .list_child(&mut cur_path, entry.file_name(), entry.metadata().await)
                    .await
                {
                    new_children.push(next_id);
                }
            }
            self.id_to_path
                .get_mut(&id)
                .ok_or(nfsstat3::NFS3ERR_NOENT)?
                .children = Some(BTreeSet::from_iter(new_children));
        }

        Ok(())
    }

    /// One `read_dir` entry -> child entry (C3): `None` when its stat
    /// failed (it raced away) or `name` cannot be interned -- skipped
    /// before anything is interned or pushed, `cur_path` stays as it was.
    async fn list_child(
        &mut self,
        cur_path: &mut Vec<Symbol>,
        name: OsString,
        meta: io::Result<Metadata>,
    ) -> Option<fileid3> {
        let meta = meta.ok()?;
        let sym = self.intern.intern(name).ok()?;
        cur_path.push(sym);
        let id = self.create_entry(cur_path, meta).await;
        cur_path.pop();
        Some(id)
    }

    pub(crate) async fn create_entry(&mut self, fullpath: &Vec<Symbol>, meta: Metadata) -> fileid3 {
        let next_id = if let Some(chid) = self.path_to_id.get(fullpath) {
            if let Some(chent) = self.id_to_path.get_mut(chid) {
                chent.fsmeta = metadata_to_fattr3(*chid, &meta);
            }
            *chid
        } else {
            // path does not exist
            let next_id = self.next_fileid.fetch_add(1, Ordering::Relaxed);
            let metafattr = metadata_to_fattr3(next_id, &meta);
            let new_entry = FSEntry {
                name: fullpath.clone(),
                fsmeta: metafattr,
                children_meta: metafattr,
                children: None,
            };
            debug!("creating new entry {:?}: {:?}", next_id, meta);
            self.id_to_path.insert(next_id, new_entry);
            self.path_to_id.insert(fullpath.clone(), next_id);
            next_id
        };
        next_id
    }

    /// creates a FS object in a given directory and of a given type
    pub(crate) async fn create_object(
        &mut self,
        dirid: fileid3,
        objectname: &filename3,
        object: &CreateFSObject,
    ) -> Result<(fileid3, fattr3), nfsstat3> {
        let ent = self.find_entry(dirid)?;
        let mut path = self.sym_to_path(&ent.name).await;
        let objectname_osstr = OsStr::from_bytes(objectname).to_os_string();
        path.push(&objectname_osstr);

        match object {
            CreateFSObject::Directory => {
                if exists_no_traverse(&path) {
                    return Err(nfsstat3::NFS3ERR_EXIST);
                }
                tokio::fs::create_dir(&path)
                    .await
                    .map_err(|_| nfsstat3::NFS3ERR_IO)?;
            }
            CreateFSObject::File(setattr) => {
                let file = File::create(&path).map_err(|_| nfsstat3::NFS3ERR_IO)?;
                let _ = file_setattr(&file, setattr).await;
            }
            CreateFSObject::Exclusive => {
                let _ = File::options()
                    .write(true)
                    .create_new(true)
                    .open(&path)
                    .map_err(|_| nfsstat3::NFS3ERR_EXIST)?;
            }
            CreateFSObject::Symlink((_, target)) => {
                if exists_no_traverse(&path) {
                    return Err(nfsstat3::NFS3ERR_EXIST);
                }
                tokio::fs::symlink(OsStr::from_bytes(target), &path)
                    .await
                    .map_err(|_| nfsstat3::NFS3ERR_IO)?;
                // we do not set attributes on symlinks
            }
        }

        let _ = self.refresh_entry(dirid).await;
        // C3: not an I/O race (intern only fails on capacity) but the same
        // posture -- an error reply, never a panic in the FSMap task.
        let sym = self
            .intern
            .intern(objectname_osstr)
            .map_err(|_| nfsstat3::NFS3ERR_IO)?;
        let mut name = ent.name.clone();
        name.push(sym);
        let meta = path.symlink_metadata().map_err(|_| nfsstat3::NFS3ERR_IO)?;
        let fileid = self.create_entry(&name, meta.clone()).await;
        // update the children list
        if let Some(ref mut children) = self
            .id_to_path
            .get_mut(&dirid)
            .ok_or(nfsstat3::NFS3ERR_NOENT)?
            .children
        {
            children.insert(fileid);
        }
        Ok((fileid, metadata_to_fattr3(fileid, &meta)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_root() -> PathBuf {
        let d = std::env::temp_dir().join(format!("init-pro-fsmap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Batch D (C3): a directory entry whose stat raced away (ENOENT) is
    /// skipped -- not a panic -- leaving no half-built path behind; a live
    /// entry still registers as before.
    #[tokio::test]
    async fn relisting_skips_a_child_that_raced_away() {
        let root = tmp_root();
        std::fs::write(root.join("kept"), b"k").unwrap();
        let mut map = FSMap::new(root.clone());
        map.refresh_dir_list(0).await.unwrap();
        let mut ids = Vec::new();
        map.collect_all_children(0, &mut ids);
        assert_eq!(ids.len(), 2, "root + the kept file");
        // The ENOENT half of the race: the stat fails after the listing.
        let mut cur_path = Vec::new();
        let gone = map
            .list_child(
                &mut cur_path,
                OsString::from("gone"),
                Err(io::Error::from(io::ErrorKind::NotFound)),
            )
            .await;
        assert_eq!(gone, None, "skipped");
        assert!(cur_path.is_empty(), "cur_path left as it was");
        assert!(
            map.intern.check_interned(OsStr::new("gone")).is_none(),
            "a skipped name leaves no interned symbol behind"
        );
        // The live half: a stat that works registers the child.
        let meta = std::fs::metadata(root.join("kept")).unwrap();
        let id = map
            .list_child(&mut cur_path, OsString::from("kept"), Ok(meta))
            .await
            .unwrap();
        let kept = map.intern.check_interned(OsStr::new("kept")).unwrap();
        assert!(cur_path.is_empty(), "cur_path restored after the live call");
        assert_eq!(map.path_to_id.get(&vec![kept]), Some(&id));
        let _ = std::fs::remove_dir_all(&root);
    }
}
