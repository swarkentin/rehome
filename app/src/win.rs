//! The real Windows implementation of `rehome_core::Platform`.
//!
//! Kept deliberately small: `std` does everything it can (renames, reading links, removing
//! a junction); raw Win32 is used only for what `std` cannot do.

use rehome_core::{AppUser, DriveKind, Platform, VolumeInfo};
use std::ffi::OsStr;
use std::io;
use std::iter::once;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};
use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateDirectoryW, CreateFileW, GetDiskFreeSpaceExW, GetDriveTypeW, GetLogicalDrives,
    GetVolumeInformationW, GetVolumeNameForVolumeMountPointW, GetVolumePathNameW, RemoveDirectoryW,
};
use windows_sys::Win32::System::RestartManager::{
    RmEndSession, RmGetList, RmRegisterResources, RmStartSession, RM_PROCESS_INFO,
};
use windows_sys::Win32::System::IO::DeviceIoControl;

const GENERIC_WRITE: u32 = 0x4000_0000;
const OPEN_EXISTING: u32 = 3;
const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
const FSCTL_SET_REPARSE_POINT: u32 = 0x0009_00A4;
const IO_REPARSE_TAG_MOUNT_POINT: u32 = 0xA000_0003;
const ERROR_MORE_DATA: u32 = 234;
const RM_SERVICE: i32 = 3; // RmService

pub struct Windows;

fn wide(p: &Path) -> Vec<u16> {
    wide_str(p.as_os_str())
}
fn wide_str(s: &OsStr) -> Vec<u16> {
    s.encode_wide().chain(once(0)).collect()
}
fn from_wide(buf: &[u16]) -> String {
    let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..n])
}

