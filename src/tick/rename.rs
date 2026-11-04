//! One sheep's turn at the rename loop: rename what may be renamed, and say
//! what happened.
//!
//! `tick` hands this the bases `judge::survey` picked out for one sheep and
//! takes back what the turn did. What follows the turn is not part of it. The
//! reopen that follows is a request to the shepherd, and this module makes no
//! request: a fault on disk is that one log's problem and the turn goes on to
//! the next base, while a refused reopen stops the whole tick, which only
//! `tick` can see.
//!
//! The state a turn carries belongs to the tick rather than to the sheep,
//! because two sheep can write to one log path. `renamed` is what an earlier
//! sheep has already moved and `faulted` what an earlier sheep failed to move,
//! and both outlive the turn that wrote them. `renamed` is what tells a later
//! sheep its file is already gone and that it still needs its reopen.
//! `faulted` is what holds a file back from tidy however many sheep reopened.

use std::{collections::BTreeMap, time::SystemTime};

use crate::{
    config::Config,
    file_set::{FileSet, ResolvedDir},
    naming::LogPath,
    rotate::rotate,
};

use super::{judge::collides_with_a_live_log, report::Report};

/// What one tick's rename phase carries from one sheep's turn to the next.
pub(super) struct Renames<'a, 'b> {
    /// Every live log path the flock reported.
    ///
    /// A base is resolved through this set rather than by
    /// [`ResolvedDir::of`](crate::file_set::ResolvedDir::of), so it is
    /// resolved the way the set itself was, at the moment it was built, and
    /// the rename guard asks against the same set
    /// [`prune::tidy`](crate::prune::tidy) protects.
    pub(super) protected: &'a FileSet,
    /// The rules of every sheep that writes to each log file, keyed by the
    /// resolved directory and the live name. Read for `keep` alone: the
    /// renaming sheep names and compresses by its own rules.
    pub(super) owners: &'a BTreeMap<(ResolvedDir, String), Vec<&'b Config>>,
    /// The instant this tick runs at, which is what a generation's name is
    /// built from.
    pub(super) now: SystemTime,
    /// The log paths this tick has already renamed, by any sheep.
    ///
    /// A `FileSet` rather than a set of paths, for the same reason
    /// `protected` is one: two sheep can be handed one file under two
    /// spellings, and a second rename of a file that has already moved fails
    /// with NotFound and leaves the second sheep never reopened.
    pub(super) renamed: FileSet,
    /// Files some sheep failed to rename this tick.
    ///
    /// A later sheep sharing the file can succeed where that one failed, and
    /// the first sheep, reopened before the file moved or never reopened at
    /// all, then writes into the renamed generation through its old
    /// descriptor. Such a file is held back from tidy however many sheep did
    /// reopen it.
    pub(super) faulted: FileSet,
}

/// What one sheep's turn did.
pub(super) struct Turn {
    /// Whether this sheep needs its reopen: the turn renamed one of its own
    /// logs, or an earlier sheep renamed a log this one writes to.
    pub(super) rotated_any: bool,
    /// The bases this turn renamed, each with the rules its generations will
    /// be tidied under.
    pub(super) rotated: Vec<(LogPath, Config)>,
}

