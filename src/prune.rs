//! Compressing rotated generations, and deleting the ones past `keep`.
//!
//! This is the only module in the binary that removes a file, and
//! [`compress`] is its child: a compression that succeeds ends in
//! [`remove`]. Everything else renames, writes or reads, so every wrong
//! answer made here is somebody's evidence missing from the directory they
//! went to look in after an incident. That asymmetry settles every arguable
//! case on its own: a generation left behind costs disk, and disk is cheap.
//!
//! [`tidy`] runs *after* the reopen, never before. Compressing a large log
//! is not quick, and doing it first would widen the stretch during which
//! shep is still writing through its old descriptor. The caller owns that
//! ordering; this module's half of the bargain is to do nothing slow before
//! it returns.

use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};

use crate::{
    config::Config,
    error::Error,
    file_set::FileSet,
    naming::{LogPath, Order, with_gz},
    rotate::{GenerationFile, generations},
};

mod compress;

/// What one pass of [`tidy`] did.
///
/// `Debug` is derived, deliberately, on the reasoning [`Error`] gives for
/// its own: every message in `faults` is printed for the operator on the
/// tick's summary line, and the path in it is the diagnostic. Redacting it
/// from `Debug` would hide from a maintainer what a user is shown anyway.
/// Pinned by `debug_carries_the_fault_paths`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tidied {
    /// Files gzipped on this pass. A generation is compressed at most once,
    /// so this is also the number of generations compressed.
    pub compressed: usize,
    /// Files deleted on this pass for being past `keep`.
    ///
    /// Files rather than generations, because the two can differ. A
    /// generation an earlier crash left half-compressed wears two files, and
    /// they go together, so it contributes two.
    pub deleted: usize,
    /// Times a generation was left alone because acting on it would have
    /// written over or removed a path the caller named as a live log.
    ///
    /// Refusals rather than files: one file can be the reason for two of
    /// them, once as a candidate generation of its own and once as the name
    /// some other generation wanted to compress into.
    ///
    /// Anything above zero is a name collision worth reporting rather than
    /// discovering later. A base with no extension generates, byte for byte,
    /// the names a base whose extension is all digits generates: under
    /// `numeric` naming `/var/log/web` produces exactly `/var/log/web.1`,
    /// which may be the live log of a different sheep. Both readings of that
    /// name are correct, so no amount of care in the matcher can separate
    /// them, and the only party that knows which files are being written to
    /// is the shepherd.
    pub skipped_protected: usize,
    /// Compressions and deletions that failed, one message each. A fault is
    /// one generation's: the generation is left as it was, and the pass goes
    /// on to the next one and still prunes. A generation that will not
    /// compress and is past `keep` is still deleted, plain.
    pub faults: Vec<String>,
}

/// One rotated generation, and the file or files on disk carrying it.
///
/// Normally one file. Two only when an earlier pass crashed between writing
/// the `.gz` and removing the plain copy: both halves parse to the same
/// [`Order`], so they are one generation wearing two files and they share
/// one slot. A slot each would let `keep` spare one half and delete the
/// other, wiping the generation out while reporting that it survived.
#[derive(Debug, Default)]
struct Generation {
    /// The uncompressed copy, while there still is one.
    plain: Option<PathBuf>,
    /// The gzipped copy, once there is one.
    gz: Option<PathBuf>,
}

impl Generation {
    /// Every file this generation still has on disk.
    fn files(&self) -> impl Iterator<Item = &PathBuf> {
        [self.plain.as_ref(), self.gz.as_ref()]
            .into_iter()
            .flatten()
    }
}

