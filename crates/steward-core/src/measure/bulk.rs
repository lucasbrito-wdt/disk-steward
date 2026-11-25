//! One `getattrlistbulk` call yields names, type, sizes and private bytes for a whole directory.

use std::{
    ffi::OsStr,
    fs::File,
    io,
    os::{fd::AsRawFd, unix::ffi::OsStrExt},
    path::{Path, PathBuf},
};

use super::UF_COMPRESSED;

// sys/attr.h — not yet in every libc release we pin.
const ATTR_CMN_ERROR: u32 = 0x2000_0000;
// sys/vnode.h
const VREG: u32 = 1;
const VDIR: u32 = 2;

const BUF_SIZE: usize = 64 * 1024;

const COMMON: u32 = libc::ATTR_CMN_RETURNED_ATTRS
    | libc::ATTR_CMN_NAME
    | ATTR_CMN_ERROR
    | libc::ATTR_CMN_OBJTYPE
    | libc::ATTR_CMN_DEVID
    | libc::ATTR_CMN_FLAGS
    | libc::ATTR_CMN_FILEID;

const FILE: u32 =
    libc::ATTR_FILE_LINKCOUNT | libc::ATTR_FILE_ALLOCSIZE | libc::ATTR_FILE_DATALENGTH;

const OPTIONS: u64 = libc::FSOPT_ATTR_CMN_EXTENDED as u64;

#[derive(Debug)]
pub enum DirEnt {
    Dir { path: PathBuf, dev: u64 },
    File(FileEnt),
}

#[derive(Debug)]
pub struct FileEnt {
    pub path: PathBuf,
    pub logical: u64,
    pub on_disk: u64,
    pub nlink: u64,
    pub ino: u64,
    pub dev: u64,
    pub compressed: bool,
    pub private: io::Result<u64>,
}

/// Lists direct children of `dir` via `getattrlistbulk` (symlinks and non-files are skipped).
pub fn read_dir_bulk(dir: &Path) -> io::Result<Vec<DirEnt>> {
    let file = File::open(dir)?;
    let fd = file.as_raw_fd();
    let mut list = libc::attrlist {
        bitmapcount: libc::ATTR_BIT_MAP_COUNT,
        reserved: 0,
        commonattr: COMMON,
        volattr: 0,
        dirattr: 0,
        fileattr: FILE,
        forkattr: libc::ATTR_CMNEXT_PRIVATESIZE,
    };
    let mut buf = vec![0u8; BUF_SIZE];
    let mut out = Vec::new();
    loop {
        // SAFETY: `fd` is an open directory; `list` and `buf` are valid for the call.
        let n = unsafe {
            libc::getattrlistbulk(
                fd,
                (&raw mut list).cast(),
                buf.as_mut_ptr().cast(),
                buf.len(),
                OPTIONS,
            )
        };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        if n == 0 {
            break;
        }
        let mut off = 0usize;
        for _ in 0..n {
            let Some((ent, next)) = parse_entry(&buf, off, dir)? else {
                break;
            };
            off = next;
            if let Some(ent) = ent {
                out.push(ent);
            }
        }
    }
    Ok(out)
}