impl<'a, 'b> Renames<'a, 'b> {
    /// Rename every base in `bases` that may be renamed, on `rules`, and add
    /// what happened to `report`.
    ///
    /// `stopping` is the caller's answer to its own stop check, taken before
    /// the turn rather than asked here. Whether a turn may start a rename is a
    /// question about the shepherd's kill ladder, and the answer has to be
    /// taken once, between sheep, where nothing is waiting on a reopen. A
    /// sheep's own renames always run to completion.
    ///
    /// A base an earlier sheep already renamed is counted even under
    /// `stopping`, and that order is load-bearing: the sheep sharing the file
    /// is writing into the renamed generation through its own descriptor, and
    /// only this dog asks shep to reopen it. Pinned by
    /// `a_stop_requested_after_a_rename_still_reopens_the_sheep_sharing_that_file`.
    pub(super) fn for_sheep(
        &mut self,
        bases: &[LogPath],
        rules: &Config,
        stopping: bool,
        report: &mut Report,
    ) -> Turn {
        let mut rotated_any = false;
        let mut rotated: Vec<(LogPath, Config)> = Vec::new();
        for base in bases {
            let dir = self.protected.resolve(&base.dir);
            let live_name = base.live_name();
            if self.renamed.contains(&dir, &live_name) {
                // Another sheep shares this path and has already rotated it.
                // This one is still writing through its own descriptor, so it
                // still needs the reopen `tick` makes after this returns.
                rotated_any = true;
                continue;
            }
            if stopping {
                continue;
            }
            if collides_with_a_live_log(base, &dir, rules.naming, self.protected) {
                report.skipped_collision += 1;
                continue;
            }
            match rotate(base, rules.naming, self.now) {
                Ok(_generation) => {
                    // The generations are pruned to the most any owner of
                    // the file asked to keep, so no sheep sharing it loses
                    // history it asked for. The renaming sheep's own rules
                    // otherwise stand: it names and compresses.
                    let keep = self
                        .owners
                        .get(&(dir.clone(), live_name.clone()))
                        .and_then(|owned| owned.iter().map(|owner| owner.keep).max())
                        .unwrap_or(rules.keep);
                    rotated.push((
                        base.clone(),
                        Config {
                            keep,
                            ..rules.clone()
                        },
                    ));
                    self.renamed.insert(dir, live_name);
                    report.rotated += 1;
                    rotated_any = true;
                }
                // A rename that fails leaves the live file where it was, so
                // this log is simply not rotated this tick. That is this
                // log's problem, said so, and no reason to leave the next
                // one alone.
                Err(err) => {
                    report.rename_failed.push(err.to_string());
                    self.faulted.insert(dir, live_name);
                }
            }
        }
        Turn {
            rotated_any,
            rotated,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{Fake, run, sheep};
    use super::*;
    use std::fs;
    #[tokio::test]
    async fn two_sheep_sharing_one_log_path_rotate_it_once_and_both_reopen() {
        let dir = tempfile::tempdir().expect("tempdir");
        let shared = dir.path().join("web-out.log");
        fs::write(&shared, "x".repeat(2048)).expect("seeded");
        let fake = Fake::new(
            "max_size = \"1K\"\n",
            vec![
                sheep("web-0", Some(&shared), None),
                sheep("web-1", Some(&shared), None),
            ],
        );

        let (_config, report) = run(&fake, SystemTime::now()).await.expect("ticked");

        assert_eq!(report.rotated, 1, "one path, one rename");
        let reopened = fake.reopened.borrow().clone();
        assert!(reopened.contains(&"web-0".to_owned()));
        assert!(reopened.contains(&"web-1".to_owned()));
    }

    #[tokio::test]
    async fn a_rename_that_fails_still_reopens_the_sheep_it_half_rotated() {
        // Propagating the failure straight out would leave the file that DID
        // rotate with no reopen behind it, and shep writing into a renamed
        // file that no later tick ever revisits.
        let dir = tempfile::tempdir().expect("tempdir");
        let out = dir.path().join("api-0-out.log");
        let err = dir.path().join("api-0-err.log");
        fs::write(&out, "x".repeat(2048)).expect("seeded");
        fs::write(&err, "x".repeat(2048)).expect("seeded");
        // The last generation number there is, so the shift has nowhere to
        // go and `rotate` refuses rather than wrapping.
        fs::write(dir.path().join("api-0-err.log.4294967295"), "oldest").expect("seeded");
        let fake = Fake::new(
            "max_size = \"1K\"\nnaming = \"numeric\"\n",
            vec![sheep("api", Some(&out), Some(&err))],
        );

        let (_config, report) = run(&fake, SystemTime::now())
            .await
            .expect("a rename fault is reported, not returned as an error");

        let why = report
            .rename_failed
            .first()
            .expect("the fault is in the report");
        assert!(why.contains("api-0-err.log"), "{why}");
        assert!(why.contains("no generation numbers left"), "{why}");
        assert_eq!(
            *fake.reopened.borrow(),
            vec!["api".to_owned()],
            "the half that rotated still needs its reopen"
        );
        assert!(dir.path().join("api-0-out.log.1").exists());
        assert!(
            report.summary().expect("a line").contains("to rename"),
            "the fault reaches the summary line"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn two_sheep_sharing_one_log_under_two_spellings_rotate_it_once_and_both_reopen() {
        // The same file, reached through a symlinked directory by one sheep
        // and directly by the other. Path equality reads those as two files,
        // so the second rename ran against a path the first had already
        // moved, failed NotFound, and the second sheep was never reopened:
        // it went on writing into the renamed generation for good.
        let dir = tempfile::tempdir().expect("tempdir");
        let real = dir.path().join("real");
        fs::create_dir(&real).expect("real");
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).expect("symlink");
        let through_link = link.join("web-out.log");
        let direct = real.join("web-out.log");
        fs::write(&direct, "x".repeat(2048)).expect("seeded");
        let fake = Fake::new(
            "max_size = \"1K\"\n",
            vec![
                sheep("web-0", Some(&through_link), None),
                sheep("web-1", Some(&direct), None),
            ],
        );

        let (_config, report) = run(&fake, SystemTime::now())
            .await
            .expect("one file under two spellings is one rotation, not an error");

        assert_eq!(report.rotated, 1, "one file, one rename");
        let reopened = fake.reopened.borrow().clone();
        assert!(reopened.contains(&"web-0".to_owned()));
        assert!(
            reopened.contains(&"web-1".to_owned()),
            "the second sheep still holds the old descriptor and needs its reopen"
        );
    }

    #[tokio::test]
    async fn a_rename_fault_does_not_stop_the_sheeps_other_logs() {
        // beta's out log faults. Its err log is alpha's out log, which alpha
        // rotated: beta still reaches it, counts as rotated through it, and
        // is reopened, so the shared log is safe to tidy after all.
        let dir = tempfile::tempdir().expect("tempdir");
        let shared = dir.path().join("shared.log");
        let betas_out = dir.path().join("beta-0-out.log");
        fs::write(&shared, "x".repeat(2048)).expect("seeded");
        fs::write(&betas_out, "x".repeat(2048)).expect("seeded");
        fs::write(dir.path().join("shared.log.1"), "old\n").expect("seeded");
        fs::write(dir.path().join("beta-0-out.log.4294967295"), "oldest").expect("seeded");
        let fake = Fake::new(
            "max_size = \"1K\"\nnaming = \"numeric\"\nkeep = 1\ncompress = false\n",
            vec![
                sheep("alpha", Some(&shared), None),
                sheep("beta", Some(&betas_out), Some(&shared)),
            ],
        );

        let (_config, report) = run(&fake, SystemTime::now()).await.expect("ticked");

        assert_eq!(
            report.rename_failed.len(),
            1,
            "beta's out log could not rotate"
        );
        assert_eq!(
            *fake.reopened.borrow(),
            vec!["alpha".to_owned(), "beta".to_owned()],
            "beta shares the rotated log, so it is reopened for it"
        );
        assert_eq!(
            report.deleted, 1,
            "both sheep reopened, so shared.log.2 went"
        );
        assert!(
            betas_out.exists(),
            "the log that could not rotate is where it was"
        );
    }

    #[tokio::test]
    async fn a_rename_that_fails_still_tidies_what_did_reopen() {
        // api's out log rotates, its err log cannot (no generation numbers
        // left), api is reopened for the half that moved. The out log's
        // generations are then safe to tidy, and the rename fault is still
        // the error the tick returns.
        let dir = tempfile::tempdir().expect("tempdir");
        let out = dir.path().join("api-0-out.log");
        let err = dir.path().join("api-0-err.log");
        fs::write(&out, "x".repeat(2048)).expect("seeded");
        fs::write(&err, "x".repeat(2048)).expect("seeded");
        fs::write(dir.path().join("api-0-out.log.1"), "old\n").expect("seeded");
        fs::write(dir.path().join("api-0-err.log.4294967295"), "oldest").expect("seeded");
        let fake = Fake::new(
            "max_size = \"1K\"\nnaming = \"numeric\"\nkeep = 1\ncompress = false\n",
            vec![sheep("api", Some(&out), Some(&err))],
        );

        let (_config, report) = run(&fake, SystemTime::now())
            .await
            .expect("a rename fault is reported, not returned as an error");

        assert_eq!(report.rename_failed.len(), 1);
        assert_eq!(*fake.reopened.borrow(), vec!["api".to_owned()]);
        assert!(
            dir.path().join("api-0-out.log.1").exists(),
            "the live file moved to .1"
        );
        assert!(
            !dir.path().join("api-0-out.log.2").exists(),
            "the old generation, shifted to .2, was past keep = 1 and went"
        );
    }
}
