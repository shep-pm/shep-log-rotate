//! Turning one rotated generation into its `.gz`, and the create that keeps
//! the archive at the source log's permissions.
//!
//! A child of [`prune`](super) rather than a module of its own because a
//! compression ends in a delete: the plain file goes only once the archive
//! is on disk, and that delete is the parent's `remove`, the one line in
//! this binary that destroys data.

use std::{fs, io, path::Path};

use flate2::{Compression, write::GzEncoder};

use crate::error::Error;

/// Create `target`, empty and already at `permissions`, and hand back the
/// open handle.
///
/// The create is exclusive: `O_EXCL` never follows a symlink, so this is
/// the one create that cannot be pointed somewhere else by a link planted
/// at the target's name. Nothing this dog writes is a symlink, so one found
/// there is somebody else's and is refused. A regular file found there is
/// the half-written leftover of the crash that made this generation a
/// twin, and finishing the compression means replacing it: it is removed
/// and the create runs again, as exclusive as the first, so a link planted
/// in between is refused too. A check before an ordinary create would
/// leave that window open.
///
/// The permissions go on the handle rather than the path, and in the same
/// function as the create, because the two have to stay adjacent. A create
/// opens at `0o666 & !umask`, so a chmod that drifts away from it, down to
/// the end of a longer function say, leaves a log that is private for a
/// reason readable by anyone for the entire length of the gzip. On a large
/// log that is not a short window, and no test can see it: what a test can
/// check is that the mode is right at the end, which is true either way.
/// Only the empty file is ever at the creation mode, and the handle keeps
/// its write access across the change, so a read-only mode still works.
fn create_with_permissions(target: &Path, permissions: fs::Permissions) -> Result<fs::File, Error> {
    let file = match create_new(target) {
        Ok(file) => file,
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
            let found = fs::symlink_metadata(target).map_err(Error::io_at(target))?;
            if found.file_type().is_symlink() {
                return Err(Error::io_at(target)(io::Error::other(
                    "is a symlink, and this dog does not write through one",
                )));
            }
            super::remove(target)?;
            create_new(target).map_err(Error::io_at(target))?
        }
        Err(err) => return Err(Error::io_at(target)(err)),
    };
    file.set_permissions(permissions)
        .map_err(Error::io_at(target))?;
    Ok(file)
}

/// Create `target` for writing, failing if anything is at that name.
fn create_new(target: &Path) -> io::Result<fs::File> {
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(target)
}

/// gzip `path` into `target`, then remove `path`.
///
/// The order is the whole point. A crash after the write and before the
/// remove leaves both files, which is the state [`tidy`](super) recovers
/// from: the two halves carry the same generation, share one slot, and the
/// plain half is compressed again over the leftover. Removing first would
/// lose the log outright.
///
/// The `sync_all` puts the compressed bytes on the disk before the unlink is
/// issued. It does not order the two directory operations against each
/// other, which would need `base.dir` fsynced as well, so it narrows that
/// window rather than closing it.
///
/// Permissions come across with the bytes, and ahead of them. A log written
/// 0600 because it carries something private stays 0600 once compressed.
pub(super) fn compress(path: &Path, target: &Path) -> Result<(), Error> {
    let mut source = fs::File::open(path).map_err(Error::io_at(path))?;
    let permissions = source.metadata().map_err(Error::io_at(path))?.permissions();

    let sink = create_with_permissions(target, permissions)?;

    let mut encoder = GzEncoder::new(sink, Compression::default());
    io::copy(&mut source, &mut encoder).map_err(Error::io_at(path))?;
    let sink = encoder.finish().map_err(Error::io_at(target))?;
    sink.sync_all().map_err(Error::io_at(target))?;

    super::remove(path)
}

#[cfg(test)]
mod tests {
    use super::super::tests::{config, gunzip};
    use super::*;
    use crate::{
        config::Naming,
        file_set::FileSet,
        prune::tidy,
        test_support::{live_log, seed},
    };

