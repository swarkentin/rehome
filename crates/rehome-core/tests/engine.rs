//! Engine tests against an emulated platform (symlinks stand in for junctions).
#![cfg(unix)]

use rehome_core::journal::{Journal, Kind, State};
use rehome_core::safety::Known;
use rehome_core::*;
use std::cell::{Cell, RefCell};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

struct Fake {
    dest_root: PathBuf,
    dest_free: u64,
    dest_kind: DriveKind,
    dest_fs: &'static str,
    fail_rename_source: Cell<bool>,
    fail_junction: Cell<bool>,
    before_rename_source: RefCell<Option<Box<dyn Fn()>>>,
    users: Vec<AppUser>,
}

impl Fake {
    fn new(dest_root: &Path) -> Fake {
        Fake {
            dest_root: dest_root.into(),
            dest_free: 1 << 40,
            dest_kind: DriveKind::Fixed,
            dest_fs: "NTFS",
            fail_rename_source: Cell::new(false),
            fail_junction: Cell::new(false),
            before_rename_source: RefCell::new(None),
            users: vec![],
        }
    }
}

impl Platform for Fake {
    fn volume_info(&self, path: &Path) -> io::Result<VolumeInfo> {
        Ok(if path.starts_with(&self.dest_root) {
            VolumeInfo {
                kind: self.dest_kind,
                fs: self.dest_fs.into(),
                free_bytes: self.dest_free,
                volume_id: "dest".into(),
            }
        } else {
            VolumeInfo {
                kind: DriveKind::Fixed,
                fs: "NTFS".into(),
                free_bytes: 1 << 40,
                volume_id: "src".into(),
            }
        })
    }
    fn create_junction(&self, link: &Path, target: &Path) -> io::Result<()> {
        if self.fail_junction.get() {
            return Err(io::Error::other("injected junction failure"));
        }
        std::os::unix::fs::symlink(target, link)
    }
    fn read_junction(&self, link: &Path) -> io::Result<Option<PathBuf>> {
        match fs::symlink_metadata(link) {
            Ok(m) if m.file_type().is_symlink() => Ok(Some(fs::read_link(link)?)),
            _ => Ok(None),
        }
    }
    fn remove_junction(&self, link: &Path) -> io::Result<()> {
        fs::remove_file(link)
    }
    fn rename_dir(&self, from: &Path, to: &Path) -> io::Result<()> {
        if to.to_string_lossy().ends_with(".rehome-old") {
            if let Some(h) = self.before_rename_source.borrow().as_ref() {
                h();
            }
            if self.fail_rename_source.get() {
                return Err(io::Error::from_raw_os_error(32));
            }
        }
        fs::rename(from, to)
    }
    fn is_in_use_error(&self, e: &io::Error) -> bool {
        e.raw_os_error() == Some(32)
    }
    fn find_users(&self, _: &Path) -> Vec<AppUser> {
        self.users.clone()
    }
}

struct Env {
    root: PathBuf,
    src: PathBuf,
    dest: PathBuf,
    journals: PathBuf,
}

fn env(name: &str) -> Env {
    let root = std::env::temp_dir().join(format!("rehome-engine-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let src = root.join("C/Users/sam/AppData/Local/Game");
    fs::create_dir_all(src.join("cache/deep")).unwrap();
    fs::write(src.join("a.txt"), "alpha").unwrap();
    fs::write(src.join("cache/deep/b.bin"), vec![1u8; 10_000]).unwrap();
    fs::create_dir_all(src.join("empty")).unwrap();
    Env {
        dest: root.join("D/Rehome/Game"),
        journals: root.join("journals"),
        root,
        src,
    }
}

fn engine<'a>(f: &'a Fake, e: &Env) -> Engine<'a, Fake> {
    Engine::new(f, Known::typical("sam"), &e.journals)
}

fn plan(eng: &Engine<Fake>, e: &Env) -> Plan {
    eng.check(&e.src, &e.dest, &NoObserver)
        .expect("plan should be allowed")
}

fn no_journals(e: &Env) -> bool {
    Journal::pending(&e.journals).is_empty()
}

fn assert_original_intact(e: &Env) {
    assert!(
        fs::symlink_metadata(&e.src).unwrap().is_dir(),
        "source must be a real directory"
    );
    assert!(!fs::symlink_metadata(&e.src)
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(fs::read_to_string(e.src.join("a.txt")).unwrap(), "alpha");
    assert_eq!(
        fs::read(e.src.join("cache/deep/b.bin")).unwrap().len(),
        10_000
    );
    assert!(e.src.join("empty").is_dir());
}

fn leftovers(e: &Env) -> Vec<&'static str> {
    let mut v = vec![];
    if e.dest.exists() {
        v.push("dest")
    }
    if PathBuf::from(format!("{}.rehome-partial", e.dest.display())).exists() {
        v.push("partial")
    }
    if PathBuf::from(format!("{}.rehome-old", e.src.display())).exists() {
        v.push("old")
    }
    v
}

