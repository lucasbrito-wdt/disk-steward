//! Clone-aware size measurement. `du` counts clones twice; `private_bytes` (getattrlist
//! ATTR_CMNEXT_PRIVATESIZE) only counts bytes not shared with any other file.
//!
//! Directory walks use `getattrlistbulk` so one syscall returns type, sizes and private
//! bytes for every entry (instead of per-file `lstat` + `getattrlist`).

mod bulk;

use crate::model::Measure;
use bulk::{DirEnt, FileEnt};
use globset::GlobSet;
use std::{
    collections::HashSet,
    ffi::CString,
    fs, io,
    os::{
        macos::fs::MetadataExt as _,
        unix::{ffi::OsStrExt, fs::MetadataExt},
    },
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
    time::Duration,
};

const WORKERS: usize = 4;

pub(crate) const UF_COMPRESSED: u32 = 0x20; // sys/stat.h
// Header values (sys/attr.h): ATTR_CMNEXT_PRIVATESIZE 0x8 (in `forkattr`), FSOPT_ATTR_CMN_EXTENDED 0x20.
const OPTIONS: u32 = libc::FSOPT_NOFOLLOW | libc::FSOPT_ATTR_CMN_EXTENDED;

/// Walks `root` with a small worker pool (symlinks are not followed, other filesystems are not entered).
/// `private_bytes` is `None` when the filesystem refuses the attribute for any file.
pub fn measure(root: &Path) -> io::Result<Measure> {
    measure_excluding(root, &GlobSet::empty())
}

/// Like `measure`, skipping files whose full path matches `skip` (engine `exclude` semantics).
pub fn measure_excluding(root: &Path, skip: &GlobSet) -> io::Result<Measure> {
    let root_md = fs::metadata(root)?;
    let seen = Mutex::new(HashSet::new());
    if root_md.is_file() {
        let mut acc = Acc::new(&seen);
        acc.file_from_meta(root, &root_md);
        return Ok(acc.m);
    }
    let dev = root_md.dev();
    let queue = Mutex::new(vec![root.to_path_buf()]);
    let pending = AtomicUsize::new(1); // directories queued or being read
    Ok(thread::scope(|s| {
        let workers: Vec<_> = (0..WORKERS)
            .map(|_| s.spawn(|| walk(dev, &queue, &pending, &seen, skip)))
            .collect();
        workers
            .into_iter()
            .map(|w| w.join().expect("measure worker panicked"))
            .fold(Acc::new(&seen).m, merge)
    }))
}

fn walk(
    root_dev: u64,
    queue: &Mutex<Vec<PathBuf>>,
    pending: &AtomicUsize,
    seen: &Mutex<HashSet<(u64, u64)>>,
    skip: &GlobSet,
) -> Measure {
    let mut acc = Acc::new(seen);
    loop {
        let next = queue.lock().expect("queue lock").pop();
        let Some(dir) = next else {
            if pending.load(Ordering::SeqCst) == 0 {
                return acc.m;
            }
            thread::sleep(Duration::from_micros(200));
            continue;
        };
        let mut subdirs = Vec::new();
        match bulk::read_dir_bulk(&dir) {
            Ok(entries) => {
                for ent in entries {
                    match ent {
                        DirEnt::Dir { path, dev } => {
                            if dev == root_dev {
                                subdirs.push(path);
                            }
                        }
                        DirEnt::File(f) => {
                            if f.dev == root_dev && !skip.is_match(&f.path) {
                                acc.file_ent(f);
                            }
                        }
                    }
                }
            }
            Err(_) => {
                // Permission / parse / ERANGE: fall back to per-entry lstat + getattrlist.
                for entry in fs::read_dir(&dir).into_iter().flatten().flatten() {
                    let Ok(md) = entry.metadata() else { continue };
                    if md.dev() != root_dev {
                        continue;
                    }
                    if md.is_dir() {
                        subdirs.push(entry.path());
                    } else if md.is_file() && !skip.is_match(entry.path()) {
                        acc.file_from_meta(&entry.path(), &md);
                    }
                }
            }
        }
        pending.fetch_add(subdirs.len(), Ordering::SeqCst); // before the parent is released
        queue.lock().expect("queue lock").extend(subdirs);
        pending.fetch_sub(1, Ordering::SeqCst);
    }
}