    #[test]
    fn compression_round_trips_the_bytes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let body = "line one\nline two\nline three\n";
        seed(dir.path(), "web-0-out.2026-08-20T15-04-05.log", body);
        seed(dir.path(), "web-0-out.2026-08-20T15-04-06.log", "newest\n");
        let base = live_log(dir.path(), "web-0-out.log", "live\n");

        tidy(&base, &config(Naming::Dated, 5, true), &FileSet::default()).expect("tidied");

        assert_eq!(
            gunzip(&dir.path().join("web-0-out.2026-08-20T15-04-05.log.gz")),
            body,
            "a rotator that mangles a log is worse than no rotator"
        );
    }

    #[cfg(unix)]
    #[test]
    fn compression_keeps_the_source_files_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tempdir");
        let private = dir.path().join("web-0-out.2026-08-20T15-04-01.log");
        fs::write(&private, "a token, probably\n").expect("seeded");
        fs::set_permissions(&private, fs::Permissions::from_mode(0o600)).expect("chmod");
        seed(dir.path(), "web-0-out.2026-08-20T15-04-02.log", "newest\n");
        let base = live_log(dir.path(), "web-0-out.log", "live\n");

        tidy(&base, &config(Naming::Dated, 5, true), &FileSet::default()).expect("tidied");

        let mode = fs::metadata(dir.path().join("web-0-out.2026-08-20T15-04-01.log.gz"))
            .expect("the compressed copy")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            mode, 0o600,
            "a log kept private must not widen when it is compressed"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_at_the_gz_target_is_refused_not_written_through() {
        // Somebody planted a link where this generation's archive goes.
        // Creating the archive through it would truncate and fill whatever
        // it points at, anywhere on disk.
        let dir = tempfile::tempdir().expect("tempdir");
        let elsewhere = seed(dir.path(), "elsewhere.txt", "somebody else's file\n");
        let plain = seed(dir.path(), "web-0-out.2026-08-20T15-04-01.log", "ours\n");
        seed(dir.path(), "web-0-out.2026-08-20T15-04-02.log", "newest\n");
        let link = dir.path().join("web-0-out.2026-08-20T15-04-01.log.gz");
        std::os::unix::fs::symlink(&elsewhere, &link).expect("linked");
        let base = live_log(dir.path(), "web-0-out.log", "live\n");

        let tidied =
            tidy(&base, &config(Naming::Dated, 5, true), &FileSet::default()).expect("tidied");

        assert_eq!(tidied.compressed, 0);
        assert_eq!(tidied.faults.len(), 1, "{:?}", tidied.faults);
        assert!(tidied.faults[0].contains("symlink"), "{:?}", tidied.faults);
        assert_eq!(
            fs::read_to_string(&elsewhere).expect("untouched"),
            "somebody else's file\n"
        );
        assert!(plain.exists(), "the generation is left plain");
        assert!(
            fs::symlink_metadata(&link)
                .expect("still there")
                .file_type()
                .is_symlink(),
            "and the link is not this dog's to remove"
        );
    }

    #[cfg(unix)]
    #[test]
    fn compression_narrows_a_stale_gz_left_wide_by_an_earlier_crash() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tempdir");
        let private = dir.path().join("web-0-out.2026-08-20T15-04-01.log");
        fs::write(&private, "a token, probably\n").expect("seeded");
        fs::set_permissions(&private, fs::Permissions::from_mode(0o600)).expect("chmod");
        // The half-written `.gz` an earlier crash left, at the mode the
        // umask happened to give it. Overwriting it must not inherit that.
        let stale = dir.path().join("web-0-out.2026-08-20T15-04-01.log.gz");
        fs::write(&stale, "half a gzip stream").expect("seeded");
        fs::set_permissions(&stale, fs::Permissions::from_mode(0o644)).expect("chmod");
        seed(dir.path(), "web-0-out.2026-08-20T15-04-02.log", "newest\n");
        let base = live_log(dir.path(), "web-0-out.log", "live\n");

        tidy(&base, &config(Naming::Dated, 5, true), &FileSet::default()).expect("tidied");

        let mode = fs::metadata(&stale)
            .expect("the compressed copy")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "the source's mode wins, not the leftover's");
    }
}
