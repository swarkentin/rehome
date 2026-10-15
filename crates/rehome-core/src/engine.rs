//! The move / undo / recover state machine. See DESIGN.md §4 and §7.
//!
//! The original is only deleted as the very last step, after the junction has been
//! created and read back. Every earlier failure rolls back to the original folder.

use crate::error::{ErrorKind, RehomeError, Result};
use crate::fsops;
use crate::journal::{Journal, Kind, State};
use crate::platform::{AppUser, DriveKind, Platform};
use crate::safety::{self, Finding, Known};
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

const GB: u64 = 1024 * 1024 * 1024;

/// Space needed on the destination: size + 5% + 256 MB.
pub fn required_bytes(bytes: u64) -> u64 {
    bytes + bytes / 20 + 256 * 1024 * 1024
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Copying,
    Verifying,
    SwitchingOver,
    CleaningUp,
}

impl fmt::Display for Step {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Step::Copying => "Copying files",
            Step::Verifying => "Checking the copy",
            Step::SwitchingOver => "Switching over",
            Step::CleaningUp => "Cleaning up",
        })
    }
}

/// Progress and cancellation hooks. All methods are optional.
pub trait Observer {
    fn step(&self, _step: Step) {}
    fn progress(&self, _done_bytes: u64, _total_bytes: u64) {}
    fn cancelled(&self) -> bool {
        false
    }
}

pub struct NoObserver;
impl Observer for NoObserver {}

#[derive(Debug, Clone)]
pub struct Plan {
    pub source: PathBuf,
    pub dest: PathBuf,
    pub scan: fsops::ScanResult,
    pub required_bytes: u64,
    pub dest_free_bytes: u64,
    pub warnings: Vec<Finding>,
    /// Apps that appear to be using the folder. Informational: the rename is the real test.
    pub users: Vec<AppUser>,
}

#[derive(Debug, Clone, Default)]
pub struct Options {
    /// The user has seen and accepted `Plan::warnings`.
    pub accept_warnings: bool,
    /// Keep the renamed original (`<source>.rehome-old`) for the user to delete.
    pub keep_backup: bool,
}