impl Platform for Windows {
    fn volume_info(&self, path: &Path) -> io::Result<VolumeInfo> {
        // Use the nearest ancestor that exists (the destination usually doesn't yet).
        let mut p = path;
        while !p.exists() {
            p = p.parent().ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "no existing parent folder")
            })?;
        }
        let w = wide(p);
        unsafe {
            let mut root = [0u16; 261];
            if GetVolumePathNameW(w.as_ptr(), root.as_mut_ptr(), root.len() as u32) == 0 {
                return Err(io::Error::last_os_error());
            }
            let kind = match GetDriveTypeW(root.as_ptr()) {
                3 => DriveKind::Fixed,
                2 => DriveKind::Removable,
                4 => DriveKind::Network,
                5 => DriveKind::Optical,
                6 => DriveKind::Ram,
                _ => DriveKind::Unknown,
            };
            let mut fs = [0u16; 64];
            if GetVolumeInformationW(
                root.as_ptr(),
                null_mut(),
                0,
                null_mut(),
                null_mut(),
                null_mut(),
                fs.as_mut_ptr(),
                fs.len() as u32,
            ) == 0
            {
                return Err(io::Error::last_os_error());
            }
            let mut avail = 0u64;
            let (mut total, mut free) = (0u64, 0u64);
            if GetDiskFreeSpaceExW(root.as_ptr(), &mut avail, &mut total, &mut free) == 0 {
                return Err(io::Error::last_os_error());
            }
            // The volume GUID path identifies the volume regardless of drive letters.
            let mut guid = [0u16; 64];
            let volume_id = if GetVolumeNameForVolumeMountPointW(
                root.as_ptr(),
                guid.as_mut_ptr(),
                guid.len() as u32,
            ) != 0
            {
                from_wide(&guid)
            } else {
                from_wide(&root).to_lowercase()
            };
            Ok(VolumeInfo {
                kind,
                fs: from_wide(&fs),
                free_bytes: avail,
                volume_id,
            })
        }
    }

    fn create_junction(&self, link: &Path, target: &Path) -> io::Result<()> {
        // Substitute name is the NT path (\??\D:\x); print name is the friendly one.
        let print: Vec<u16> = target.as_os_str().encode_wide().collect();
        let mut subst: Vec<u16> = r"\??\".encode_utf16().collect();
        subst.extend(&print);
        let (sl, pl) = ((subst.len() * 2) as u16, (print.len() * 2) as u16);
        let data_len: u16 = 8 + (sl + 2) + (pl + 2);

        let mut buf: Vec<u8> = Vec::with_capacity(8 + data_len as usize);
        buf.extend(IO_REPARSE_TAG_MOUNT_POINT.to_le_bytes());
        buf.extend(data_len.to_le_bytes());
        buf.extend(0u16.to_le_bytes()); // reserved
        buf.extend(0u16.to_le_bytes()); // substitute name offset
        buf.extend(sl.to_le_bytes());
        buf.extend((sl + 2).to_le_bytes()); // print name offset
        buf.extend(pl.to_le_bytes());
        for c in subst
            .iter()
            .chain(once(&0))
            .chain(print.iter())
            .chain(once(&0))
        {
            buf.extend(c.to_le_bytes());
        }

        let wlink = wide(link);
        unsafe {
            if CreateDirectoryW(wlink.as_ptr(), null()) == 0 {
                return Err(io::Error::last_os_error());
            }
            let h = CreateFileW(
                wlink.as_ptr(),
                GENERIC_WRITE,
                0,
                null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
                null_mut(),
            );
            if h == INVALID_HANDLE_VALUE {
                let e = io::Error::last_os_error();
                RemoveDirectoryW(wlink.as_ptr());
                return Err(e);
            }
            let mut returned = 0u32;
            let ok = DeviceIoControl(
                h,
                FSCTL_SET_REPARSE_POINT,
                buf.as_ptr().cast(),
                buf.len() as u32,
                null_mut(),
                0,
                &mut returned,
                null_mut(),
            );
            let err = (ok == 0).then(io::Error::last_os_error);
            CloseHandle(h);
            if let Some(e) = err {
                RemoveDirectoryW(wlink.as_ptr());
                return Err(e);
            }
        }
        Ok(())
    }

    fn read_junction(&self, link: &Path) -> io::Result<Option<PathBuf>> {
        match std::fs::symlink_metadata(link) {
            // std reports junctions and symlinks as "symlinks" and can read both targets.
            Ok(m) if m.file_type().is_symlink() => std::fs::read_link(link).map(Some),
            Ok(_) => Ok(None),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn remove_junction(&self, link: &Path) -> io::Result<()> {
        // Refuse to touch anything that is not a link: remove_dir on a junction removes only
        // the link, but on a real (empty) folder it would remove the folder.
        if self.read_junction(link)?.is_none() {
            return Err(io::Error::other("not a junction; refusing to remove"));
        }
        std::fs::remove_dir(link)
    }

    fn is_in_use_error(&self, e: &io::Error) -> bool {
        // 32 = sharing violation, 5 = access denied (what renaming a folder with open files returns)
        matches!(e.raw_os_error(), Some(32) | Some(5))
    }

    fn find_users(&self, dir: &Path) -> Vec<AppUser> {
        let files = sample_files(dir, 1000);
        if files.is_empty() {
            return Vec::new();
        }
        let wides: Vec<Vec<u16>> = files.iter().map(|p| wide(p)).collect();
        let ptrs: Vec<*const u16> = wides.iter().map(|w| w.as_ptr()).collect();
        let mut out: Vec<AppUser> = Vec::new();
        unsafe {
            let mut session = 0u32;
            let mut key = [0u16; 33];
            if RmStartSession(&mut session, 0, key.as_mut_ptr()) != 0 {
                return out;
            }
            if RmRegisterResources(
                session,
                ptrs.len() as u32,
                ptrs.as_ptr(),
                0,
                null(),
                0,
                null(),
            ) == 0
            {
                let (mut needed, mut count, mut reasons) = (0u32, 0u32, 0u32);
                let r = RmGetList(session, &mut needed, &mut count, null_mut(), &mut reasons);
                if (r == ERROR_MORE_DATA || r == 0) && needed > 0 {
                    let mut info: Vec<RM_PROCESS_INFO> = vec![std::mem::zeroed(); needed as usize];
                    count = needed;
                    if RmGetList(
                        session,
                        &mut needed,
                        &mut count,
                        info.as_mut_ptr(),
                        &mut reasons,
                    ) == 0
                    {
                        for i in info.iter().take(count as usize) {
                            let pid = i.Process.dwProcessId;
                            if out.iter().any(|u| u.pid == Some(pid)) {
                                continue;
                            }
                            let is_service = i.ApplicationType == RM_SERVICE;
                            let name = if is_service {
                                from_wide(&i.strServiceShortName)
                            } else {
                                from_wide(&i.strAppName)
                            };
                            out.push(AppUser {
                                name,
                                pid: Some(pid),
                                is_service,
                            });
                        }
                    }
                }
            }
            RmEndSession(session);
        }
        out
    }
}

/// Up to `limit` files, preferring the kinds apps tend to keep open.
fn sample_files(root: &Path, limit: usize) -> Vec<PathBuf> {
    const HOT: [&str; 12] = [
        "db", "sqlite", "lock", "log", "dll", "exe", "pak", "dat", "ldb", "wal", "json", "bin",
    ];
    let (mut hot, mut rest) = (Vec::new(), Vec::new());
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in rd.flatten() {
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_symlink() {
                continue;
            }
            if ft.is_dir() {
                stack.push(e.path());
            } else if e
                .path()
                .extension()
                .and_then(|x| x.to_str())
                .is_some_and(|x| HOT.contains(&x.to_ascii_lowercase().as_str()))
            {
                hot.push(e.path());
            } else {
                rest.push(e.path());
            }
            if hot.len() >= limit {
                return hot;
            }
            if hot.len() + rest.len() >= limit * 20 {
                stack.clear();
                break;
            }
        }
    }
    hot.extend(rest);
    hot.truncate(limit);
    hot
}

/// Internal drives that could receive a folder, as `("D:\\", info)`.
pub fn fixed_drives() -> Vec<(String, VolumeInfo)> {
    let mask = unsafe { GetLogicalDrives() };
    (0..26u8)
        .filter(|i| mask & (1 << i) != 0)
        .filter_map(|i| {
            let root = format!("{}:\\", (b'A' + i) as char);
            Windows
                .volume_info(Path::new(&root))
                .ok()
                .filter(|v| v.kind == DriveKind::Fixed)
                .map(|v| (root, v))
        })
        .collect()
}