/// Compress and prune the generations of one log.
///
/// The newest generation is left plain in both naming schemes, so the most
/// recent rotation greps without a decompression step first. Everything past
/// `config.keep` is deleted; `keep` counts rotated generations and never the
/// live file, so `keep = 5` leaves five rotated generations behind.
///
/// A generation an earlier crash left half-compressed, a plain file and its
/// `.gz` side by side, is one generation and takes one slot. Its files go
/// together when it is pruned, and the next pass with compression on folds
/// it back down to one file.
///
/// A compression or a deletion that fails is one generation's fault,
/// recorded in [`Tidied::faults`], and the pass goes on: the next generation
/// is still compressed and the prune still runs. One generation that will
/// not compress, a `.gz` target that is a directory say, must not hold every
/// older generation of the base past `keep` for good.
///
/// No member of `protected` is ever compressed or deleted, whatever its name
/// says, and none of them counts against `keep` either: a live log that
/// happens to look like a generation is not one of this dog's, so sparing it
/// must not cost a real generation its place. Members are matched by file
/// name within `base.dir`, which [`FileSet`] resolves first, so a `..` in
/// the caller's spelling or a symlinked log directory cannot quietly turn
/// the guard off.
///
/// # Errors
/// [`Error::Io`], naming the directory, if the log directory cannot be
/// listed. Nothing can be done for a base whose generations cannot be seen.
pub fn tidy(base: &LogPath, config: &Config, protected: &FileSet) -> Result<Tidied, Error> {
    let mut tidied = Tidied::default();
    let untouchable = protected.names_in(&protected.resolve(&base.dir));

    // Two things happen here, and both are about slots.
    //
    // Protected paths are sifted out before anything counts positions. They
    // are somebody else's live logs that merely read as generations, so they
    // must not take up a slot `keep` was holding for a real one, and they
    // must not be mistaken for the newest generation and so decide which
    // file stays plain.
    //
    // Files sharing an `Order` are folded into one slot, because they are
    // one generation. `generations` returns its list sorted, so equal orders
    // arrive adjacent and a single running comparison is enough.
    let mut ours: Vec<Generation> = Vec::new();
    let mut current: Option<Order> = None;
    for GenerationFile {
        path,
        order,
        compressed,
    } in generations(base, config.naming)?
    {
        if is_protected(untouchable, &path) {
            tidied.skipped_protected += 1;
            continue;
        }
        if current.as_ref() != Some(&order) {
            ours.push(Generation::default());
            current = Some(order);
        }
        let generation = ours.last_mut().expect("a slot was pushed just above");
        if compressed {
            generation.gz = Some(path);
        } else {
            generation.plain = Some(path);
        }
    }

    // Index 0 is the newest and stays plain. Compressing before pruning
    // rather than after is the readable order, not the frugal one: the
    // "newest stays plain" rule sits next to the list it is about.
    if config.compress {
        for generation in ours.iter_mut().skip(1) {
            let Some(plain) = generation.plain.clone() else {
                continue;
            };
            let target = with_gz(&plain);
            // The target is a file this pass is about to create or truncate.
            // Truncating a live log is the same harm as deleting one, so it
            // gets the same refusal.
            if is_protected(untouchable, &target) {
                tidied.skipped_protected += 1;
                continue;
            }
            // A `.gz` already sitting there is the half-written leftover of
            // the crash that made this generation a twin. Replacing it is
            // how the interrupted compression finishes.
            if let Err(err) = compress::compress(&plain, &target) {
                tidied.faults.push(err.to_string());
                // `compress` creates its target before it writes a byte,
                // so a fault after that, a full disk say, leaves a partial
                // `.gz` beside the plain file. The generation wears both
                // now: they go together if it is pruned below, and the next
                // pass compresses the plain half over the partial one. Left
                // unrecorded, the prune would remove the plain file alone
                // and the truncated archive would stand in for the
                // generation for good.
                if fs::symlink_metadata(&target).is_ok_and(|meta| meta.is_file()) {
                    generation.gz = Some(target);
                }
                continue;
            }
            generation.plain = None;
            generation.gz = Some(target);
            tidied.compressed += 1;
        }
    }

    // Both halves of a twin go together. Sparing either would leave a file
    // no later pass can reach: it would be the same one generation every
    // time, and one generation is one slot.
    for generation in ours.iter().skip(config.keep) {
        for path in generation.files() {
            match remove(path) {
                Ok(()) => tidied.deleted += 1,
                Err(err) => tidied.faults.push(err.to_string()),
            }
        }
    }

    Ok(tidied)
}