#[derive(Debug)]
pub struct MoveOutcome {
    pub bytes_moved: u64,
    /// A folder that could not be (or was asked not to be) deleted.
    pub leftover: Option<PathBuf>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum RecoverOutcome {
    RolledBack,
    Completed,
}

impl From<Finding> for RehomeError {
    fn from(f: Finding) -> Self {
        RehomeError::new(ErrorKind::Blocked, f.title, f.explanation, f.suggestion)
    }
}

fn sib(p: &Path, suffix: &str) -> PathBuf {
    let mut s = p.as_os_str().to_owned();
    s.push(suffix);
    s.into()
}

fn io_finding(code: &'static str, doing: &str, path: &Path, e: &io::Error) -> Finding {
    let r = RehomeError::io(doing, path, e);
    Finding {
        code,
        title: r.title,
        explanation: r.explanation,
        suggestion: r.suggestion,
    }
}

fn blocked(code: &'static str, title: &str, explanation: String, suggestion: &str) -> Vec<Finding> {
    vec![Finding::new(code, title, explanation, suggestion)]
}

fn is_real_dir(p: &Path) -> bool {
    fs::symlink_metadata(p)
        .map(|m| m.is_dir() && !m.file_type().is_symlink())
        .unwrap_or(false)
}

fn exists(p: &Path) -> bool {
    fs::symlink_metadata(p).is_ok()
}

fn names(users: &[AppUser]) -> String {
    users
        .iter()
        .map(|u| {
            if u.is_service {
                format!("{} (service)", u.name)
            } else {
                u.name.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn in_use_error(path: &Path, users: &[AppUser], e: &io::Error) -> RehomeError {
    let who = if users.is_empty() {
        "Windows didn't say which app.".to_string()
    } else {
        format!("Apps that may be using it: {}.", names(users))
    };
    RehomeError::new(
        ErrorKind::InUse,
        "Can't switch over yet: something is still using this folder.",
        format!(
            "Windows wouldn't let Rehome rename \"{}\" because files inside it are open. {who} \
             (This can also mean your account lacks permission for the folder.)",
            path.display()
        ),
        "Close those apps (check the system tray too, and stop any related services), then try again.",
    )
    .with_details(format!("{e} [{:?}]", e.raw_os_error()))
}

fn stuck(msg: String, j: &Journal, hint: &str) -> RehomeError {
    RehomeError::new(
        ErrorKind::Rollback,
        "Rehome couldn't fully undo the move.",
        format!(
            "{msg} Your files are still on disk. Look in these places: original \"{}\", renamed original \"{}\", relocated copy \"{}\", temporary copy \"{}\".",
            j.source.display(),
            j.old.display(),
            j.dest.display(),
            j.partial.display()
        ),
        hint,
    )
    .data_at_risk()
}

pub struct Engine<'a, P: Platform> {
    p: &'a P,
    known: Known,
    journal_dir: PathBuf,
}

impl<'a, P: Platform> Engine<'a, P> {
    pub fn new(platform: &'a P, known: Known, journal_dir: impl Into<PathBuf>) -> Self {
        Engine {
            p: platform,
            known,
            journal_dir: journal_dir.into(),
        }
    }

    // ---------------------------------------------------------------- check

    /// Everything that can be decided without changing anything on disk.
    /// `Err` holds the reasons the move is refused.
    pub fn check(
        &self,
        source: &Path,
        dest: &Path,
        obs: &dyn Observer,
    ) -> std::result::Result<Plan, Vec<Finding>> {
        let (s, d) = (source.to_string_lossy(), dest.to_string_lossy());

        let v = safety::check_source(&s, &self.known);
        if !v.blocked.is_empty() {
            return Err(v.blocked);
        }
        let mut warnings = v.warnings;
        let rel = safety::check_relation(&s, &d);
        if !rel.blocked.is_empty() {
            return Err(rel.blocked);
        }
        if !source.is_absolute() || !dest.is_absolute() {
            return Err(blocked(
                "relative_path",
                "Please give full paths.",
                "Both the folder and its destination must be complete paths such as D:\\Games\\Cache.".into(),
                "Use the full path, including the drive letter.",
            ));
        }

        if let Ok(Some(target)) = self.p.read_junction(source) {
            return Err(blocked(
                "already_link",
                "This folder has already been moved (or is a shortcut).",
                format!(
                    "\"{s}\" is a shortcut folder that points to \"{}\".",
                    target.display()
                ),
                "To bring the files back, use \"Move back\" instead.",
            ));
        }
        match fs::symlink_metadata(source) {
            Ok(m) if m.is_dir() => {}
            Ok(_) => {
                return Err(blocked(
                    "not_a_folder",
                    "That isn't a folder.",
                    format!("\"{s}\" is a file."),
                    "Choose a folder.",
                ))
            }
            Err(e) => {
                return Err(vec![io_finding(
                    "cannot_open",
                    "open the folder",
                    source,
                    &e,
                )])
            }
        }

        if exists(dest) {
            return Err(blocked(
                "dest_exists",
                "The destination already exists.",
                format!("\"{d}\" is already there, and Rehome won't merge into or overwrite it."),
                "Choose a different destination, or remove the existing folder.",
            ));
        }
        for (p, what) in [
            (sib(dest, ".rehome-partial"), "partial copy"),
            (sib(source, ".rehome-old"), "old copy"),
        ] {
            if exists(&p) {
                return Err(blocked(
                    "leftover",
                    "A leftover from an earlier attempt is in the way.",
                    format!("Found a {what} at \"{}\".", p.display()),
                    "Open Rehome's recovery (or check the folder yourself), then delete it if you're sure it's not needed.",
                ));
            }
        }

        // Volumes
        let sv = self
            .p
            .volume_info(source)
            .map_err(|e| vec![io_finding("volume", "inspect the source drive", source, &e)])?;
        let dv = self.p.volume_info(dest).map_err(|e| {
            vec![io_finding(
                "volume",
                "inspect the destination drive",
                dest,
                &e,
            )]
        })?;
        if dv.kind != DriveKind::Fixed {
            let what = match dv.kind {
                DriveKind::Removable => "removable (USB or card) drive",
                DriveKind::Network => "network drive",
                DriveKind::Optical => "disc drive",
                DriveKind::Ram => "RAM disk",
                _ => "drive of an unknown type",
            };
            return Err(blocked(
                "dest_not_fixed",
                "Choose a drive that's built into this PC.",
                format!("The destination is a {what}. If it's unplugged or unavailable, apps would find a broken folder."),
                "Pick an internal drive (such as D:).",
            ));
        }
        if sv.volume_id == dv.volume_id {
            return Err(blocked(
                "same_volume",
                "That's the same drive.",
                "Moving to the same drive wouldn't free up any space.".into(),
                "Choose a different drive as the destination.",
            ));
        }
        let fs_name = dv.fs.to_ascii_lowercase();
        if fs_name == "fat32" || fs_name == "fat" {
            return Err(blocked(
                "dest_fat32",
                "That drive's format can't hold these files safely.",
                format!("The destination drive uses {}, which has no permissions and a 4 GB file size limit.", dv.fs),
                "Choose an NTFS drive.",
            ));
        }
        if fs_name == "exfat" {
            warnings.push(Finding::new(
                "dest_exfat",
                "The destination drive uses exFAT.",
                "exFAT can't store file permissions. Fine for game caches; not recommended for data that needs them.".into(),
                "Prefer an NTFS drive if you have one.",
            ));
        }

        // Contents
        let scan = fsops::scan(source, &|| obs.cancelled()).map_err(|e| {
            if fsops::is_cancel(&e) {
                blocked(
                    "cancelled",
                    "Cancelled.",
                    "The size check was stopped.".into(),
                    "Start again when ready.",
                )
            } else {
                vec![io_finding("scan", "read the folder's contents", source, &e)]
            }
        })?;
        if scan.has_marker {
            return Err(blocked(
                "has_marker",
                "This folder already contains Rehome data.",
                format!("\"{s}\" has a \".rehome\" folder at its top level."),
                "If it came from an earlier move, use \"Move back\" on the original location.",
            ));
        }
        if !scan.links.is_empty() {
            let list: Vec<String> = scan
                .links
                .iter()
                .take(3)
                .map(|p| format!("\"{}\"", p.display()))
                .collect();
            return Err(blocked(
                "contains_links",
                "This folder contains shortcut links to other places.",
                format!("Found {} (symbolic links or junctions). Copying them could duplicate or break data.", list.join(", ")),
                "Choose a folder without links, or move its subfolders one by one.",
            ));
        }
        if scan.placeholders > 0 {
            return Err(blocked(
                "cloud_files",
                "Some files are stored in the cloud, not on this PC.",
                format!("{} cloud placeholder files (such as OneDrive) are inside. Copying would download them all, and the sync app may fight the change.", scan.placeholders),
                "Make the folder \"Always keep on this device\", or pick a folder that isn't synced.",
            ));
        }
        if scan.encrypted > 0 {
            warnings.push(Finding::new(
                "encrypted",
                "Some files are encrypted (EFS).",
                format!("{} encrypted files are inside. They copy only if your encryption key is available.", scan.encrypted),
                "Continue only if you're signed in as the account that owns them.",
            ));
        }
        let required = required_bytes(scan.bytes);
        if required > dv.free_bytes {
            return Err(blocked(
                "no_space",
                "Not enough room on the destination drive.",
                format!(
                    "The move needs about {} but only {} is free.",
                    human(required),
                    human(dv.free_bytes)
                ),
                &format!(
                    "Free up {} on the destination or choose a drive with more space.",
                    human(required - dv.free_bytes)
                ),
            ));
        }
        if scan.bytes > 100 * GB {
            warnings.push(Finding::new(
                "very_large",
                "This is a very large folder.",
                format!(
                    "{} will be copied. This can take a long time.",
                    human(scan.bytes)
                ),
                "You can cancel safely at any time before the final switch.",
            ));
        }

        let users = self.p.find_users(source);
        if users.iter().any(|u| u.is_service) {
            warnings.push(Finding::new(
                "service_in_use",
                "A background service is using this folder.",
                format!("Detected: {}.", names(&users)),
                "Stop the service first; otherwise the switch-over will likely be refused.",
            ));
        }

        Ok(Plan {
            source: source.into(),
            dest: dest.into(),
            required_bytes: required,
            dest_free_bytes: dv.free_bytes,
            scan,
            warnings,
            users,
        })
    }

    // ----------------------------------------------------------------- move

    pub fn move_folder(
        &self,
        plan: &Plan,
        opts: &Options,
        obs: &dyn Observer,
    ) -> Result<MoveOutcome> {
        if !plan.warnings.is_empty() && !opts.accept_warnings {
            return Err(RehomeError::new(
                ErrorKind::NeedsConfirmation,
                "Please confirm the warnings first.",
                plan.warnings
                    .iter()
                    .map(|w| w.title.clone())
                    .collect::<Vec<_>>()
                    .join(" "),
                "Review the warnings and confirm to continue.",
            ));
        }
        let partial = sib(&plan.dest, ".rehome-partial");
        let old = sib(&plan.source, ".rehome-old");
        for p in [&partial, &old] {
            if exists(p) {
                return Err(RehomeError::new(
                    ErrorKind::Blocked,
                    "A leftover from an earlier attempt is in the way.",
                    format!("\"{}\" already exists.", p.display()),
                    "Run recovery, or delete it if you're sure it's not needed.",
                ));
            }
        }
        if let Some(parent) = plan.dest.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| RehomeError::io("create the destination folder", parent, &e))?;
        }

        let mut j = Journal::new(
            &self.journal_dir,
            Kind::Move,
            &plan.source,
            &plan.dest,
            &partial,
            &old,
            plan.scan.bytes,
        );
        j.keep_backup = opts.keep_backup;
        j.save()?;

        // 1. Copy (the original is untouched).
        obs.step(Step::Copying);
        j.set_state(State::Copying)
            .map_err(|e| self.abandon(&j, e))?;
        let total = plan.scan.bytes;
        let r = fsops::copy_tree(&plan.source, &partial, &mut |done| {
            obs.progress(done, total);
            !obs.cancelled()
        });
        if let Err(e) = r {
            let err = if fsops::is_cancel(&e) {
                RehomeError::cancelled()
            } else {
                RehomeError::io("copy the files", &plan.dest, &e)
            };
            return Err(self.abandon(&j, err));
        }
        j.state = State::Copied;

        // 2. Verify.
        obs.step(Step::Verifying);
        let d = fsops::diff(&plan.source, &partial).map_err(|e| {
            self.abandon(
                &j,
                RehomeError::io("compare the copy with the original", &partial, &e),
            )
        })?;
        if let Some(first) = d.first() {
            return Err(self.abandon(
                &j,
                RehomeError::new(
                    ErrorKind::VerifyFailed,
                    "The copy didn't match the original.",
                    format!(
                        "{} item(s) differ, starting with \"{}\". Something may have changed the folder while it was being copied.",
                        d.len(),
                        first.path().display()
                    ),
                    "Close apps using the folder and try again.",
                ),
            ));
        }
        j.set_state(State::Verified)
            .map_err(|e| self.abandon(&j, e))?;

        // 3. Atomic in-use test: rename the original out of the way.
        obs.step(Step::SwitchingOver);
        if obs.cancelled() {
            return Err(self.abandon(&j, RehomeError::cancelled()));
        }
        if let Err(e) = self.p.rename_dir(&plan.source, &old) {
            let err = if self.p.is_in_use_error(&e) {
                in_use_error(&plan.source, &self.p.find_users(&plan.source), &e)
            } else {
                RehomeError::io("rename the original folder", &plan.source, &e)
            };
            return Err(self.abandon(&j, err));
        }
        j.set_state(State::SourceRenamed)
            .map_err(|e| self.after(&mut j, e))?;

        // 4. Nothing can be writing now. Pick up anything that changed since the verify.
        let ds = fsops::diff(&old, &partial)
            .map_err(|e| RehomeError::io("re-check the copy", &partial, &e));
        let ds = self.post(&mut j, ds)?;
        if !ds.is_empty() {
            let r = fsops::apply_diff(&old, &partial, &ds)
                .map_err(|e| RehomeError::io("update the copy", &partial, &e));
            self.post(&mut j, r)?;
            let again = fsops::diff(&old, &partial)
                .map_err(|e| RehomeError::io("re-check the copy", &partial, &e));
            if !self.post(&mut j, again)?.is_empty() {
                let e = RehomeError::new(
                    ErrorKind::VerifyFailed,
                    "The copy didn't match the original.",
                    "Files still differed after a final update.",
                    "Try again.",
                );
                return Err(self.after(&mut j, e));
            }
        }
        j.set_state(State::DeltaSynced)
            .map_err(|e| self.after(&mut j, e))?;

        let r = self.write_marker(&partial, &plan.source, plan.scan.bytes);
        self.post(&mut j, r)?;

        // 5. Publish the copy under its final name.
        let r = self
            .p
            .rename_dir(&partial, &plan.dest)
            .map_err(|e| RehomeError::io("finish placing the copy", &plan.dest, &e));
        self.post(&mut j, r)?;
        j.set_state(State::DestPublished)
            .map_err(|e| self.after(&mut j, e))?;

        // 6. Junction, read back.
        let r = self
            .p
            .create_junction(&plan.source, &plan.dest)
            .map_err(|e| RehomeError::io("create the shortcut folder", &plan.source, &e));
        self.post(&mut j, r)?;
        let ok = matches!(self.p.read_junction(&plan.source), Ok(Some(t)) if safety::is_same(&t.to_string_lossy(), &plan.dest.to_string_lossy()))
            && fs::read_dir(&plan.source).is_ok();
        if !ok {
            let e = RehomeError::new(
                ErrorKind::VerifyFailed,
                "The shortcut folder didn't work.",
                format!("After creating it, \"{}\" couldn't be opened or didn't point to the new location.", plan.source.display()),
                "Nothing was changed. Try again, and report this if it repeats.",
            );
            return Err(self.after(&mut j, e));
        }
        j.set_state(State::JunctionMade)
            .map_err(|e| self.after(&mut j, e))?;

        // 7. The only irreversible step: delete the renamed original.
        obs.step(Step::CleaningUp);
        let mut leftover = None;
        if opts.keep_backup {
            leftover = Some(old.clone());
            j.discard();
        } else if fsops::remove_tree(&old).is_ok() {
            j.discard();
        } else {
            // Journal stays; recovery will retry the delete on next launch.
            leftover = Some(old.clone());
        }
        Ok(MoveOutcome {
            bytes_moved: plan.scan.bytes,
            leftover,
        })
    }

    fn write_marker(&self, tree: &Path, origin: &Path, bytes: u64) -> Result<()> {
        let dir = tree.join(fsops::MARKER_DIR);
        let when = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        fs::create_dir(&dir)
            .and_then(|_| {
                fs::write(
                    dir.join("origin.txt"),
                    format!(
                        "version=1\norigin={}\nbytes={bytes}\nmoved_unix={when}\n",
                        origin.display()
                    ),
                )
            })
            .map_err(|e| RehomeError::io("record where the folder came from", &dir, &e))
    }

    /// Before the original was renamed: remove the partial copy and finish. The original is untouched.
    fn abandon(&self, j: &Journal, mut err: RehomeError) -> RehomeError {
        match fsops::remove_tree(&j.partial) {
            Ok(()) => j.discard(),
            Err(e) => {
                err.explanation.push_str(&format!(
                    " A partial copy was left at \"{}\" and can be deleted ({e}).",
                    j.partial.display()
                ));
            }
        }
        err
    }

    /// After the original was renamed: undo everything and report the original error.
    fn after(&self, j: &mut Journal, mut err: RehomeError) -> RehomeError {
        match self.rollback_move(j) {
            Ok(()) => {
                err.explanation
                    .push_str(" Rehome undid the move and your original folder is back in place.");
                err
            }
            Err(rb) => rb.with_details(format!("original problem: {}", err.explanation)),
        }
    }

    fn post<T>(&self, j: &mut Journal, r: Result<T>) -> Result<T> {
        r.map_err(|e| self.after(j, e))
    }

    /// Observation-driven and idempotent: safe to call again after a crash.
    fn rollback_move(&self, j: &mut Journal) -> Result<()> {
        let hint =
            "Check those folders, then copy anything you need into the original location by hand.";
        match self.p.read_junction(&j.source) {
            Ok(Some(t)) if safety::is_same(&t.to_string_lossy(), &j.dest.to_string_lossy()) => {
                self.p.remove_junction(&j.source).map_err(|e| {
                    stuck(
                        format!("Couldn't remove the shortcut folder ({e})."),
                        j,
                        hint,
                    )
                })?;
            }
            Ok(Some(t)) => {
                return Err(stuck(
                    format!(
                    "The shortcut at the original location points somewhere unexpected (\"{}\").",
                    t.display()
                ),
                    j,
                    hint,
                ))
            }
            Ok(None) => {}
            Err(e) => {
                return Err(stuck(
                    format!("Couldn't inspect the original location ({e})."),
                    j,
                    hint,
                ))
            }
        }
        if exists(&j.old) {
            if exists(&j.source) {
                return Err(stuck(
                    "Both the original location and its renamed copy exist.".into(),
                    j,
                    hint,
                ));
            }
            self.p.rename_dir(&j.old, &j.source).map_err(|e| {
                stuck(
                    format!("Couldn't rename the original folder back ({e})."),
                    j,
                    hint,
                )
            })?;
        }
        if !is_real_dir(&j.source) {
            return Err(stuck(
                "Couldn't confirm the original folder is in place, so nothing was deleted.".into(),
                j,
                hint,
            ));
        }
        // The original is back: our copies are disposable.
        let mut leftovers = Vec::new();
        if fsops::remove_tree(&j.partial).is_err() {
            leftovers.push(j.partial.clone());
        }
        if j.state >= State::DestPublished && fsops::remove_tree(&j.dest).is_err() {
            leftovers.push(j.dest.clone());
        }
        if leftovers.is_empty() {
            j.discard();
            Ok(())
        } else {
            let list: Vec<String> = leftovers
                .iter()
                .map(|p| format!("\"{}\"", p.display()))
                .collect();
            Err(RehomeError::new(
                ErrorKind::Rollback,
                "Your original folder is back, but a temporary copy is left over.",
                format!("Rehome couldn't delete {}.", list.join(" and ")),
                "You can delete it yourself. Rehome will also retry next time it starts.",
            ))
        }
    }

    // ----------------------------------------------------------------- undo

    /// "Move back": copy the data home, replace the junction with the real folder.
    pub fn restore(&self, source: &Path, obs: &dyn Observer) -> Result<MoveOutcome> {
        let blocked_err =
            |t: &str, e: String, s: &str| RehomeError::new(ErrorKind::Blocked, t, e, s);
        let dest = match self.p.read_junction(source) {
            Ok(Some(t)) => t,
            Ok(None) => {
                return Err(blocked_err(
                    "This isn't a moved folder.",
                    format!(
                        "\"{}\" is a normal folder, not a shortcut created by Rehome.",
                        source.display()
                    ),
                    "Choose the shortcut folder at the original location.",
                ))
            }
            Err(e) => return Err(RehomeError::io("inspect the folder", source, &e)),
        };
        let marker =
            fs::read_to_string(dest.join(fsops::MARKER_DIR).join("origin.txt")).unwrap_or_default();
        let origin_ok = marker
            .lines()
            .find_map(|l| l.strip_prefix("origin="))
            .is_some_and(|o| safety::is_same(o, &source.to_string_lossy()));
        if !origin_ok {
            return Err(blocked_err(
                "Rehome didn't create this shortcut.",
                format!(
                    "\"{}\" points to \"{}\", which has no Rehome record for this location.",
                    source.display(),
                    dest.display()
                ),
                "Only folders moved with Rehome can be moved back automatically.",
            ));
        }
        let scan = fsops::scan(&dest, &|| obs.cancelled()).map_err(|e| {
            if fsops::is_cancel(&e) {
                RehomeError::cancelled()
            } else {
                RehomeError::io("read the moved folder", &dest, &e)
            }
        })?;
        if !scan.links.is_empty() {
            return Err(blocked_err(
                "The moved folder now contains links.",
                format!("Found shortcut links inside \"{}\".", dest.display()),
                "Remove them, or move the data back by hand.",
            ));
        }
        let home = source.parent().unwrap_or(source);
        let sv = self
            .p
            .volume_info(home)
            .map_err(|e| RehomeError::io("inspect the original drive", home, &e))?;
        let required = required_bytes(scan.bytes);
        if required > sv.free_bytes {
            return Err(RehomeError::new(
                ErrorKind::NoSpace,
                "Not enough room to move this back.",
                format!(
                    "Moving back needs about {} but only {} is free on the original drive.",
                    human(required),
                    human(sv.free_bytes)
                ),
                format!(
                    "Free up {} on that drive first.",
                    human(required - sv.free_bytes)
                ),
            ));
        }
        let restoring = sib(source, ".rehome-restoring");
        if exists(&restoring) {
            return Err(blocked_err(
                "A leftover from an earlier attempt is in the way.",
                format!("\"{}\" already exists.", restoring.display()),
                "Run recovery, or delete it if you're sure it's not needed.",
            ));
        }

        let mut j = Journal::new(
            &self.journal_dir,
            Kind::Restore,
            source,
            &dest,
            &restoring,
            Path::new(""),
            scan.bytes,
        );
        j.save()?;
        obs.step(Step::Copying);
        j.state = State::Copying;
        let total = scan.bytes;
        let r = fsops::copy_tree(&dest, &restoring, &mut |done| {
            obs.progress(done, total);
            !obs.cancelled()
        });
        if let Err(e) = r {
            let err = if fsops::is_cancel(&e) {
                RehomeError::cancelled()
            } else {
                RehomeError::io("copy the files back", source, &e)
            };
            return Err(self.abandon(&j, err));
        }
        obs.step(Step::Verifying);
        let d = fsops::diff(&dest, &restoring)
            .map_err(|e| self.abandon(&j, RehomeError::io("check the copy", &restoring, &e)))?;
        if !d.is_empty() {
            return Err(self.abandon(
                &j,
                RehomeError::new(
                    ErrorKind::VerifyFailed,
                    "The copy didn't match.",
                    format!(
                        "{} item(s) differ. The moved folder may have changed during the copy.",
                        d.len()
                    ),
                    "Close apps using the folder and try again.",
                ),
            ));
        }
        j.state = State::Verified;

        obs.step(Step::SwitchingOver);
        if let Err(e) = self.p.remove_junction(source) {
            return Err(self.abandon(
                &j,
                RehomeError::io("remove the shortcut folder", source, &e),
            ));
        }
        j.set_state(State::LinkRemoved).ok();
        if let Err(e) = self.p.rename_dir(&restoring, source) {
            // Put the shortcut back so apps keep working, then clean up.
            let relinked = self.p.create_junction(source, &dest).is_ok();
            let mut err = RehomeError::io("put the folder back", source, &e);
            if !relinked {
                err = stuck("The shortcut was removed and couldn't be recreated.".into(), &j, "Create a junction from the original path to the relocated folder, or rename the temporary copy back.");
                return Err(err);
            }
            return Err(self.abandon(&j, err));
        }

        obs.step(Step::CleaningUp);
        let mut leftover = None;
        if fsops::remove_tree(&dest).is_ok() {
            j.discard();
        } else {
            leftover = Some(dest.clone());
        }
        Ok(MoveOutcome {
            bytes_moved: scan.bytes,
            leftover,
        })
    }

    // -------------------------------------------------------------- recover

    /// Unfinished operations from an earlier run (crash, power loss, killed process).
    pub fn pending(&self) -> Vec<Journal> {
        Journal::pending(&self.journal_dir)
    }

    /// Bring an unfinished operation to a safe end: roll back before the shortcut
    /// existed, or finish cleaning up once it did.
    pub fn recover(&self, j: &mut Journal) -> Result<RecoverOutcome> {
        match j.kind {
            Kind::Move if j.state == State::JunctionMade => {
                let ok = matches!(self.p.read_junction(&j.source), Ok(Some(t)) if safety::is_same(&t.to_string_lossy(), &j.dest.to_string_lossy()));
                if !ok {
                    return self.rollback_move(j).map(|_| RecoverOutcome::RolledBack);
                }
                if !j.keep_backup {
                    fsops::remove_tree(&j.old)
                        .map_err(|e| RehomeError::io("delete the old copy", &j.old, &e))?;
                }
                j.discard();
                Ok(RecoverOutcome::Completed)
            }
            Kind::Move => self.rollback_move(j).map(|_| RecoverOutcome::RolledBack),
            Kind::Restore => self.recover_restore(j),
        }
    }

    fn recover_restore(&self, j: &mut Journal) -> Result<RecoverOutcome> {
        let hint = "Check those folders and copy anything you need by hand.";
        match self.p.read_junction(&j.source) {
            Ok(Some(t)) if safety::is_same(&t.to_string_lossy(), &j.dest.to_string_lossy()) => {
                fsops::remove_tree(&j.partial)
                    .map_err(|e| RehomeError::io("delete the temporary copy", &j.partial, &e))?;
                j.discard();
                Ok(RecoverOutcome::RolledBack)
            }
            Ok(Some(_)) => Err(stuck(
                "The shortcut points somewhere unexpected.".into(),
                j,
                hint,
            )),
            Ok(None) => {
                if !exists(&j.source) && exists(&j.partial) {
                    self.p.rename_dir(&j.partial, &j.source).map_err(|e| {
                        stuck(
                            format!("Couldn't finish moving the folder back ({e})."),
                            j,
                            hint,
                        )
                    })?;
                }
                if !is_real_dir(&j.source) || exists(&j.partial) {
                    return Err(stuck(
                        "Couldn't confirm the folder is back in place.".into(),
                        j,
                        hint,
                    ));
                }
                fsops::remove_tree(&j.dest)
                    .map_err(|e| RehomeError::io("delete the relocated copy", &j.dest, &e))?;
                j.discard();
                Ok(RecoverOutcome::Completed)
            }
            Err(e) => Err(stuck(
                format!("Couldn't inspect the original location ({e})."),
                j,
                hint,
            )),
        }
    }
}

/// "12.3 GB" style sizes.
pub fn human(b: u64) -> String {
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{b} B")
    } else {
        format!("{v:.1} {}", U[i])
    }
}