fn parse_entry(buf: &[u8], off: usize, dir: &Path) -> io::Result<Option<(Option<DirEnt>, usize)>> {
    if off + 4 > buf.len() {
        return Ok(None);
    }
    let len = u32::from_ne_bytes(buf[off..off + 4].try_into().unwrap()) as usize;
    if len < 4 || off + len > buf.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "getattrlistbulk entry length out of range",
        ));
    }
    let record = &buf[off..off + len];
    let next = off + len;
    let mut p = 4usize;

    let returned = read_attr_set(record, &mut p)?;
    let mut error = 0u32;
    if returned.commonattr & ATTR_CMN_ERROR != 0 {
        error = read_u32(record, &mut p)?;
    }

    let mut name_ref: Option<(usize, libc::attrreference_t)> = None;
    let mut obj_type = 0u32;
    let mut devid = 0i32;
    let mut flags = 0u32;
    let mut fileid = 0u64;

    // Common attrs after ERROR, in ascending bit order (Apple sample for ERROR-before-name).
    if returned.commonattr & libc::ATTR_CMN_NAME != 0 {
        let at = p;
        let r = read_attr_ref(record, &mut p)?;
        name_ref = Some((at, r));
    }
    if returned.commonattr & libc::ATTR_CMN_DEVID != 0 {
        devid = read_i32(record, &mut p)?;
    }
    if returned.commonattr & libc::ATTR_CMN_OBJTYPE != 0 {
        obj_type = read_u32(record, &mut p)?;
    }
    if returned.commonattr & libc::ATTR_CMN_FLAGS != 0 {
        flags = read_u32(record, &mut p)?;
    }
    if returned.commonattr & libc::ATTR_CMN_FILEID != 0 {
        fileid = read_u64(record, &mut p)?;
    }

    let mut nlink = 1u32;
    let mut allocsize = 0i64;
    let mut datalen = 0i64;
    if returned.fileattr & libc::ATTR_FILE_LINKCOUNT != 0 {
        nlink = read_u32(record, &mut p)?;
    }
    if returned.fileattr & libc::ATTR_FILE_ALLOCSIZE != 0 {
        allocsize = read_i64(record, &mut p)?;
    }
    if returned.fileattr & libc::ATTR_FILE_DATALENGTH != 0 {
        datalen = read_i64(record, &mut p)?;
    }

    let mut privatesize: Option<i64> = None;
    if returned.forkattr & libc::ATTR_CMNEXT_PRIVATESIZE != 0 {
        privatesize = Some(read_i64(record, &mut p)?);
    }

    if error != 0 {
        return Ok(Some((None, next)));
    }
    let Some((name_at, name_info)) = name_ref else {
        return Ok(Some((None, next)));
    };
    let name = read_name(record, name_at, name_info)?;
    if name == OsStr::new(".") || name == OsStr::new("..") {
        return Ok(Some((None, next)));
    }
    let path = dir.join(name);
    let dev = devid as u64;

    let ent = match obj_type {
        VDIR => Some(DirEnt::Dir { path, dev }),
        VREG => {
            let logical = u64::try_from(datalen).unwrap_or(0);
            let on_disk = u64::try_from(allocsize).unwrap_or(0);
            let compressed = flags & UF_COMPRESSED != 0;
            let private = if compressed {
                Ok(on_disk)
            } else {
                match privatesize {
                    Some(n) => u64::try_from(n).map_err(|_| {
                        io::Error::new(io::ErrorKind::InvalidData, "negative private size")
                    }),
                    None => Err(io::Error::new(
                        io::ErrorKind::Unsupported,
                        "private size not returned",
                    )),
                }
            };
            Some(DirEnt::File(FileEnt {
                path,
                logical,
                on_disk,
                nlink: u64::from(nlink),
                ino: fileid,
                dev,
                compressed,
                private,
            }))
        }
        _ => None, // symlink, device, …
    };
    Ok(Some((ent, next)))
}

fn read_attr_set(buf: &[u8], p: &mut usize) -> io::Result<libc::attribute_set_t> {
    Ok(libc::attribute_set_t {
        commonattr: read_u32(buf, p)?,
        volattr: read_u32(buf, p)?,
        dirattr: read_u32(buf, p)?,
        fileattr: read_u32(buf, p)?,
        forkattr: read_u32(buf, p)?,
    })
}

fn read_attr_ref(buf: &[u8], p: &mut usize) -> io::Result<libc::attrreference_t> {
    let attr_dataoffset = read_i32(buf, p)?;
    let attr_length = read_u32(buf, p)?;
    Ok(libc::attrreference_t {
        attr_dataoffset,
        attr_length,
    })
}

fn read_name<'a>(
    record: &'a [u8],
    ref_at: usize,
    info: libc::attrreference_t,
) -> io::Result<&'a OsStr> {
    let start = ref_at
        .checked_add(info.attr_dataoffset as usize)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "name offset overflow"))?;
    let end = start
        .checked_add(info.attr_length as usize)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "name length overflow"))?;
    if end > record.len() || info.attr_length == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "name reference out of range",
        ));
    }
    let bytes = &record[start..end];
    // Length includes the trailing NUL.
    let bare = bytes.strip_suffix(&[0]).unwrap_or(bytes);
    Ok(OsStr::from_bytes(bare))
}

fn read_u32(buf: &[u8], p: &mut usize) -> io::Result<u32> {
    let b = take(buf, p, 4)?;
    Ok(u32::from_ne_bytes(b.try_into().unwrap()))
}

fn read_i32(buf: &[u8], p: &mut usize) -> io::Result<i32> {
    let b = take(buf, p, 4)?;
    Ok(i32::from_ne_bytes(b.try_into().unwrap()))
}

fn read_u64(buf: &[u8], p: &mut usize) -> io::Result<u64> {
    let b = take(buf, p, 8)?;
    Ok(u64::from_ne_bytes(b.try_into().unwrap()))
}

fn read_i64(buf: &[u8], p: &mut usize) -> io::Result<i64> {
    let b = take(buf, p, 8)?;
    Ok(i64::from_ne_bytes(b.try_into().unwrap()))
}

fn take<'a>(buf: &'a [u8], p: &mut usize, n: usize) -> io::Result<&'a [u8]> {
    let end = p
        .checked_add(n)
        .filter(|e| *e <= buf.len())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "attr buffer underrun"))?;
    let slice = &buf[*p..end];
    *p = end;
    Ok(slice)
}
