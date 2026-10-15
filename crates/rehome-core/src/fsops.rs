//! Scan, copy, compare and sync directory trees. `std` only.
//!
//! Links (symlinks/junctions) inside a tree are never followed and never copied:
//! the engine refuses to move trees that contain them.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Folder Rehome writes at the top of a relocated tree. Ignored by copy/compare.
pub const MARKER_DIR: &str = ".rehome";

#[derive(Debug, Default, Clone)]
pub struct ScanResult {
    pub files: u64,
    pub dirs: u64,
    pub bytes: u64,
    /// Up to 20 symlinks/junctions found inside the tree.
    pub links: Vec<PathBuf>,
    /// Files that live in the cloud and aren't fully on disk (OneDrive etc.).
    pub placeholders: u64,
    pub encrypted: u64,
    /// The tree already has a top-level marker folder.
    pub has_marker: bool,
}

#[derive(Debug, Clone, Copy, Default)]
struct Flags {
    placeholder: bool,
    encrypted: bool,
}

#[cfg(windows)]
fn flags(m: &fs::Metadata) -> Flags {
    use std::os::windows::fs::MetadataExt;
    let a = m.file_attributes();
    Flags {
        // RECALL_ON_DATA_ACCESS | RECALL_ON_OPEN | OFFLINE
        placeholder: a & (0x0040_0000 | 0x0004_0000 | 0x0000_1000) != 0,
        encrypted: a & 0x4000 != 0,
    }
}
#[cfg(not(windows))]
fn flags(_: &fs::Metadata) -> Flags {
    Flags::default()
}

fn cancelled() -> io::Error {
    io::Error::new(io::ErrorKind::Interrupted, "cancelled")
}

pub fn is_cancel(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::Interrupted && e.to_string() == "cancelled"
}

/// Walk `root` totalling size and noting anything the engine must refuse or warn about.
pub fn scan(root: &Path, should_cancel: &dyn Fn() -> bool) -> io::Result<ScanResult> {
    let mut r = ScanResult::default();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if should_cancel() {
            return Err(cancelled());
        }
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let ft = entry.file_type()?;
            if ft.is_symlink() {
                if r.links.len() < 20 {
                    r.links.push(entry.path());
                }
            } else if ft.is_dir() {
                if dir == root && entry.file_name() == MARKER_DIR {
                    r.has_marker = true;
                }
                r.dirs += 1;
                stack.push(entry.path());
            } else {
                let m = entry.metadata()?;
                r.files += 1;
                r.bytes += m.len();
                let f = flags(&m);
                r.placeholders += f.placeholder as u64;
                r.encrypted += f.encrypted as u64;
            }
        }
    }
    Ok(r)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    is_dir: bool,
    size: u64,
    mtime: Option<SystemTime>,
}

fn collect(root: &Path) -> io::Result<BTreeMap<PathBuf, Entry>> {
    let mut out = BTreeMap::new();
    let mut stack = vec![PathBuf::new()];
    while let Some(rel) = stack.pop() {
        for entry in fs::read_dir(root.join(&rel))? {
            let entry = entry?;
            let name = entry.file_name();
            if rel.as_os_str().is_empty() && name == MARKER_DIR {
                continue;
            }
            let ft = entry.file_type()?;
            let rel_path = rel.join(&name);
            if ft.is_symlink() {
                return Err(io::Error::other(format!(
                    "unexpected link at {}",
                    entry.path().display()
                )));
            }
            let m = entry.metadata()?;
            if ft.is_dir() {
                stack.push(rel_path.clone());
                out.insert(
                    rel_path,
                    Entry {
                        is_dir: true,
                        size: 0,
                        mtime: None,
                    },
                );
            } else {
                out.insert(
                    rel_path,
                    Entry {
                        is_dir: false,
                        size: m.len(),
                        mtime: m.modified().ok(),
                    },
                );
            }
        }
    }
    Ok(out)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Diff {
    /// In source, not in destination.
    Missing(PathBuf),
    /// In destination, not in source.
    Extra(PathBuf),
    /// Same name, different content signature (size or modified time).
    Changed(PathBuf),
    /// One side is a file, the other a folder.
    TypeMismatch(PathBuf),
}

impl Diff {
    pub fn path(&self) -> &Path {
        match self {
            Diff::Missing(p) | Diff::Extra(p) | Diff::Changed(p) | Diff::TypeMismatch(p) => p,
        }
    }
}

fn same_mtime(a: Option<SystemTime>, b: Option<SystemTime>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => {
            let d = a
                .duration_since(b)
                .or_else(|e| Ok::<_, ()>(e.duration()))
                .unwrap();
            d <= Duration::from_secs(2) // FAT-family drives store 2-second resolution
        }
        _ => true,
    }
}