#[test]
fn happy_path_moves_and_links() {
    let e = env("happy");
    let f = Fake::new(&e.root.join("D"));
    let eng = engine(&f, &e);
    let p = plan(&eng, &e);
    assert_eq!(p.scan.files, 2);
    let out = eng
        .move_folder(&p, &Options::default(), &NoObserver)
        .unwrap();
    assert!(out.leftover.is_none());
    // Looks like nothing moved:
    assert_eq!(fs::read_to_string(e.src.join("a.txt")).unwrap(), "alpha");
    assert_eq!(fs::read_link(&e.src).unwrap(), e.dest);
    // Data really is on the other drive, with a marker:
    assert_eq!(
        fs::read(e.dest.join("cache/deep/b.bin")).unwrap().len(),
        10_000
    );
    let marker = fs::read_to_string(e.dest.join(".rehome/origin.txt")).unwrap();
    assert!(marker.contains(&format!("origin={}", e.src.display())));
    assert_eq!(leftovers(&e), vec!["dest"]);
    assert!(no_journals(&e));
}

#[test]
fn keep_backup_leaves_old_folder() {
    let e = env("keep");
    let f = Fake::new(&e.root.join("D"));
    let eng = engine(&f, &e);
    let p = plan(&eng, &e);
    let out = eng
        .move_folder(
            &p,
            &Options {
                keep_backup: true,
                ..Default::default()
            },
            &NoObserver,
        )
        .unwrap();
    let old = out.leftover.unwrap();
    assert!(old.join("a.txt").exists());
    assert!(no_journals(&e));
}

#[test]
fn in_use_folder_is_refused_and_untouched() {
    let e = env("inuse");
    let mut f = Fake::new(&e.root.join("D"));
    f.users = vec![AppUser {
        name: "SomeGame Launcher".into(),
        pid: Some(7),
        is_service: false,
    }];
    f.fail_rename_source.set(true);
    let eng = engine(&f, &e);
    let p = plan(&eng, &e);
    let err = eng
        .move_folder(&p, &Options::default(), &NoObserver)
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::InUse);
    assert!(err.explanation.contains("SomeGame Launcher"), "{err}");
    assert!(err.data_safe);
    assert_original_intact(&e);
    assert!(leftovers(&e).is_empty(), "{:?}", leftovers(&e));
    assert!(no_journals(&e));
    // Once the app is closed, the same plan works.
    f.fail_rename_source.set(false);
    eng.move_folder(&p, &Options::default(), &NoObserver)
        .unwrap();
}

#[test]
fn junction_failure_rolls_everything_back() {
    let e = env("junction");
    let f = Fake::new(&e.root.join("D"));
    f.fail_junction.set(true);
    let eng = engine(&f, &e);
    let p = plan(&eng, &e);
    let err = eng
        .move_folder(&p, &Options::default(), &NoObserver)
        .unwrap_err();
    assert!(err.explanation.contains("undid the move"), "{err}");
    assert_original_intact(&e);
    assert!(leftovers(&e).is_empty(), "{:?}", leftovers(&e));
    assert!(no_journals(&e));
}

struct VerifyHook(PathBuf);
impl Observer for VerifyHook {
    fn step(&self, s: Step) {
        if s == Step::Verifying {
            fs::write(self.0.join("sneaky.txt"), "appeared during copy").unwrap();
        }
    }
}

#[test]
fn change_during_copy_fails_verification_cleanly() {
    let e = env("verify");
    let f = Fake::new(&e.root.join("D"));
    let eng = engine(&f, &e);
    let p = plan(&eng, &e);
    let err = eng
        .move_folder(&p, &Options::default(), &VerifyHook(e.src.clone()))
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::VerifyFailed);
    assert!(e.src.join("a.txt").exists() && e.src.join("sneaky.txt").exists());
    assert!(!e.src.symlink_metadata().unwrap().file_type().is_symlink());
    assert!(leftovers(&e).is_empty(), "{:?}", leftovers(&e));
    assert!(no_journals(&e));
}

#[test]
fn writes_between_verify_and_switch_are_not_lost() {
    let e = env("delta");
    let f = Fake::new(&e.root.join("D"));
    let src = e.src.clone();
    *f.before_rename_source.borrow_mut() = Some(Box::new(move || {
        fs::write(src.join("a.txt"), "alpha, edited just before the switch").unwrap();
        fs::write(src.join("new.txt"), "brand new").unwrap();
        fs::remove_dir_all(src.join("cache")).unwrap();
    }));
    let eng = engine(&f, &e);
    let p = plan(&eng, &e);
    eng.move_folder(&p, &Options::default(), &NoObserver)
        .unwrap();
    assert_eq!(
        fs::read_to_string(e.dest.join("a.txt")).unwrap(),
        "alpha, edited just before the switch"
    );
    assert_eq!(
        fs::read_to_string(e.dest.join("new.txt")).unwrap(),
        "brand new"
    );
    assert!(!e.dest.join("cache").exists());
}

