use super::*;
use std::{
    collections::HashSet,
    os::{macos::fs::MetadataExt as _, unix::fs::MetadataExt},
    process::Command,
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

/// Per-file `lstat` + `getattrlist` walk for equivalence / bench against bulk.
fn measure_legacy(root: &Path) -> io::Result<Measure> {
    let root_md = fs::metadata(root)?;
    let seen = Mutex::new(HashSet::new());
    if root_md.is_file() {
        let mut acc = Acc::new(&seen);
        acc.file_from_meta(root, &root_md);
        return Ok(acc.m);
    }
    let dev = root_md.dev();
    let queue = Mutex::new(vec![root.to_path_buf()]);
    let pending = AtomicUsize::new(1);
    Ok(thread::scope(|s| {
        let workers: Vec<_> = (0..WORKERS)
            .map(|_| {
                s.spawn(|| {
                    let mut acc = Acc::new(&seen);
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
                        for entry in fs::read_dir(&dir).into_iter().flatten().flatten() {
                            let Ok(md) = entry.metadata() else { continue };
                            if md.dev() != dev {
                                continue;
                            }
                            if md.is_dir() {
                                subdirs.push(entry.path());
                            } else if md.is_file() {
                                acc.file_from_meta(&entry.path(), &md);
                            }
                        }
                        pending.fetch_add(subdirs.len(), Ordering::SeqCst);
                        queue.lock().expect("queue lock").extend(subdirs);
                        pending.fetch_sub(1, Ordering::SeqCst);
                    }
                })
            })
            .collect();
        workers
            .into_iter()
            .map(|w| w.join().expect("legacy worker panicked"))
            .fold(Acc::new(&seen).m, merge)
    }))
}

#[test]
fn constants_match_sdk_header() {
    assert_eq!(libc::ATTR_CMNEXT_PRIVATESIZE, 0x8);
    assert_eq!(libc::FSOPT_ATTR_CMN_EXTENDED, 0x20);
    assert_eq!(libc::FSOPT_NOFOLLOW, 0x1);
    assert_eq!(libc::ATTR_BIT_MAP_COUNT, 5);
    assert_eq!(libc::ATTR_CMN_NAME, 0x1);
    assert_eq!(libc::ATTR_CMN_RETURNED_ATTRS, 0x8000_0000);
    assert_eq!(libc::ATTR_FILE_ALLOCSIZE, 0x4);
    assert_eq!(libc::ATTR_FILE_DATALENGTH, 0x200);
}

#[test]
fn counts_files_and_skips_symlinks() {
    let t = tempfile::tempdir().unwrap();
    fs::create_dir(t.path().join("d")).unwrap();
    fs::write(t.path().join("d/a"), vec![1u8; 10_000]).unwrap();
    fs::write(t.path().join("b"), vec![2u8; 5_000]).unwrap();
    std::os::unix::fs::symlink(t.path().join("d"), t.path().join("link")).unwrap();
    fs::hard_link(t.path().join("b"), t.path().join("b2")).unwrap();
    let m = measure(t.path()).unwrap();
    assert_eq!(
        (m.total_files, m.logical_bytes, m.compressed_files),
        (2, 15_000, 0)
    );
    assert!(m.on_disk_bytes >= 15_000);
}

#[test]
fn excluded_paths_are_not_measured() {
    let t = tempfile::tempdir().unwrap();
    let env = t.path().join("archive-v0/h1/lib/python3.13");
    fs::create_dir_all(&env).unwrap();
    fs::create_dir_all(t.path().join("archive-v0/wheel")).unwrap();
    let data: Vec<u8> = (0..4_194_304u32)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
        .collect();
    fs::write(env.join("orig"), &data).unwrap();
    let cloned = Command::new("cp")
        .arg("-c")
        .arg(env.join("orig"))
        .arg(env.join("clone"))
        .status()
        .unwrap();
    if !cloned.success() {
        eprintln!("SKIP: cp -c failed (temp dir is not on APFS?)");
        return;
    }
    fs::write(t.path().join("archive-v0/wheel/mod.py"), "x = 1").unwrap();
    let all = measure(t.path()).unwrap();
    assert!(
        all.private_bytes.unwrap() < all.on_disk_bytes,
        "clones must show as shared"
    );
    let mut skip = globset::GlobSetBuilder::new();
    let pat = format!("{}/archive-v0/*/lib/python*/**", t.path().display());
    skip.add(
        globset::GlobBuilder::new(&pat)
            .literal_separator(true)
            .build()
            .unwrap(),
    );
    let m = measure_excluding(t.path(), &skip.build().unwrap()).unwrap();
    assert_eq!(m.total_files, 1);
    assert_eq!(m.private_bytes, Some(m.on_disk_bytes));
}

