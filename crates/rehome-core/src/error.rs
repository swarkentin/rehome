//! User-facing errors. Every error says what happened, why, and what to do next.

use std::fmt;
use std::io;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// Refused by a safety rule before anything was changed.
    Blocked,
    /// Something still has files in the folder open.
    InUse,
    NoSpace,
    Io,
    /// The copy did not match the original.
    VerifyFailed,
    Cancelled,
    /// Rolling back failed: the user may need to act.
    Rollback,
    NeedsConfirmation,
}

#[derive(Debug, Clone)]
pub struct RehomeError {
    pub kind: ErrorKind,
    pub title: String,
    pub explanation: String,
    pub suggestion: String,
    /// True when the user's original files are known to be intact.
    pub data_safe: bool,
    /// Technical detail (OS error codes, paths) for a "Details" expander.
    pub details: Option<String>,
}

impl RehomeError {
    pub fn new(
        kind: ErrorKind,
        title: impl Into<String>,
        explanation: impl Into<String>,
        suggestion: impl Into<String>,
    ) -> Self {
        Self {
            kind,
            title: title.into(),
            explanation: explanation.into(),
            suggestion: suggestion.into(),
            data_safe: true,
            details: None,
        }
    }

    pub fn data_at_risk(mut self) -> Self {
        self.data_safe = false;
        self
    }

    pub fn with_details(mut self, d: impl Into<String>) -> Self {
        self.details = Some(d.into());
        self
    }

    /// Wrap an I/O error with plain-language context.
    pub fn io(doing: &str, path: &Path, e: &io::Error) -> Self {
        let (why, what) = describe_io(e);
        let kind = if is_disk_full(e) {
            ErrorKind::NoSpace
        } else {
            ErrorKind::Io
        };
        Self::new(
            kind,
            format!("Couldn't {doing}."),
            format!("{why} (\"{}\")", path.display()),
            what,
        )
        .with_details(format!("{e} [{:?}]", e.raw_os_error()))
    }

    pub fn cancelled() -> Self {
        Self::new(
            ErrorKind::Cancelled,
            "Cancelled.",
            "The move was stopped before it finished.",
            "Nothing else to do. You can start the move again whenever you like.",
        )
    }
}

pub fn is_disk_full(e: &io::Error) -> bool {
    // 112 = ERROR_DISK_FULL (Windows), 28 = ENOSPC (Unix), 39 = ERROR_HANDLE_DISK_FULL
    matches!(e.raw_os_error(), Some(112) | Some(39) | Some(28))
}

fn describe_io(e: &io::Error) -> (String, String) {
    if is_disk_full(e) {
        return (
            "The drive ran out of space while writing.".into(),
            "Free up space on the destination drive or choose a drive with more room.".into(),
        );
    }
    match e.raw_os_error() {
        Some(32) => {
            return (
                "Another program is using a file in this folder.".into(),
                "Close apps that might be using the folder (check the system tray) and try again."
                    .into(),
            )
        }
        Some(206) => {
            return (
                "A file path is too long for Windows to handle.".into(),
                "Choose a shorter destination path, or move the folder to the root of a drive."
                    .into(),
            )
        }
        _ => {}
    }
    match e.kind() {
        io::ErrorKind::NotFound => (
            "A file or folder disappeared or doesn't exist.".into(),
            "Make sure nothing else is changing the folder, then try again.".into(),
        ),
        io::ErrorKind::PermissionDenied => (
            "Windows denied access.".into(),
            "Try again as administrator, or check that your account owns this folder.".into(),
        ),
        io::ErrorKind::AlreadyExists => (
            "Something already exists at that location.".into(),
            "Choose a different destination or remove the existing item.".into(),
        ),
        _ => (
            format!("Windows reported: {e}."),
            "Try again. If this keeps happening, copy the details and report it.".into(),
        ),
    }
}

impl fmt::Display for RehomeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "{}", self.title)?;
        writeln!(f, "{}", self.explanation)?;
        write!(f, "What to do: {}", self.suggestion)?;
        if self.data_safe {
            write!(f, "\nYour original files are intact.")?;
        }
        Ok(())
    }
}

impl std::error::Error for RehomeError {}

pub type Result<T> = std::result::Result<T, RehomeError>;
