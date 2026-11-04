//! Building a generation name for a base log path.
//!
//! A child of `naming`. This half writes the name and `recognise` reads one
//! back. The shapes both must agree on are the parent's constants, so a drift
//! between the two halves shows up as a name that never matches itself back.

use std::{
    path::{Path, PathBuf},
    time::SystemTime,
};

use jiff::Timestamp;

use super::{GZ_SUFFIX, STAMP_FORMAT, is_stamp};

#[cfg(doc)]
use super::match_generation;

/// A live log file, split into the pieces a generation name is built from.
///
/// `/var/log/web-0-out.log` splits into `/var/log`, `web-0-out` and `log`.
/// The extension is the final one only: `/var/log/web.out` splits into `web`
/// and `out`, not `web` and `out.log`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogPath {
    /// The directory the log and all its generations live in.
    pub dir: PathBuf,
    /// The file name with its final extension removed.
    pub stem: String,
    /// The final extension, without its dot. `None` for a name with no dot.
    pub ext: Option<String>,
}

impl LogPath {
    /// Split a log path into directory, stem and final extension.
    ///
    /// A path with no directory component gets `.`, not the empty path.
    /// `Path::new("app.log").parent()` is `Some("")` rather than `None`, and
    /// the empty path is not a directory anything can read: `fs::read_dir("")`
    /// is `NotFound`. `rotate::generations` reads a `NotFound` as "this sheep
    /// has never started" and returns an empty list, so a base spelled
    /// without a directory component would report no generations however many
    /// were on disk, and the numeric shift would rename the live file over
    /// the top of generation 1. shep does not absolutise a Flockfile's
    /// `out_file`, so a relative one arrives here exactly as it was written.
    ///
    /// The substitution lives here, once, so every consumer inherits it: a
    /// second copy of this rule somewhere downstream is a copy that can be
    /// missing from a third place.
    ///
    /// Returns `None` if `path` has no file name, or if the file name is not
    /// valid UTF-8. A name this dog cannot spell is a name it must not go on
    /// to build generations from, rename, or delete. The directory is kept
    /// as it came, UTF-8 or not: it is only ever joined and compared, never
    /// spelled into a name.
    pub fn split(path: &Path) -> Option<Self> {
        let name = path.file_name()?.to_str()?;
        // `file_stem`/`extension` already implement "the final extension,
        // and a leading dot does not start one".
        let stem = Path::new(name).file_stem()?.to_str()?.to_owned();
        let ext = match Path::new(name).extension() {
            Some(ext) => Some(ext.to_str()?.to_owned()),
            None => None,
        };
        let dir = match path.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
            _ => PathBuf::from("."),
        };
        Some(Self { dir, stem, ext })
    }

    /// Rebuild the live log path this was split from.
    pub fn live(&self) -> PathBuf {
        self.dir.join(self.live_name())
    }

    /// The live log's file name alone: `web-0-out.log` for
    /// `/var/log/web-0-out.log`.
    pub fn live_name(&self) -> String {
        self.file_name("")
    }

    /// The file name for this base, with `infix` spliced in before the
    /// extension. An empty `infix` rebuilds the live name.
    fn file_name(&self, infix: &str) -> String {
        let mut name = String::with_capacity(self.stem.len() + infix.len() + 8);
        name.push_str(&self.stem);
        name.push_str(infix);
        if let Some(ext) = &self.ext {
            name.push('.');
            name.push_str(ext);
        }
        name
    }
}

/// Render `at` as a UTC stamp, `2026-08-21T15-04-05`, exactly 19 characters.
///
/// UTC rather than local time on purpose. Local time goes backwards for an
/// hour at the end of daylight saving, which would misorder that hour's
/// generations once a year in a scheme whose pruning depends on their order.
/// UTC has no such hole. The README says the stamps are UTC so nobody reads
/// one as local.
///
/// A time outside years 1970 to 9999 is clamped to the nearest end of that
/// range. Only a badly broken clock gets there, and a year outside it renders
/// in more or fewer than four digits, so [`match_generation`] would never
/// recognise the file again and pruning would never reclaim it.
pub fn stamp_utc(at: SystemTime) -> String {
    let timestamp = match Timestamp::try_from(at) {
        Ok(timestamp) => timestamp.max(Timestamp::UNIX_EPOCH),
        Err(_) if at < SystemTime::UNIX_EPOCH => Timestamp::UNIX_EPOCH,
        Err(_) => Timestamp::MAX,
    };
    timestamp.strftime(STAMP_FORMAT).to_string()
}

/// Build the dated name for a generation: `{stem}.{stamp}.{ext}`, or
/// `{stem}.{stamp}.{counter}.{ext}` when `counter` is 1 or more.
///
/// The extension stays last, so the file still matches `*.log`, still opens
/// with log syntax highlighting, and is still found by every glob an operator
/// already has.
///
/// # Panics
/// In debug builds, if `stamp` is not the shape [`stamp_utc`] produces. A
/// name built from a malformed stamp could never be matched back, so it could
/// never be pruned.
#[track_caller]
pub fn dated_name(base: &LogPath, stamp: &str, counter: u32) -> PathBuf {
    debug_assert!(
        is_stamp(stamp.as_bytes()),
        "{stamp} is not the shape stamp_utc produces"
    );
    let infix = if counter == 0 {
        format!(".{stamp}")
    } else {
        format!(".{stamp}.{counter}")
    };
    base.dir.join(base.file_name(&infix))
}

