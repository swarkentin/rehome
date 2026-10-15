//! Crash-safe record of an in-progress move. A tiny `key=value` file written
//! atomically (temp file + rename) before each step.

use crate::error::{ErrorKind, RehomeError, Result};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum State {
    Planned,
    Copying,
    Copied,
    Verified,
    SourceRenamed,
    DeltaSynced,
    DestPublished,
    JunctionMade,
    // Restore ("move back") only:
    LinkRemoved,
}

impl State {
    const ALL: [(State, &'static str); 9] = [
        (State::Planned, "planned"),
        (State::Copying, "copying"),
        (State::Copied, "copied"),
        (State::Verified, "verified"),
        (State::SourceRenamed, "source-renamed"),
        (State::DeltaSynced, "delta-synced"),
        (State::DestPublished, "dest-published"),
        (State::JunctionMade, "junction-made"),
        (State::LinkRemoved, "link-removed"),
    ];
    pub fn as_str(self) -> &'static str {
        Self::ALL.iter().find(|(s, _)| *s == self).unwrap().1
    }
    pub fn parse(s: &str) -> Option<State> {
        Self::ALL.iter().find(|(_, n)| *n == s).map(|(s, _)| *s)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Move,
    Restore,
}

#[derive(Debug, Clone)]
pub struct Journal {
    pub id: String,
    pub kind: Kind,
    pub state: State,
    /// The original path (where the junction lives).
    pub source: PathBuf,
    /// Where the data lives after the move.
    pub dest: PathBuf,
    /// Temporary copy (`<dest>.rehome-partial`, or `<source>.rehome-restoring`).
    pub partial: PathBuf,
    /// The renamed original (`<source>.rehome-old`); unused for restores.
    pub old: PathBuf,
    pub bytes: u64,
    pub keep_backup: bool,
    dir: PathBuf,
}

fn new_id() -> String {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{:x}-{:x}", n, std::process::id())
}

impl Journal {
    pub fn new(
        dir: &Path,
        kind: Kind,
        source: &Path,
        dest: &Path,
        partial: &Path,
        old: &Path,
        bytes: u64,
    ) -> Self {
        Journal {
            id: new_id(),
            kind,
            state: State::Planned,
            source: source.into(),
            dest: dest.into(),
            partial: partial.into(),
            old: old.into(),
            bytes,
            keep_backup: false,
            dir: dir.into(),
        }
    }

    fn path(&self) -> PathBuf {
        self.dir.join(format!("{}.journal", self.id))
    }

    pub fn save(&self) -> Result<()> {
        let fail = |e: std::io::Error| {
            RehomeError::io("save progress information", &self.dir, &e)
                .with_details("journal write failed")
        };
        fs::create_dir_all(&self.dir).map_err(fail)?;
        let text = format!(
            "version=1\nid={}\nkind={}\nstate={}\nsource={}\ndest={}\npartial={}\nold={}\nbytes={}\nkeep_backup={}\n",
            self.id,
            if self.kind == Kind::Move { "move" } else { "restore" },
            self.state.as_str(),
            self.source.display(),
            self.dest.display(),
            self.partial.display(),
            self.old.display(),
            self.bytes,
            self.keep_backup,
        );
        let tmp = self.dir.join(format!("{}.tmp", self.id));
        fs::write(&tmp, text).map_err(fail)?;
        fs::rename(&tmp, self.path()).map_err(fail)
    }

    pub fn set_state(&mut self, s: State) -> Result<()> {
        self.state = s;
        self.save()
    }

    /// Remove the journal file (the move finished or was fully rolled back).
    pub fn discard(&self) {
        let _ = fs::remove_file(self.path());
    }

    pub fn load(dir: &Path, file: &Path) -> Result<Journal> {
        let bad = |why: &str| {
            RehomeError::new(
                ErrorKind::Io,
                "A progress file is damaged.",
                format!("Couldn't read \"{}\": {why}.", file.display()),
                "Delete that file; then check the folder it refers to by hand.",
            )
        };
        let text = fs::read_to_string(file)
            .map_err(|e| RehomeError::io("read progress information", file, &e))?;
        let get = |k: &str| -> Option<&str> {
            text.lines()
                .find_map(|l| l.strip_prefix(k).and_then(|r| r.strip_prefix('=')))
        };
        let need = |k: &str| get(k).ok_or_else(|| bad(&format!("missing {k}")));
        Ok(Journal {
            id: need("id")?.into(),
            kind: match need("kind")? {
                "move" => Kind::Move,
                "restore" => Kind::Restore,
                _ => return Err(bad("unknown kind")),
            },
            state: State::parse(need("state")?).ok_or_else(|| bad("unknown state"))?,
            source: need("source")?.into(),
            dest: need("dest")?.into(),
            partial: need("partial")?.into(),
            old: need("old")?.into(),
            bytes: need("bytes")?.parse().map_err(|_| bad("bad size"))?,
            keep_backup: get("keep_backup") == Some("true"),
            dir: dir.into(),
        })
    }

    /// All unfinished journals in `dir`.
    pub fn pending(dir: &Path) -> Vec<Journal> {
        let Ok(rd) = fs::read_dir(dir) else {
            return Vec::new();
        };
        let mut v: Vec<Journal> = rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "journal"))
            .filter_map(|p| Journal::load(dir, &p).ok())
            .collect();
        v.sort_by(|a, b| a.id.cmp(&b.id));
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_discards() {
        let dir = std::env::temp_dir().join(format!("rehome-journal-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let mut j = Journal::new(
            &dir,
            Kind::Move,
            Path::new("C:\\a b"),
            Path::new("D:\\x"),
            Path::new("D:\\x.p"),
            Path::new("C:\\a b.o"),
            42,
        );
        j.save().unwrap();
        j.set_state(State::SourceRenamed).unwrap();
        let p = Journal::pending(&dir);
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].state, State::SourceRenamed);
        assert_eq!(p[0].source, Path::new("C:\\a b"));
        assert_eq!(p[0].bytes, 42);
        j.discard();
        assert!(Journal::pending(&dir).is_empty());
    }

    #[test]
    fn states_are_ordered_and_named() {
        assert!(State::Copying < State::DestPublished);
        for (s, n) in State::ALL {
            assert_eq!(State::parse(n), Some(s));
        }
        assert_eq!(State::parse("nope"), None);
    }
}