/// Compare two trees by names, kinds, sizes and modified times.
pub fn diff(src: &Path, dst: &Path) -> io::Result<Vec<Diff>> {
    let (s, d) = (collect(src)?, collect(dst)?);
    let mut out = Vec::new();
    for (p, se) in &s {
        match d.get(p) {
            None => out.push(Diff::Missing(p.clone())),
            Some(de) if de.is_dir != se.is_dir => out.push(Diff::TypeMismatch(p.clone())),
            Some(de) if !se.is_dir && (de.size != se.size || !same_mtime(se.mtime, de.mtime)) => {
                out.push(Diff::Changed(p.clone()))
            }
            _ => {}
        }
    }
    for p in d.keys() {
        if !s.contains_key(p) {
            out.push(Diff::Extra(p.clone()));
        }
    }
    Ok(out)
}

fn set_mtime(path: &Path, t: SystemTime) -> io::Result<()> {
    #[cfg(windows)]
    let f = {
        use std::os::windows::fs::OpenOptionsExt;
        // FILE_WRITE_ATTRIBUTES works even on read-only files.
        fs::OpenOptions::new()
            .access_mode(0x0100)
            .share_mode(7)
            .open(path)?
    };
    #[cfg(not(windows))]
    let f = fs::File::open(path)?;
    f.set_modified(t)
}

fn copy_file(from: &Path, to: &Path) -> io::Result<u64> {
    let n = fs::copy(from, to)?;
    if let Ok(t) = fs::metadata(from).and_then(|m| m.modified()) {
        set_mtime(to, t)?;
    }
    Ok(n)
}

/// Copy the whole tree. `dst` must not exist. `progress` receives total bytes copied
/// so far and returns `false` to cancel.
pub fn copy_tree(src: &Path, dst: &Path, progress: &mut dyn FnMut(u64) -> bool) -> io::Result<()> {
    fs::create_dir(dst)?;
    let mut done = 0u64;
    let mut stack = vec![PathBuf::new()];
    while let Some(rel) = stack.pop() {
        for entry in fs::read_dir(src.join(&rel))? {
            let entry = entry?;
            if rel.as_os_str().is_empty() && entry.file_name() == MARKER_DIR {
                continue;
            }
            let ft = entry.file_type()?;
            let rel_path = rel.join(entry.file_name());
            if ft.is_symlink() {
                return Err(io::Error::other(format!(
                    "unexpected link at {}",
                    entry.path().display()
                )));
            } else if ft.is_dir() {
                fs::create_dir(dst.join(&rel_path))?;
                stack.push(rel_path);
            } else {
                done += copy_file(&entry.path(), &dst.join(&rel_path))?;
                if !progress(done) {
                    return Err(cancelled());
                }
            }
        }
    }
    Ok(())
}

/// Make `dst` match `src` by applying `diffs` (from [`diff`]`(src, dst)`).
pub fn apply_diff(src: &Path, dst: &Path, diffs: &[Diff]) -> io::Result<()> {
    let mut sorted: Vec<&Diff> = diffs.iter().collect();
    sorted.sort_by(|a, b| a.path().cmp(b.path()));
    // Removals first, deepest first, so replaced items make way for new ones.
    for d in sorted.iter().rev() {
        if matches!(d, Diff::Extra(_) | Diff::TypeMismatch(_)) {
            remove_any(&dst.join(d.path()))?;
        }
    }
    // Then creations, parents before children.
    for d in &sorted {
        if matches!(
            d,
            Diff::Missing(_) | Diff::Changed(_) | Diff::TypeMismatch(_)
        ) {
            let from = src.join(d.path());
            let to = dst.join(d.path());
            if fs::metadata(&from)?.is_dir() {
                fs::create_dir_all(&to)?;
            } else {
                if matches!(d, Diff::Changed(_)) {
                    remove_any(&to)?;
                }
                copy_file(&from, &to)?;
            }
        }
    }
    Ok(())
}