struct CancelAfterCopy(Cell<bool>);
impl Observer for CancelAfterCopy {
    fn step(&self, s: Step) {
        if s == Step::SwitchingOver {
            self.0.set(true);
        }
    }
    fn cancelled(&self) -> bool {
        self.0.get()
    }
}

#[test]
fn cancel_before_switch_leaves_no_trace() {
    let e = env("cancel");
    let f = Fake::new(&e.root.join("D"));
    let eng = engine(&f, &e);
    let p = plan(&eng, &e);
    let err = eng
        .move_folder(&p, &Options::default(), &CancelAfterCopy(Cell::new(false)))
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Cancelled);
    assert_original_intact(&e);
    assert!(leftovers(&e).is_empty());
    assert!(no_journals(&e));
}

#[test]
fn restore_brings_data_home() {
    let e = env("restore");
    let f = Fake::new(&e.root.join("D"));
    let eng = engine(&f, &e);
    let p = plan(&eng, &e);
    eng.move_folder(&p, &Options::default(), &NoObserver)
        .unwrap();
    fs::write(e.dest.join("made-after-move.txt"), "keep me").unwrap();
    eng.restore(&e.src, &NoObserver).unwrap();
    assert!(!e.src.symlink_metadata().unwrap().file_type().is_symlink());
    assert_eq!(
        fs::read_to_string(e.src.join("made-after-move.txt")).unwrap(),
        "keep me"
    );
    assert_eq!(fs::read_to_string(e.src.join("a.txt")).unwrap(), "alpha");
    assert!(!e.src.join(".rehome").exists());
    assert!(!e.dest.exists());
    assert!(no_journals(&e));
}

#[test]
fn restore_refuses_foreign_links() {
    let e = env("foreign");
    let f = Fake::new(&e.root.join("D"));
    let eng = engine(&f, &e);
    let other = e.root.join("other");
    fs::create_dir_all(&other).unwrap();
    let link = e.root.join("link");
    std::os::unix::fs::symlink(&other, &link).unwrap();
    assert_eq!(
        eng.restore(&link, &NoObserver).unwrap_err().kind,
        ErrorKind::Blocked
    );
    assert_eq!(
        eng.restore(&e.src, &NoObserver).unwrap_err().kind,
        ErrorKind::Blocked
    ); // a real folder
    assert!(other.exists());
}

#[test]
fn check_refuses_unsafe_requests() {
    let e = env("blocked");
    let mut f = Fake::new(&e.root.join("D"));
    let code = |f: &Fake, src: &Path, dest: &Path| {
        let eng = engine(f, &e);
        eng.check(src, dest, &NoObserver).unwrap_err()[0].code
    };
    // destination exists
    fs::create_dir_all(&e.dest).unwrap();
    assert_eq!(code(&f, &e.src, &e.dest), "dest_exists");
    fs::remove_dir_all(&e.dest).unwrap();
    // same drive
    assert_eq!(code(&f, &e.src, &e.root.join("C/elsewhere")), "same_volume");
    // inside itself
    assert_eq!(code(&f, &e.src, &e.src.join("inner")), "dest_inside_source");
    // not a folder / missing
    assert_eq!(code(&f, &e.src.join("a.txt"), &e.dest), "not_a_folder");
    assert_eq!(code(&f, &e.root.join("nope"), &e.dest), "cannot_open");
    // removable / FAT32 / no space
    f.dest_kind = DriveKind::Removable;
    assert_eq!(code(&f, &e.src, &e.dest), "dest_not_fixed");
    f.dest_kind = DriveKind::Fixed;
    f.dest_fs = "FAT32";
    assert_eq!(code(&f, &e.src, &e.dest), "dest_fat32");
    f.dest_fs = "NTFS";
    f.dest_free = 1024;
    assert_eq!(code(&f, &e.src, &e.dest), "no_space");
    // contains a link
    f.dest_free = 1 << 40;
    std::os::unix::fs::symlink("/tmp", e.src.join("lnk")).unwrap();
    assert_eq!(code(&f, &e.src, &e.dest), "contains_links");
    fs::remove_file(e.src.join("lnk")).unwrap();
    // already moved
    let eng = engine(&f, &e);
    let p = plan(&eng, &e);
    eng.move_folder(&p, &Options::default(), &NoObserver)
        .unwrap();
    assert_eq!(code(&f, &e.src, &e.root.join("D/second")), "already_link");
}