#[test]
fn clone_has_smaller_private_size_than_on_disk() {
    let t = tempfile::tempdir().unwrap();
    let data: Vec<u8> = (0..4_194_304u32)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
        .collect();
    fs::write(t.path().join("orig"), &data).unwrap();
    let cloned = Command::new("cp")
        .arg("-c")
        .arg(t.path().join("orig"))
        .arg(t.path().join("clone"))
        .status()
        .unwrap();
    if !cloned.success() {
        eprintln!("SKIP: cp -c failed (temp dir is not on APFS?)");
        return;
    }
    let m = measure(t.path()).unwrap();
    let private = m.private_bytes.expect("private size supported");
    assert!(
        private < m.on_disk_bytes,
        "private {private} vs on disk {}",
        m.on_disk_bytes
    );
    fs::write(
        t.path().join("unique"),
        data.iter().rev().copied().collect::<Vec<_>>(),
    )
    .unwrap();
    let alone = measure(&t.path().join("unique")).unwrap();
    assert_eq!(alone.private_bytes, Some(alone.on_disk_bytes));
}

/// Fixture with clones and (when `ditto` cooperates) a compressed file; bulk must match legacy.
#[test]
fn bulk_matches_legacy_on_clones_and_compressed() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path();
    fs::create_dir_all(root.join("sub/a")).unwrap();
    let data: Vec<u8> = (0..256_000u32)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
        .collect();
    fs::write(root.join("sub/a/orig"), &data).unwrap();
    let cloned = Command::new("cp")
        .arg("-c")
        .arg(root.join("sub/a/orig"))
        .arg(root.join("sub/a/clone"))
        .status()
        .unwrap();
    if !cloned.success() {
        eprintln!("SKIP: cp -c failed (temp dir is not on APFS?)");
        return;
    }
    fs::write(root.join("plain.txt"), b"hello").unwrap();
    fs::hard_link(root.join("plain.txt"), root.join("plain2.txt")).unwrap();
    let compressible = "compressible text line\n".repeat(40_000);
    let src = root.join("to_compress.txt");
    fs::write(&src, &compressible).unwrap();
    let dst = root.join("compressed.txt");
    let ditto = Command::new("ditto")
        .args(["--hfsCompression"])
        .arg(&src)
        .arg(&dst)
        .status();
    let compressed_ok = ditto.map(|s| s.success()).unwrap_or(false);
    if !compressed_ok {
        eprintln!("note: ditto --hfsCompression unavailable; comparing without compressed file");
        let _ = fs::remove_file(&src);
    } else {
        let _ = fs::remove_file(&src);
        let md = fs::metadata(&dst).unwrap();
        assert!(
            md.st_flags() & UF_COMPRESSED != 0,
            "ditto should set UF_COMPRESSED"
        );
    }

    let bulk = measure(root).unwrap();
    let legacy = measure_legacy(root).unwrap();
    assert_eq!(bulk.total_files, legacy.total_files);
    assert_eq!(bulk.logical_bytes, legacy.logical_bytes);
    assert_eq!(bulk.compressed_files, legacy.compressed_files);
    assert_eq!(bulk.on_disk_bytes, legacy.on_disk_bytes);
    assert_eq!(bulk.private_bytes, legacy.private_bytes);
    if compressed_ok {
        assert!(bulk.compressed_files >= 1);
    }
    assert!(
        bulk.private_bytes.unwrap() < bulk.on_disk_bytes,
        "clones must reduce private vs on-disk"
    );
}

/// Benchmark: bulk vs legacy on a few thousand small files (printed; fails if bulk is >> slower).
#[test]
fn bench_bulk_vs_legacy() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path();
    for d in 0..20u32 {
        let dir = root.join(format!("d{d}"));
        fs::create_dir(&dir).unwrap();
        for f in 0..100u32 {
            fs::write(dir.join(format!("f{f}.dat")), vec![f as u8; 64]).unwrap();
        }
    }
    let _ = measure(root).unwrap();
    let _ = measure_legacy(root).unwrap();

    let t0 = Instant::now();
    let bulk = measure(root).unwrap();
    let bulk_ms = t0.elapsed().as_secs_f64() * 1000.0;

    let t1 = Instant::now();
    let legacy = measure_legacy(root).unwrap();
    let legacy_ms = t1.elapsed().as_secs_f64() * 1000.0;

    assert_eq!(bulk.total_files, legacy.total_files);
    assert_eq!(bulk.logical_bytes, legacy.logical_bytes);
    assert_eq!(bulk.on_disk_bytes, legacy.on_disk_bytes);
    assert_eq!(bulk.private_bytes, legacy.private_bytes);

    eprintln!(
        "bench_bulk_vs_legacy: {} files; bulk={bulk_ms:.2} ms, legacy={legacy_ms:.2} ms, speedup={:.2}x",
        bulk.total_files,
        legacy_ms / bulk_ms.max(0.001)
    );
    assert!(
        bulk_ms <= legacy_ms * 2.0 + 50.0,
        "bulk {bulk_ms:.2} ms slower than legacy {legacy_ms:.2} ms"
    );
}