/// Build the numeric name for a generation: `{stem}.{ext}.{n}`.
///
/// `.1` is the newest, following logrotate. macOS `newsyslog` disagrees and
/// calls the newest `.0`; shep follows logrotate, and the README says so.
///
/// # Panics
/// In debug builds, if `n` is 0. `.0` is not a name this dog writes, so
/// [`match_generation`] refuses it, so a `.0` written here could never be
/// pruned.
#[track_caller]
pub fn numeric_name(base: &LogPath, n: u32) -> PathBuf {
    debug_assert!(n >= 1, "numeric generations start at 1, not {n}");
    let mut name = base.live_name();
    name.push('.');
    name.push_str(&n.to_string());
    base.dir.join(name)
}

/// Append a `.gz` suffix to a generation's path.
///
/// Appended to the whole name rather than swapped in as an extension, so
/// `web-0-out.2026-08-20T15-04-05.log` becomes
/// `web-0-out.2026-08-20T15-04-05.log.gz` and every glob an operator already
/// has keeps working.
///
/// It lives next to [`match_generation`] because it is the other half of that
/// function's `.gz` stripping. The suffix one writes is the suffix the other
/// reads back, and two copies of it in two modules is two places for that
/// agreement to drift.
pub fn with_gz(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(GZ_SUFFIX);
    PathBuf::from(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::naming::base;

    #[test]
    fn a_log_path_splits_into_directory_stem_and_extension() {
        let split = base();
        assert_eq!(split.dir, Path::new("/var/log"));
        assert_eq!(split.stem, "web-0-out");
        assert_eq!(split.ext.as_deref(), Some("log"));
        assert_eq!(split.live(), Path::new("/var/log/web-0-out.log"));
        assert_eq!(split.live_name(), "web-0-out.log");
    }

    #[test]
    fn a_log_path_without_an_extension_still_splits() {
        let split = LogPath::split(Path::new("/var/log/web-out")).expect("splits");
        assert_eq!(split.stem, "web-out");
        assert_eq!(split.ext, None);
        assert_eq!(split.live(), Path::new("/var/log/web-out"));
    }

    #[test]
    fn a_relative_path_splits_to_the_current_directory() {
        // `Path::new("web-0-out.log").parent()` is `Some("")`, not `None`,
        // and an empty directory is not a directory anything can read:
        // `fs::read_dir("")` is `NotFound`. `rotate::generations` reads a
        // `NotFound` as "this sheep has never started", which for a base
        // with no directory component would mean every generation on disk
        // is invisible and the numeric shift renames over the top of one.
        // The empty parent is turned into `.` here, once, so no consumer
        // has to know about it.
        let here = LogPath::split(Path::new("web-0-out.log")).expect("splits");
        assert_eq!(here.dir, Path::new("."));
        assert_eq!(here.live(), Path::new("./web-0-out.log"));
        assert_eq!(
            dated_name(&here, "2026-08-20T15-04-05", 0),
            Path::new("./web-0-out.2026-08-20T15-04-05.log")
        );
        assert_eq!(numeric_name(&here, 1), Path::new("./web-0-out.log.1"));
    }

    #[test]
    fn a_path_with_no_file_name_does_not_split() {
        for path in ["/", "", ".", "..", "/..", "../.."] {
            assert!(
                LogPath::split(Path::new(path)).is_none(),
                "{path:?} has no file name"
            );
        }
        // A trailing slash is not a signal std keeps: "/var/log/" and
        // "/var/log" are the same Path, so this splits as a file called
        // "log" in "/var". Callers pass the path of a log file, not of a
        // directory, so nothing downstream depends on telling them apart.
        let slashed = LogPath::split(Path::new("/var/log/")).expect("splits");
        assert_eq!(slashed.stem, "log");
        assert_eq!(slashed.dir, Path::new("/var"));
    }

    #[test]
    fn the_dated_name_keeps_the_extension_last() {
        let made = dated_name(&base(), "2026-08-20T15-04-05", 0);
        assert_eq!(
            made,
            Path::new("/var/log/web-0-out.2026-08-20T15-04-05.log")
        );
    }

    #[test]
    fn a_same_second_collision_appends_a_counter_before_the_extension() {
        let made = dated_name(&base(), "2026-08-20T15-04-05", 1);
        assert_eq!(
            made,
            Path::new("/var/log/web-0-out.2026-08-20T15-04-05.1.log")
        );
    }

    #[test]
    fn the_numeric_name_appends_after_the_extension() {
        assert_eq!(
            numeric_name(&base(), 1),
            Path::new("/var/log/web-0-out.log.1")
        );
    }

    #[test]
    fn the_stamp_is_utc_and_nineteen_characters() {
        let at = std::time::UNIX_EPOCH + core::time::Duration::from_secs(1_787_324_645);
        let stamp = stamp_utc(at);
        assert_eq!(stamp, "2026-08-21T15-04-05");
        assert_eq!(stamp.len(), 19);
        assert_eq!(stamp.as_bytes()[10], b'T');
        assert!(
            !stamp.contains(':'),
            "colons are not portable in filenames: {stamp}"
        );
    }
}