#[test]
fn warnings_require_confirmation() {
    let e = env("warn");
    let mut f = Fake::new(&e.root.join("D"));
    f.dest_fs = "exFAT";
    let eng = engine(&f, &e);
    let p = plan(&eng, &e);
    assert_eq!(p.warnings[0].code, "dest_exfat");
    let err = eng
        .move_folder(&p, &Options::default(), &NoObserver)
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::NeedsConfirmation);
    assert_original_intact(&e);
    assert!(no_journals(&e));
    eng.move_folder(
        &p,
        &Options {
            accept_warnings: true,
            ..Default::default()
        },
        &NoObserver,
    )
    .unwrap();
}

#[test]
fn service_users_warn() {
    let e = env("svc");
    let mut f = Fake::new(&e.root.join("D"));
    f.users = vec![AppUser {
        name: "GameUpdater".into(),
        pid: None,
        is_service: true,
    }];
    let eng = engine(&f, &e);
    let p = plan(&eng, &e);
    assert!(p.warnings.iter().any(|w| w.code == "service_in_use"));
}

// ---- crash recovery: build the on-disk situation a crash would leave, then recover.

fn crash_journal(e: &Env, state: State) -> Journal {
    let partial = PathBuf::from(format!("{}.rehome-partial", e.dest.display()));
    let old = PathBuf::from(format!("{}.rehome-old", e.src.display()));
    let mut j = Journal::new(&e.journals, Kind::Move, &e.src, &e.dest, &partial, &old, 0);
    j.set_state(state).unwrap();
    j
}

#[test]
fn recover_after_crash_during_copy() {
    let e = env("crash1");
    let f = Fake::new(&e.root.join("D"));
    let eng = engine(&f, &e);
    let mut j = crash_journal(&e, State::Copying);
    fs::create_dir_all(PathBuf::from(format!("{}.rehome-partial", e.dest.display())).join("half"))
        .unwrap();
    assert_eq!(eng.pending().len(), 1);
    assert_eq!(eng.recover(&mut j).unwrap(), RecoverOutcome::RolledBack);
    assert_original_intact(&e);
    assert!(leftovers(&e).is_empty());
    assert!(no_journals(&e));
}

#[test]
fn recover_after_crash_between_rename_and_junction() {
    let e = env("crash2");
    let f = Fake::new(&e.root.join("D"));
    let eng = engine(&f, &e);
    let mut j = crash_journal(&e, State::SourceRenamed);
    // Disk as the crash left it: original renamed away, full copy in the partial folder.
    let partial = PathBuf::from(format!("{}.rehome-partial", e.dest.display()));
    let old = PathBuf::from(format!("{}.rehome-old", e.src.display()));
    fs::create_dir_all(partial.parent().unwrap()).unwrap();
    fs::rename(&e.src, &old).unwrap();
    fs::create_dir_all(&partial).unwrap();
    assert_eq!(eng.recover(&mut j).unwrap(), RecoverOutcome::RolledBack);
    assert_original_intact(&e);
    assert!(leftovers(&e).is_empty());
    assert!(no_journals(&e));
}

#[test]
fn recover_after_crash_with_junction_made_but_old_not_deleted() {
    let e = env("crash3");
    let f = Fake::new(&e.root.join("D"));
    let eng = engine(&f, &e);
    let mut j = crash_journal(&e, State::JunctionMade);
    let old = PathBuf::from(format!("{}.rehome-old", e.src.display()));
    fs::rename(&e.src, &old).unwrap();
    fs::create_dir_all(&e.dest).unwrap();
    fs::write(e.dest.join("a.txt"), "alpha").unwrap();
    std::os::unix::fs::symlink(&e.dest, &e.src).unwrap();
    assert_eq!(eng.recover(&mut j).unwrap(), RecoverOutcome::Completed);
    assert!(!old.exists());
    assert_eq!(fs::read_to_string(e.src.join("a.txt")).unwrap(), "alpha");
    assert!(no_journals(&e));
}

#[test]
fn rollback_refuses_to_delete_when_original_is_not_in_place() {
    let e = env("safety");
    let f = Fake::new(&e.root.join("D"));
    let eng = engine(&f, &e);
    let mut j = crash_journal(&e, State::DestPublished);
    // The "original" is missing entirely and there is no old copy: the copy at dest is the only data.
    fs::remove_dir_all(&e.src).unwrap();
    fs::create_dir_all(&e.dest).unwrap();
    fs::write(e.dest.join("precious.txt"), "only copy").unwrap();
    let err = eng.recover(&mut j).unwrap_err();
    assert_eq!(err.kind, ErrorKind::Rollback);
    assert_eq!(
        fs::read_to_string(e.dest.join("precious.txt")).unwrap(),
        "only copy"
    );
}