fn remove_any(p: &Path) -> io::Result<()> {
    match fs::symlink_metadata(p) {
        Ok(m) if m.is_dir() && !m.file_type().is_symlink() => fs::remove_dir_all(p),
        Ok(_) => fs::remove_file(p),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Delete a directory tree if it exists. Never follows links.
pub fn remove_tree(p: &Path) -> io::Result<()> {
    match fs::symlink_metadata(p) {
        Ok(m) if m.is_dir() && !m.file_type().is_symlink() => fs::remove_dir_all(p),
        Ok(_) => Err(io::Error::other("refusing to delete: not a real directory")),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("rehome-fsops-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }

    fn sample(root: &Path) {
        fs::create_dir_all(root.join("a/b")).unwrap();
        fs::create_dir_all(root.join("empty")).unwrap();
        fs::write(root.join("top.txt"), "hello").unwrap();
        fs::write(root.join("a/b/deep.bin"), vec![7u8; 4096]).unwrap();
    }

    #[test]
    fn scan_counts() {
        let r = tmp("scan");
        sample(&r);
        let s = scan(&r, &|| false).unwrap();
        assert_eq!((s.files, s.dirs, s.bytes), (2, 3, 4101));
        assert!(s.links.is_empty() && !s.has_marker);
        fs::create_dir(r.join(MARKER_DIR)).unwrap();
        assert!(scan(&r, &|| false).unwrap().has_marker);
    }

    #[test]
    fn scan_can_cancel() {
        let r = tmp("scancancel");
        sample(&r);
        assert!(is_cancel(&scan(&r, &|| true).unwrap_err()));
    }

    #[test]
    fn copy_then_diff_is_clean() {
        let r = tmp("copy");
        let (s, d) = (r.join("s"), r.join("d"));
        fs::create_dir(&s).unwrap();
        sample(&s);
        copy_tree(&s, &d, &mut |_| true).unwrap();
        assert_eq!(diff(&s, &d).unwrap(), vec![]);
        assert_eq!(fs::read(d.join("a/b/deep.bin")).unwrap().len(), 4096);
        assert!(d.join("empty").is_dir());
    }

    #[test]
    fn copy_refuses_existing_destination() {
        let r = tmp("copyexists");
        let (s, d) = (r.join("s"), r.join("d"));
        fs::create_dir(&s).unwrap();
        fs::create_dir(&d).unwrap();
        assert!(copy_tree(&s, &d, &mut |_| true).is_err());
    }

    #[test]
    fn copy_can_cancel() {
        let r = tmp("copycancel");
        let (s, d) = (r.join("s"), r.join("d"));
        fs::create_dir(&s).unwrap();
        sample(&s);
        assert!(is_cancel(&copy_tree(&s, &d, &mut |_| false).unwrap_err()));
    }

    #[test]
    fn diff_detects_every_kind_of_difference() {
        let r = tmp("diff");
        let (s, d) = (r.join("s"), r.join("d"));
        fs::create_dir(&s).unwrap();
        sample(&s);
        copy_tree(&s, &d, &mut |_| true).unwrap();
        fs::remove_file(d.join("top.txt")).unwrap(); // missing
        fs::write(d.join("extra.txt"), "x").unwrap(); // extra
        fs::write(d.join("a/b/deep.bin"), "short").unwrap(); // changed
        let mut got = diff(&s, &d).unwrap();
        got.sort_by(|a, b| a.path().cmp(b.path()));
        assert_eq!(
            got,
            vec![
                Diff::Changed("a/b/deep.bin".into()),
                Diff::Extra("extra.txt".into()),
                Diff::Missing("top.txt".into()),
            ]
        );
    }

    #[test]
    fn apply_diff_converges() {
        let r = tmp("apply");
        let (s, d) = (r.join("s"), r.join("d"));
        fs::create_dir(&s).unwrap();
        sample(&s);
        copy_tree(&s, &d, &mut |_| true).unwrap();
        // The source changes after the copy.
        fs::write(s.join("top.txt"), "changed contents").unwrap();
        fs::create_dir_all(s.join("new/sub")).unwrap();
        fs::write(s.join("new/sub/f"), "n").unwrap();
        fs::remove_dir_all(s.join("a")).unwrap();
        fs::remove_dir(s.join("empty")).unwrap(); // dir -> file swap
        fs::write(s.join("empty"), "now a file").unwrap();
        let ds = diff(&s, &d).unwrap();
        assert!(!ds.is_empty());
        apply_diff(&s, &d, &ds).unwrap();
        assert_eq!(diff(&s, &d).unwrap(), vec![]);
        assert_eq!(fs::read_to_string(d.join("empty")).unwrap(), "now a file");
        assert!(!d.join("a").exists());
    }

    #[test]
    fn marker_dir_is_ignored() {
        let r = tmp("marker");
        let (s, d) = (r.join("s"), r.join("d"));
        fs::create_dir(&s).unwrap();
        sample(&s);
        copy_tree(&s, &d, &mut |_| true).unwrap();
        fs::create_dir(d.join(MARKER_DIR)).unwrap();
        fs::write(d.join(MARKER_DIR).join("origin.txt"), "x").unwrap();
        assert_eq!(diff(&s, &d).unwrap(), vec![]);
    }

    #[cfg(unix)]
    #[test]
    fn links_are_reported_and_refused() {
        let r = tmp("links");
        let s = r.join("s");
        fs::create_dir(&s).unwrap();
        std::os::unix::fs::symlink("/tmp", s.join("lnk")).unwrap();
        assert_eq!(scan(&s, &|| false).unwrap().links.len(), 1);
        assert!(copy_tree(&s, &r.join("d"), &mut |_| true).is_err());
    }

    #[test]
    fn remove_tree_refuses_a_file() {
        let r = tmp("rmfile");
        fs::write(r.join("f"), "x").unwrap();
        assert!(remove_tree(&r.join("f")).is_err());
        assert!(remove_tree(&r.join("missing")).is_ok());
    }
}