fn merge(a: Measure, b: Measure) -> Measure {
    Measure {
        logical_bytes: a.logical_bytes + b.logical_bytes,
        on_disk_bytes: a.on_disk_bytes + b.on_disk_bytes,
        private_bytes: a.private_bytes.zip(b.private_bytes).map(|(x, y)| x + y),
        total_files: a.total_files + b.total_files,
        compressed_files: a.compressed_files + b.compressed_files,
    }
}

struct Acc<'a> {
    m: Measure,
    seen: &'a Mutex<HashSet<(u64, u64)>>,
}

impl<'a> Acc<'a> {
    fn new(seen: &'a Mutex<HashSet<(u64, u64)>>) -> Self {
        Self {
            m: Measure {
                private_bytes: Some(0),
                ..Measure::default()
            },
            seen,
        }
    }

    fn file_from_meta(&mut self, path: &Path, md: &fs::Metadata) {
        let on_disk = md.blocks() * 512;
        let compressed = md.st_flags() & UF_COMPRESSED != 0;
        let private = if compressed {
            Ok(on_disk)
        } else {
            private_size(path)
        };
        self.accumulate(
            md.nlink(),
            md.dev(),
            md.ino(),
            md.len(),
            on_disk,
            compressed,
            private,
        );
    }

    fn file_ent(&mut self, f: FileEnt) {
        self.accumulate(
            f.nlink,
            f.dev,
            f.ino,
            f.logical,
            f.on_disk,
            f.compressed,
            f.private,
        );
    }

    fn accumulate(
        &mut self,
        nlink: u64,
        dev: u64,
        ino: u64,
        logical: u64,
        on_disk: u64,
        compressed: bool,
        private: io::Result<u64>,
    ) {
        if nlink > 1 && !self.seen.lock().expect("seen lock").insert((dev, ino)) {
            return; // hardlink already counted
        }
        self.m.total_files += 1;
        self.m.logical_bytes += logical;
        self.m.on_disk_bytes += on_disk;
        self.m.compressed_files += u64::from(compressed);
        // The kernel reports 0 private bytes for compressed files (their data sits in the resource
        // fork / decmpfs xattr, not in the data fork). The engine writes compressed copies fresh
        // (never clones), so their on-disk size is private.
        match private {
            Ok(n) => self.m.private_bytes = self.m.private_bytes.map(|p| p + n),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {} // vanished mid-walk
            Err(_) => self.m.private_bytes = None,
        }
    }
}

/// Bytes of `path` not shared with any clone.
pub fn private_size(path: &Path) -> io::Result<u64> {
    let c = CString::new(path.as_os_str().as_bytes())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    // SAFETY: attrlist is plain old data; zeroed is a valid initial value.
    let mut list: libc::attrlist = unsafe { std::mem::zeroed() };
    list.bitmapcount = libc::ATTR_BIT_MAP_COUNT;
    list.forkattr = libc::ATTR_CMNEXT_PRIVATESIZE;
    // Reply layout: u32 total length, then off_t (read bytewise: packing is not 8-aligned).
    let mut buf = [0u8; 12];
    // SAFETY: valid path, attrlist and buffer of the size passed.
    let rc = unsafe {
        libc::getattrlist(
            c.as_ptr(),
            (&raw mut list).cast(),
            buf.as_mut_ptr().cast(),
            buf.len(),
            OPTIONS,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    let size = i64::from_ne_bytes(buf[4..12].try_into().expect("8 bytes"));
    u64::try_from(size)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "negative private size"))
}

#[cfg(test)]
mod tests;
