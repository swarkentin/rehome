//! The only seam between the engine and the operating system.

use std::io;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriveKind {
    Fixed,
    Removable,
    Network,
    Optical,
    Ram,
    Unknown,
}

#[derive(Debug, Clone)]
pub struct VolumeInfo {
    pub kind: DriveKind,
    /// "NTFS", "ReFS", "exFAT", "FAT32", ...
    pub fs: String,
    pub free_bytes: u64,
    /// Stable identity of the volume; equal ids mean the same volume.
    pub volume_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppUser {
    pub name: String,
    pub pid: Option<u32>,
    pub is_service: bool,
}

pub trait Platform {
    /// Volume holding `path` (the nearest existing ancestor is used if `path` doesn't exist).
    fn volume_info(&self, path: &Path) -> io::Result<VolumeInfo>;

    /// Create a directory junction at `link` pointing to `target`. `link` must not exist.
    fn create_junction(&self, link: &Path, target: &Path) -> io::Result<()>;

    /// `Ok(Some(target))` if `link` is a junction/symlink, `Ok(None)` if it is
    /// anything else (including missing).
    fn read_junction(&self, link: &Path) -> io::Result<Option<PathBuf>>;

    /// Remove the junction itself. Must never touch the target's contents.
    fn remove_junction(&self, link: &Path) -> io::Result<()>;

    /// Rename a directory in place. This doubles as the authoritative
    /// "is anything using this folder?" test.
    fn rename_dir(&self, from: &Path, to: &Path) -> io::Result<()> {
        std::fs::rename(from, to)
    }

    /// Does this error from `rename_dir` mean "something has it open"?
    fn is_in_use_error(&self, _e: &io::Error) -> bool {
        false
    }

    /// Best-effort list of apps using files under `dir`. Informational only.
    fn find_users(&self, _dir: &Path) -> Vec<AppUser> {
        Vec::new()
    }
}
