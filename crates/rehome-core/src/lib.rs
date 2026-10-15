//! Rehome core: move a folder to another drive and leave a directory junction behind.
//!
//! All logic lives here and talks to the OS only through [`platform::Platform`], so it can
//! be tested anywhere. The Windows implementation lives in the `app` crate.

pub mod engine;
pub mod error;
pub mod fsops;
pub mod journal;
pub mod platform;
pub mod safety;

pub use engine::{Engine, MoveOutcome, NoObserver, Observer, Options, Plan, RecoverOutcome, Step};
pub use error::{ErrorKind, RehomeError};
pub use platform::{AppUser, DriveKind, Platform, VolumeInfo};