/// Whether `path` names one of the caller's live logs.
///
/// By file name, which is sound only because every path this module acts on
/// was built from `base.dir`, and `names` is what the caller's [`FileSet`]
/// holds for that same directory.
fn is_protected(names: &BTreeSet<String>, path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| names.contains(name))
}

/// `fs::remove_file`, mapping its error through [`Error::Io`].
///
/// The one line in this binary that destroys data.
fn remove(path: &Path) -> Result<(), Error> {
    fs::remove_file(path).map_err(Error::io_at(path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::{Config, Naming},
        test_support::{live_log, seed},
    };
    use flate2::read::GzDecoder;
    use std::{fs, io::Read, ops::RangeInclusive};

    /// A section with `naming`, `keep` and `compress` set and the rest at
    /// its default.
    ///
    /// `pub(super)` so `compress`'s own tests can build one too.
    pub(super) fn config(naming: Naming, keep: usize, compress: bool) -> Config {
        Config {
            naming,
            keep,
            compress,
            ..Config::default()
        }
    }

    /// The whole decoded contents of a `.gz` this dog wrote.
    ///
    /// `pub(super)` for the same reason as [`config`].
    pub(super) fn gunzip(path: &Path) -> String {
        let mut decoded = String::new();
        GzDecoder::new(fs::File::open(path).expect("open"))
            .read_to_string(&mut decoded)
            .expect("decode");
        decoded
    }

    /// The name of the dated generation stamped at `second` in these tests'
    /// fixture logs, `.gz` when `compressed`.
    fn generation_name(second: u32, compressed: bool) -> String {
        let name = format!("web-0-out.2026-08-20T15-04-0{second}.log");
        if compressed {
            format!("{name}.gz")
        } else {
            name
        }
    }

    /// That generation sitting in `dir`.
    fn generation(dir: &Path, second: u32, compressed: bool) -> PathBuf {
        dir.join(generation_name(second, compressed))
    }

    /// Seed the dated generations stamped `range`, `body` in each one.
    fn seed_generations<B: AsRef<[u8]>>(
        dir: &Path,
        range: RangeInclusive<u32>,
        body: impl Fn(u32) -> B,
    ) {
        for second in range {
            seed(dir, generation_name(second, false), body(second));
        }
    }

    #[test]
    fn the_newest_generation_stays_plain_so_it_is_greppable() {
        let dir = tempfile::tempdir().expect("tempdir");
        seed_generations(dir.path(), 5..=7, |_| "body\n");
        let base = live_log(dir.path(), "web-0-out.log", "live\n");

        let tidied =
            tidy(&base, &config(Naming::Dated, 5, true), &FileSet::default()).expect("tidied");

        assert_eq!(tidied.compressed, 2);
        assert!(
            generation(dir.path(), 7, false).exists(),
            "newest stays plain"
        );
        assert!(generation(dir.path(), 6, true).exists());
        assert!(generation(dir.path(), 5, true).exists());
        assert!(
            !generation(dir.path(), 5, false).exists(),
            "the plain copy goes"
        );
    }

    #[test]
    fn keep_bounds_the_generations_and_deletes_the_oldest() {
        let dir = tempfile::tempdir().expect("tempdir");
        seed_generations(dir.path(), 1..=6, |second| format!("gen{second}\n"));
        let base = live_log(dir.path(), "web-0-out.log", "live\n");

        let tidied =
            tidy(&base, &config(Naming::Dated, 2, false), &FileSet::default()).expect("tidied");

        assert_eq!(tidied.deleted, 4);
        assert!(generation(dir.path(), 6, false).exists());
        assert!(generation(dir.path(), 5, false).exists());
        assert!(!generation(dir.path(), 4, false).exists());
        assert!(
            dir.path().join("web-0-out.log").exists(),
            "the live file is never a generation"
        );
    }

    #[test]
    fn pruning_never_deletes_a_file_this_dog_did_not_create() {
        let dir = tempfile::tempdir().expect("tempdir");
        seed_generations(dir.path(), 1..=6, |_| "ours\n");
        // Every one of these is a plausible near miss. None was written here.
        let decoys = [
            "web-0-out.2026-08-20.log",
            "web-0-out.2026-08-20T15-04-05.log.bak",
            "web-0-out-2026-08-20T15-04-05.log",
            "web-1-out.2026-08-20T15-04-05.log",
            "web-0-out.log.1",
            "important.log",
        ];
        for decoy in decoys {
            seed(dir.path(), decoy, "not ours\n");
        }
        let base = live_log(dir.path(), "web-0-out.log", "live\n");

        tidy(&base, &config(Naming::Dated, 1, true), &FileSet::default()).expect("tidied");

        for decoy in decoys {
            assert!(
                dir.path().join(decoy).exists(),
                "{decoy} was deleted and must not have been"
            );
        }
    }

    #[test]
    fn numeric_pruning_deletes_above_keep() {
        let dir = tempfile::tempdir().expect("tempdir");
        for n in 1..=6 {
            seed(
                dir.path(),
                format!("web-0-out.log.{n}"),
                format!("gen{n}\n"),
            );
        }
        let base = live_log(dir.path(), "web-0-out.log", "live\n");

        tidy(
            &base,
            &config(Naming::Numeric, 3, false),
            &FileSet::default(),
        )
        .expect("tidied");

        assert!(dir.path().join("web-0-out.log.3").exists());
        assert!(!dir.path().join("web-0-out.log.4").exists());
        assert!(!dir.path().join("web-0-out.log.6").exists());
    }

    #[test]
    fn switching_scheme_leaves_the_other_schemes_files_alone() {
        // Documented behaviour, not an accident: they stop being pruned and
        // are left for the operator. Deleting files a previous configuration
        // created is not this dog's call to make.
        let dir = tempfile::tempdir().expect("tempdir");
        for n in 1..=9 {
            seed(dir.path(), format!("web-0-out.log.{n}"), "old scheme\n");
        }
        let base = live_log(dir.path(), "web-0-out.log", "live\n");

        tidy(&base, &config(Naming::Dated, 1, true), &FileSet::default()).expect("tidied");

        for n in 1..=9 {
            assert!(
                dir.path().join(format!("web-0-out.log.{n}")).exists(),
                "left for the operator"
            );
        }
    }

    #[test]
    fn an_already_compressed_generation_is_not_compressed_twice() {
        let dir = tempfile::tempdir().expect("tempdir");
        seed(dir.path(), generation(dir.path(), 5, true), "already\n");
        seed(dir.path(), generation(dir.path(), 6, false), "newest\n");
        let base = live_log(dir.path(), "web-0-out.log", "live\n");

        let tidied =
            tidy(&base, &config(Naming::Dated, 5, true), &FileSet::default()).expect("tidied");

        assert_eq!(tidied.compressed, 0);
        assert!(
            !dir.path()
                .join("web-0-out.2026-08-20T15-04-05.log.gz.gz")
                .exists()
        );
    }

    #[test]
    fn compression_off_leaves_everything_plain() {
        let dir = tempfile::tempdir().expect("tempdir");
        seed_generations(dir.path(), 5..=6, |_| "body\n");
        let base = live_log(dir.path(), "web-0-out.log", "live\n");

        let tidied =
            tidy(&base, &config(Naming::Dated, 5, false), &FileSet::default()).expect("tidied");

        assert_eq!(tidied.compressed, 0);
        assert!(generation(dir.path(), 5, false).exists());
    }

    #[test]
    fn a_protected_live_log_is_never_compressed_or_deleted() {
        // `/var/log/web` has no extension, so under numeric naming it
        // generates the exact names `web.1`, `web.2` and so on. Every one of
        // those may be the live log of a different sheep, whose configured
        // path is literally `/var/log/web.2`. Both readings of the name are
        // correct and the matcher cannot choose between them, so the caller
        // hands over the paths the shepherd says it is writing to.
        //
        // `web.2` sits at index 1 here, which is where the test bites: with
        // `keep = 1` and compression on, index 1 would otherwise be gzipped
        // and then deleted on this same pass.
        let dir = tempfile::tempdir().expect("tempdir");
        for n in 1..=5 {
            seed(dir.path(), format!("web.{n}"), format!("gen{n}\n"));
        }
        let base = live_log(dir.path(), "web", "live\n");
        let live_elsewhere = dir.path().join("web.2");
        let protected = FileSet::from_paths([live_elsewhere.as_path()]);

        let tidied = tidy(&base, &config(Naming::Numeric, 1, true), &protected).expect("tidied");

        assert_eq!(tidied.skipped_protected, 1);
        assert_eq!(
            fs::read_to_string(&live_elsewhere).expect("a live log must survive"),
            "gen2\n",
            "contents intact"
        );
        assert!(
            !dir.path().join("web.2.gz").exists(),
            "a live log must not be compressed either"
        );
        // Sparing it must not cost a real generation its place: `keep = 1`
        // still means one of this dog's own generations, and `web.1` is it.
        assert!(dir.path().join("web.1").exists(), "the newest stays plain");
        for n in 3..=5 {
            assert!(!dir.path().join(format!("web.{n}")).exists());
            assert!(!dir.path().join(format!("web.{n}.gz")).exists());
        }
    }

    #[test]
    fn a_protected_generation_past_keep_survives_the_prune() {
        // The same guard with compression off, so nothing but the delete
        // path can be what spares the file.
        let dir = tempfile::tempdir().expect("tempdir");
        seed_generations(dir.path(), 1..=4, |second| format!("gen{second}\n"));
        let base = live_log(dir.path(), "web-0-out.log", "live\n");
        let oldest = generation(dir.path(), 1, false);
        let protected = FileSet::from_paths([oldest.as_path()]);

        let tidied = tidy(&base, &config(Naming::Dated, 1, false), &protected).expect("tidied");

        assert_eq!(tidied.skipped_protected, 1);
        assert_eq!(tidied.deleted, 2, "the two genuine generations past keep");
        assert_eq!(fs::read_to_string(&oldest).expect("survives"), "gen1\n");
    }

    #[test]
    fn a_live_log_is_not_truncated_by_being_some_generations_gz_target() {
        // Compression creates its target, which truncates whatever is
        // already there. If that target is a live log, truncating it is the
        // same harm as deleting it, so it gets the same refusal and the
        // plain generation is simply left uncompressed.
        let dir = tempfile::tempdir().expect("tempdir");
        seed(dir.path(), generation(dir.path(), 1, false), "ours\n");
        let live_elsewhere = generation(dir.path(), 1, true);
        fs::write(&live_elsewhere, "somebody is writing here\n").expect("seeded");
        seed(dir.path(), generation(dir.path(), 2, false), "newest\n");
        let base = live_log(dir.path(), "web-0-out.log", "live\n");
        let protected = FileSet::from_paths([live_elsewhere.as_path()]);

        let tidied = tidy(&base, &config(Naming::Dated, 5, true), &protected).expect("tidied");

        assert_eq!(tidied.compressed, 0);
        assert_eq!(
            fs::read_to_string(&live_elsewhere).expect("untouched"),
            "somebody is writing here\n"
        );
        assert!(
            generation(dir.path(), 1, false).exists(),
            "and the generation it could not compress stays plain"
        );
        // Two refusals over one file, because that file reaches this module
        // twice: once as a candidate generation of its own, and once as the
        // name a different generation wanted to write to. The count is of
        // refusals rather than of files, and either one alone is worth the
        // caller's attention.
        assert_eq!(tidied.skipped_protected, 2);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_shaped_like_a_generation_is_never_followed() {
        // Following it would compress and then delete a file in a directory
        // this dog was never pointed at.
        let dir = tempfile::tempdir().expect("tempdir");
        let elsewhere = dir.path().join("elsewhere.log");
        fs::write(&elsewhere, "somebody else's data\n").expect("seeded");
        let link = generation(dir.path(), 1, false);
        std::os::unix::fs::symlink(&elsewhere, &link).expect("linked");
        seed_generations(dir.path(), 2..=4, |_| "ours\n");
        let base = live_log(dir.path(), "web-0-out.log", "live\n");

        tidy(&base, &config(Naming::Dated, 1, true), &FileSet::default()).expect("tidied");

        assert!(
            fs::symlink_metadata(&link).is_ok(),
            "the link itself must still be there"
        );
        assert_eq!(
            fs::read_to_string(&elsewhere).expect("the target survives"),
            "somebody else's data\n"
        );
        assert!(
            !generation(dir.path(), 1, true).exists(),
            "and it was not compressed through the link"
        );
    }

    #[test]
    fn a_directory_shaped_like_a_generation_is_never_deleted() {
        let dir = tempfile::tempdir().expect("tempdir");
        let collision = generation(dir.path(), 1, false);
        fs::create_dir(&collision).expect("created");
        fs::write(collision.join("notes.txt"), "an operator's own\n").expect("seeded");
        seed_generations(dir.path(), 2..=4, |_| "ours\n");
        let base = live_log(dir.path(), "web-0-out.log", "live\n");

        tidy(&base, &config(Naming::Dated, 1, true), &FileSet::default()).expect("tidied");

        assert!(collision.is_dir(), "still a directory");
        assert_eq!(
            fs::read_to_string(collision.join("notes.txt")).expect("still there"),
            "an operator's own\n"
        );
        assert!(!generation(dir.path(), 1, true).exists());
    }

    #[test]
    fn a_generation_that_will_not_compress_does_not_stop_the_prune() {
        // The .gz target of the middle generation is a directory, so its
        // compression fails. The older generation still compresses, and with
        // keep = 1 both are still pruned: the fault is the one generation's
        // and the pass is not.
        let dir = tempfile::tempdir().expect("tempdir");
        seed_generations(dir.path(), 1..=3, |second| format!("gen{second}\n"));
        let blocker = generation(dir.path(), 2, true);
        fs::create_dir(&blocker).expect("blocker");
        let base = live_log(dir.path(), "web-0-out.log", "live\n");

        let tidied =
            tidy(&base, &config(Naming::Dated, 1, true), &FileSet::default()).expect("tidied");

        assert_eq!(tidied.faults.len(), 1, "{:?}", tidied.faults);
        assert!(
            tidied.faults[0].contains("15-04-02.log.gz"),
            "{:?}",
            tidied.faults
        );
        assert_eq!(
            tidied.compressed, 1,
            "the generation past the fault still compressed"
        );
        assert_eq!(tidied.deleted, 2, "and keep = 1 still pruned both");
        assert!(generation(dir.path(), 3, false).exists());
        assert!(!generation(dir.path(), 2, false).exists());
        assert!(!generation(dir.path(), 1, true).exists());
        assert!(blocker.is_dir(), "the blocker is not this dog's to remove");
    }

    #[test]
    fn debug_carries_the_fault_paths() {
        // The derived shape is the documented decision: the path is what an
        // operator is shown on the summary line, so Debug shows it too.
        let tidied = Tidied {
            faults: vec!["/var/log/web-0-out.2026-08-20T15-04-01.log.gz: is a symlink".into()],
            ..Tidied::default()
        };
        let shown = format!("{tidied:?}");
        assert!(
            shown.contains("/var/log/web-0-out.2026-08-20T15-04-01.log.gz"),
            "{shown}"
        );
    }

    #[test]
    fn keep_larger_than_the_generation_count_deletes_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        seed_generations(dir.path(), 1..=3, |_| "ours\n");
        let base = live_log(dir.path(), "web-0-out.log", "live\n");

        let tidied = tidy(
            &base,
            &config(Naming::Dated, 10, false),
            &FileSet::default(),
        )
        .expect("tidied");

        assert_eq!(tidied.deleted, 0);
        for second in 1..=3 {
            assert!(generation(dir.path(), second, false).exists());
        }
    }

    #[test]
    fn a_base_with_nothing_to_tidy_is_not_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let base = live_log(dir.path(), "web-0-out.log", "live\n");

        let tidied =
            tidy(&base, &config(Naming::Dated, 5, true), &FileSet::default()).expect("tidied");

        assert_eq!(tidied, Tidied::default());
        assert!(
            dir.path().join("web-0-out.log").exists(),
            "and the live file is still there"
        );

        // A sheep that is registered but never started has no log directory
        // at all yet. That is not a failure either.
        let never_started = dir.path().join("not-yet").join("web-0-out.log");
        let base = LogPath::split(&never_started).expect("splits");

        let tidied =
            tidy(&base, &config(Naming::Dated, 5, true), &FileSet::default()).expect("tidied");

        assert_eq!(tidied, Tidied::default());
    }

    #[test]
    fn a_crash_between_the_gz_and_the_remove_is_finished_on_the_next_pass() {
        // Compression writes `{path}.gz` and only then removes the plain
        // file, so a crash in between leaves both. Both read as the same
        // generation, so both come back from the listing, and this pass
        // finishes what the last one started.
        let dir = tempfile::tempdir().expect("tempdir");
        let body = "the log that nearly got away\n";
        seed(dir.path(), generation(dir.path(), 1, false), body);
        seed(
            dir.path(),
            generation(dir.path(), 1, true),
            "half a gzip stream",
        );
        // A newer generation, so the interrupted pair cannot land on index 0
        // and be spared as "the newest stays plain" whichever order the
        // directory happens to list them in.
        seed(dir.path(), generation(dir.path(), 2, false), "newest\n");
        let base = live_log(dir.path(), "web-0-out.log", "live\n");

        let tidied =
            tidy(&base, &config(Naming::Dated, 5, true), &FileSet::default()).expect("tidied");

        assert_eq!(tidied.compressed, 1, "the plain half, and only it");
        assert!(!generation(dir.path(), 1, false).exists());
        assert_eq!(
            gunzip(&generation(dir.path(), 1, true)),
            body,
            "the partial stream was replaced, not appended"
        );
    }

    #[test]
    fn a_half_compressed_generation_takes_one_slot_not_two() {
        // `X.log` and `X.log.gz` carry the SAME `Order`, so they are one
        // generation wearing two files, not two generations. Giving them a
        // slot each lets `keep` spare one of the two and delete the other,
        // which deletes the whole generation while reporting that two
        // survived. The twin state is not exotic: it is exactly what this
        // module's own compress-then-remove ordering leaves behind on a
        // crash, and the module documents recovering from it.
        let dir = tempfile::tempdir().expect("tempdir");
        let body = "the generation that keep = 2 promised to keep\n";
        seed(dir.path(), generation(dir.path(), 1, false), body);
        seed(
            dir.path(),
            generation(dir.path(), 1, true),
            "half a gzip stream",
        );
        seed(dir.path(), generation(dir.path(), 2, false), "newest\n");
        let base = live_log(dir.path(), "web-0-out.log", "live\n");

        let tidied =
            tidy(&base, &config(Naming::Dated, 2, true), &FileSet::default()).expect("tidied");

        assert_eq!(tidied.compressed, 1, "one generation, one compression");
        assert_eq!(tidied.deleted, 0, "two generations, and keep = 2");
        assert!(
            generation(dir.path(), 2, false).exists(),
            "the newest survives"
        );
        let survivor = generation(dir.path(), 1, true);
        assert!(
            survivor.exists(),
            "and so does the older one keep = 2 promised"
        );
        assert_eq!(
            gunzip(&survivor),
            body,
            "with its bytes, not a leftover partial stream"
        );
    }

    #[test]
    fn a_half_compressed_generation_past_keep_is_removed_once_not_twice() {
        // The same twin, this time doomed. Two slots for one generation
        // means the same path is removed twice, and the second removal fails
        // `NotFound` and aborts the pass. Every older generation then goes
        // unpruned, on this tick and on every tick after it.
        let dir = tempfile::tempdir().expect("tempdir");
        seed(dir.path(), generation(dir.path(), 1, false), "doomed\n");
        seed(
            dir.path(),
            generation(dir.path(), 1, true),
            "half a gzip stream",
        );
        seed(dir.path(), generation(dir.path(), 2, false), "newest\n");
        let base = live_log(dir.path(), "web-0-out.log", "live\n");

        let tidied = tidy(&base, &config(Naming::Dated, 1, true), &FileSet::default())
            .expect("the pass must not abort");

        assert_eq!(tidied.deleted, 1, "one file left of one generation");
        // A second removal of the same path would fail NotFound, and that
        // failure is recorded rather than returned now, so the count alone
        // no longer proves the twin was removed once.
        assert!(tidied.faults.is_empty(), "{:?}", tidied.faults);
        assert!(generation(dir.path(), 2, false).exists());
        assert!(!generation(dir.path(), 1, false).exists());
        assert!(!generation(dir.path(), 1, true).exists());
    }

    #[test]
    fn both_halves_of_a_half_compressed_generation_go_together() {
        // With compression off nothing collapses the twin down to one file,
        // so the generation is past `keep` while still wearing both. Sparing
        // either half would leave a file that no later pass can ever reach:
        // it would be the same one generation every time, and one generation
        // is one slot.
        let dir = tempfile::tempdir().expect("tempdir");
        let plain = generation(dir.path(), 1, false);
        let gz = generation(dir.path(), 1, true);
        fs::write(&plain, "doomed\n").expect("seeded");
        fs::write(&gz, "also doomed\n").expect("seeded");
        seed(dir.path(), generation(dir.path(), 2, false), "newest\n");
        let base = live_log(dir.path(), "web-0-out.log", "live\n");

        let tidied =
            tidy(&base, &config(Naming::Dated, 1, false), &FileSet::default()).expect("tidied");

        assert_eq!(tidied.deleted, 2, "two files, one generation");
        assert!(!plain.exists());
        assert!(!gz.exists(), "no half left behind to leak forever");
        assert!(generation(dir.path(), 2, false).exists());
    }

    #[test]
    fn a_protected_path_spelled_with_a_parent_component_is_still_protected() {
        // `PathBuf` equality normalises away `.` but not `..`, so comparing
        // whole paths lets this spelling through the guard with nothing said
        // about it. A guard that silently does nothing is worse than no
        // guard, because the caller believes the file is safe.
        let dir = tempfile::tempdir().expect("tempdir");
        fs::create_dir(dir.path().join("sub")).expect("created");
        seed_generations(dir.path(), 1..=4, |second| format!("gen{second}\n"));
        let base = live_log(dir.path(), "web-0-out.log", "live\n");
        let spelled_the_long_way = dir
            .path()
            .join("sub")
            .join("..")
            .join(generation_name(1, false));
        let protected = FileSet::from_paths([spelled_the_long_way.as_path()]);

        let tidied = tidy(&base, &config(Naming::Dated, 1, false), &protected).expect("tidied");

        assert_eq!(tidied.skipped_protected, 1);
        assert_eq!(
            fs::read_to_string(generation(dir.path(), 1, false)).expect("survives"),
            "gen1\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_protected_path_through_a_symlinked_directory_is_still_protected() {
        // A log directory reached through a symlink is the ordinary case,
        // not a trick: /var/log is one on macOS. The caller and this module
        // can easily be handed the two different spellings of the same
        // directory, and the guard has to hold across them.
        let dir = tempfile::tempdir().expect("tempdir");
        let real = dir.path().join("real");
        fs::create_dir(&real).expect("created");
        let through = dir.path().join("through");
        std::os::unix::fs::symlink(&real, &through).expect("linked");
        seed_generations(&real, 1..=4, |second| format!("gen{second}\n"));
        let base = live_log(&real, "web-0-out.log", "live\n");
        let protected = FileSet::from_paths([through.join(generation_name(1, false)).as_path()]);

        let tidied = tidy(&base, &config(Naming::Dated, 1, false), &protected).expect("tidied");

        assert_eq!(tidied.skipped_protected, 1);
        assert_eq!(
            fs::read_to_string(generation(&real, 1, false)).expect("survives"),
            "gen1\n"
        );
    }
}
