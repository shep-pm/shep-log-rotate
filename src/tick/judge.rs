//! What a tick settles before it renames anything: the rules each sheep runs
//! on, and one verdict on each log file.
//!
//! This is what a tick decides, not what it does. `tick` asks the shepherd for
//! its section and for every sheep's own table, hands both here, and then
//! renames, reopens, and tidies on the answers.
//!
//! Two rules here reach past this module. A log several sheep share is stat-ed
//! once and judged once, against every owner's rules, so no sheep is left out
//! of the rename loop writing into a file another one moved. And every verdict
//! is settled before any rename starts, so each owner sees the file as it was.

use core::time::Duration;
use std::{
    collections::{BTreeMap, BTreeSet, btree_map::Entry},
    fs,
    path::Path,
    time::SystemTime,
};

use shep_client::shep_core::{config::DogTable, protocol::ProcessInfo};

use crate::{
    config::{Config, Naming},
    file_set::{FileSet, ResolvedDir},
    naming::{LogPath, match_generation},
    rotate::{last_rotation, regular_files},
};

use super::report::Report;

/// What [`sheep_rules`] settled for the flock.
pub(super) struct Rules {
    /// The settings of each sheep whose own table parsed, by sheep name. A
    /// sheep with no table is absent, and rotates on the section's.
    pub(super) settings: BTreeMap<String, Config>,
    /// The sheep whose own table did not parse.
    pub(super) left_alone: BTreeSet<String>,
}

/// Lay each sheep's own table over `section`, once per tick.
///
/// A table that does not parse leaves that sheep alone this tick and says
/// so in `report`. It is that sheep's problem, so the others still rotate. A
/// table on a name the flock does not hold is ignored: there is nothing it
/// could rotate, and nothing it could leave alone.
pub(super) fn sheep_rules(
    config: &Config,
    section: &DogTable,
    tables: &BTreeMap<String, DogTable>,
    flock: &[ProcessInfo],
    report: &mut Report,
) -> Rules {
    let mut rules = Rules {
        settings: BTreeMap::new(),
        left_alone: BTreeSet::new(),
    };
    for (name, own) in tables {
        if !flock.iter().any(|sheep| sheep.name == *name) {
            continue;
        }
        match config.for_sheep(section, own) {
            Ok(settings) => {
                rules.settings.insert(name.clone(), settings);
            }
            Err(why) => {
                report.left_alone.push(format!("{name} alone ({why})"));
                rules.left_alone.insert(name.clone());
            }
        }
    }
    rules
}

/// What one tick decided about one log file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Verdict {
    /// Grown or aged past what the config allows.
    Rotate,
    /// Not yet.
    Leave,
    /// No log file to look at: a sheep registered but never started.
    Skip,
}

/// [`qualifies`]'s answer: a verdict, or the one time a directory refused to
/// be listed, with the reason.
enum Listed {
    Verdict(Verdict),
    Unlistable(String),
}

/// The rules of every sheep that writes to each log file, keyed the way the
/// tick keys a file: the directory as `protected` resolves it, and the live
/// name.
///
/// A sheep with no table of its own is an owner on the section's rules. A
/// sheep left alone is one too, on those same rules, but its files are held
/// and never reach a verdict, so what it says here is not read.
pub(super) fn owners_of<'a>(
    flock: &[ProcessInfo],
    settings: &'a BTreeMap<String, Config>,
    config: &'a Config,
    protected: &FileSet,
) -> BTreeMap<(ResolvedDir, String), Vec<&'a Config>> {
    let mut owners: BTreeMap<(ResolvedDir, String), Vec<&Config>> = BTreeMap::new();
    for sheep in flock {
        let rules = settings.get(&sheep.name).unwrap_or(config);
        for path in log_paths(sheep) {
            if let Some(base) = LogPath::split(Path::new(path)) {
                owners
                    .entry((protected.resolve(&base.dir), base.live_name()))
                    .or_default()
                    .push(rules);
            }
        }
    }
    owners
}

/// The one verdict for a log, judged against the rules of every sheep that
/// writes to it: the file rotates when any owner's rules say it should.
///
/// The first sheep in the flock's order used to decide alone, so a log's fate
/// depended on where its owners happened to be listed, and a sheep that asked
/// for a tighter `max_size` or `max_age` was ignored if another came first.
/// One owner wanting a rotation is reason enough, since declining it would
/// refuse that sheep what its own table asked for. Every owner is asked even
/// once one has said yes, so that a directory too unreadable to age a log by
/// is reported whichever order the flock lists its owners in. The file is
/// still stat-ed once, by the caller, and each owner reads it through the same
/// `metadata`.
fn judge(
    base: &LogPath,
    dir: &ResolvedDir,
    metadata: &fs::Metadata,
    owners: &[&Config],
    now: SystemTime,
    listings: &mut BTreeMap<ResolvedDir, Option<Vec<String>>>,
    report: &mut Report,
) -> Verdict {
    let mut verdict = Verdict::Leave;
    for rules in owners {
        match qualifies(base, dir, metadata, rules, now, listings) {
            // One directory this dog cannot read is that log's problem, said
            // so, and not a reason to leave every other log alone.
            Listed::Unlistable(why) => report.unlistable.push(why),
            Listed::Verdict(Verdict::Rotate) => verdict = Verdict::Rotate,
            Listed::Verdict(_) => {}
        }
    }
    verdict
}

/// Whether a log has grown or aged past what `config` allows.
///
/// An empty file never qualifies, whatever its age. Rotating one frees no
/// disk and produces an empty generation, and that generation then pushes a
/// real one past `keep` and has it deleted: a rotator destroying the history
/// it exists to keep. A sheep that logs nothing for a week is the ordinary
/// way to reach this, not a contrived one.
///
/// Age is counted from the last rotation, which the newest generation
/// records (see [`last_rotation`]). A log never rotated counts from when it
/// appeared, and where the filesystem keeps no birth time, from its last
/// write. Not from the last write first: an mtime can be older than the
/// rotation it followed, after a clock stepped back or a `touch`, and that
/// would rotate a log that was rotated an hour ago. The listing the age is
/// read from is taken once per directory per tick, whichever way the
/// directory is spelled, and a directory that refuses is asked once too:
/// [`Listed::Unlistable`] the first time, with the reason, and
/// [`Verdict::Leave`] for every later log in it. `dir` is `base.dir` as the
/// protected set resolves it.
fn qualifies(
    base: &LogPath,
    dir: &ResolvedDir,
    metadata: &fs::Metadata,
    config: &Config,
    now: SystemTime,
    listings: &mut BTreeMap<ResolvedDir, Option<Vec<String>>>,
) -> Listed {
    if metadata.len() == 0 {
        return Listed::Verdict(Verdict::Leave);
    }
    if metadata.len() >= config.max_size.bytes() {
        return Listed::Verdict(Verdict::Rotate);
    }
    let Some(max_age) = config.max_age else {
        return Listed::Verdict(Verdict::Leave);
    };
    let listed = match listings.entry(dir.clone()) {
        Entry::Vacant(slot) => match regular_files(&base.dir) {
            Ok(names) => slot.insert(Some(names)),
            Err(err) => {
                slot.insert(None);
                return Listed::Unlistable(err.to_string());
            }
        },
        Entry::Occupied(slot) => slot.into_mut(),
    };
    let Some(names) = listed else {
        return Listed::Verdict(Verdict::Leave);
    };
    let since = last_rotation(names, base, config.naming)
        .or_else(|| metadata.created().ok())
        .or_else(|| metadata.modified().ok());
    Listed::Verdict(if aged(since, now, max_age.as_duration()) {
        Verdict::Rotate
    } else {
        Verdict::Leave
    })
}

/// Whether `instant` is at least `max_age` before `now`.
///
/// `None` and an instant after `now` are both "not that old": a clock that
/// went backwards is not a reason to rotate.
fn aged(instant: Option<SystemTime>, now: SystemTime, max_age: Duration) -> bool {
    instant
        .and_then(|at| now.duration_since(at).ok())
        .is_some_and(|age| age >= max_age)
}

/// The log paths one sheep writes to: `out_file` and `err_file`, whichever
/// of them are set.
pub(super) fn log_paths(sheep: &ProcessInfo) -> impl Iterator<Item = &str> {
    [sheep.out_file.as_deref(), sheep.err_file.as_deref()]
        .into_iter()
        .flatten()
}

/// Whether rotating `base` could disturb a path some sheep is using as a
/// live log. `dir` is `base.dir`, resolved through `protected`.
///
/// A `true` here skips the base whole: not rotated, and so not tidied
/// either.
///
/// Asked by name against the flock's own list rather than by listing the
/// directory and intersecting. Every path `generations` could return and
/// find in `protected` is a name this matches, and asking by name picks up
/// two cases a listing cannot see:
///
/// - A sheep that is registered but stopped has no log file, so `generations`
///   cannot see its name. Rotating into it would hand that sheep a file full
///   of somebody else's log the moment it starts.
/// - `generations` deliberately skips symlinks and anything that is not a
///   regular file, so a live log that is a symlink is invisible to it.
///
/// It also costs one lookup instead of a `read_dir` per base.
///
/// Getting this wrong is worse than `tidy` getting its own guard wrong,
/// which is why both consult the same [`FileSet`] rather than each resolving
/// directories its own way. `tidy` declining to protect a file it should
/// have costs a deletion, which is visible. This guard declining costs a
/// rename of a file some other sheep has open, and shep goes on writing
/// into an inode with no name: that sheep's log simply stops appearing,
/// with nothing logged anywhere to say why. Measured against a real
/// shepherd before the two agreed on how a directory is spelled: two sheep
/// sharing one directory under two spellings, and one of them lost its live
/// log to the other's rotation on the first tick.
pub(super) fn collides_with_a_live_log(
    base: &LogPath,
    dir: &ResolvedDir,
    naming: Naming,
    protected: &FileSet,
) -> bool {
    protected
        .names_in(dir)
        .iter()
        .any(|name| match_generation(base, naming, name).is_some())
}

/// What [`survey`] settled for the flock, none of it renamed yet.
pub(super) struct Survey<'a> {
    /// The sheep with at least one log to rotate, in the flock's own order.
    pub(super) order: Vec<String>,
    /// The bases each sheep in `order` rotates.
    pub(super) groups: BTreeMap<String, Vec<LogPath>>,
    /// The rules of every sheep that writes to each log file. The rename
    /// phase reads it for `keep`.
    pub(super) owners: BTreeMap<(ResolvedDir, String), Vec<&'a Config>>,
}

/// Settle one verdict for every log path the flock reported, and collect the
/// sheep that have something to rotate.
///
/// Nothing here renames or deletes. `held` carries what the sheep left alone
/// write to, and no verdict is reached for any of it.
pub(super) fn survey<'a>(
    flock: &[ProcessInfo],
    settings: &'a BTreeMap<String, Config>,
    config: &'a Config,
    protected: &FileSet,
    held: &FileSet,
    now: SystemTime,
    report: &mut Report,
) -> Survey<'a> {
    // Qualify the whole flock before renaming anything. Two sheep can share
    // one log path (`merge_logs`, or an explicit `out_file` on a
    // multi-instance app), and both have to see it as it was.
    let mut order: Vec<String> = Vec::new();
    let mut groups: BTreeMap<String, Vec<LogPath>> = BTreeMap::new();
    // Each log directory's listing, read at most once per tick, for the
    // logs whose age has to be read off their newest generation. Keyed by
    // the resolved directory so two spellings of one are one listing.
    // `None` is a directory that refused to be listed, already reported.
    let mut listings: BTreeMap<ResolvedDir, Option<Vec<String>>> = BTreeMap::new();
    // One verdict per file, however many sheep write to it. Two sheep
    // sharing a log have to agree on whether it rotates: a file crossing
    // `max_size` between two stats would otherwise leave the first sheep
    // out of the loop below, never reopened, and writing into the file the
    // second one renamed. That race has no seam a test can drive, so the
    // one stat per file is what pins it.
    let mut verdicts: BTreeMap<(ResolvedDir, String), Verdict> = BTreeMap::new();
    // The rules of every sheep that writes to each file, so that the one
    // verdict is judged against all of them and not just the first sheep's.
    let owners = owners_of(flock, settings, config, protected);
    for sheep in flock {
        for path in log_paths(sheep) {
            // A path this dog cannot spell is a path it must not go on to
            // rename or delete. `LogPath::split` refuses a non-UTF-8 name,
            // and this is where that refusal gets its answer: skip it, count
            // it, and leave it for a human.
            let Some(base) = LogPath::split(Path::new(path)) else {
                report.skipped += 1;
                continue;
            };
            let file = (protected.resolve(&base.dir), base.live_name());
            if held.contains(&file.0, &file.1) {
                continue;
            }
            let verdict = match verdicts.get(&file) {
                Some(verdict) => *verdict,
                None => {
                    let verdict = match fs::metadata(base.live()) {
                        // A sheep registered but never started has no log
                        // file yet, and that is normal rather than broken.
                        Err(_) => Verdict::Skip,
                        Ok(metadata) => {
                            let rules = owners.get(&file).map_or(&[][..], Vec::as_slice);
                            judge(
                                &base,
                                &file.0,
                                &metadata,
                                rules,
                                now,
                                &mut listings,
                                &mut *report,
                            )
                        }
                    };
                    verdicts.insert(file, verdict);
                    verdict
                }
            };
            match verdict {
                Verdict::Rotate => {}
                Verdict::Leave => continue,
                Verdict::Skip => {
                    report.skipped += 1;
                    continue;
                }
            }
            if !groups.contains_key(&sheep.name) {
                order.push(sheep.name.clone());
            }
            let group = groups.entry(sheep.name.clone()).or_default();
            // `merge_logs` points `out_file` and `err_file` at one path.
            if !group.contains(&base) {
                group.push(base);
            }
        }
    }
    Survey {
        order,
        groups,
        owners,
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{Fake, a_year_on, run, sheep};
    use super::*;
    #[tokio::test]
    async fn max_age_rotates_a_small_but_old_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let out = dir.path().join("web-0-out.log");
        fs::write(&out, "tiny\n").expect("seeded");
        let fake = Fake::new(
            "max_size = \"1G\"\nmax_age = \"1s\"\n",
            vec![sheep("web", Some(&out), None)],
        );
        let (_config, report) = run(&fake, a_year_on()).await.expect("ticked");

        assert_eq!(report.rotated, 1);
    }

    // The two guards below are the reason a tick builds a protected set at
    // all. Both were mutation-checked: inverting the collision test in
    // `collides_with_a_live_log` makes each of them fail, on the file it
    // says shep is still writing into.

    #[tokio::test]
    async fn a_generation_slot_that_is_another_sheeps_live_log_is_left_alone() {
        // A base with no extension and a base whose extension is all digits
        // generate byte-identical names: `/dir/web` rotates into
        // `/dir/web.1`, which here is the live log of a different sheep.
        let dir = tempfile::tempdir().expect("tempdir");
        let grown = dir.path().join("web");
        let live = dir.path().join("web.1");
        fs::write(&grown, "x".repeat(2048)).expect("seeded");
        fs::write(&live, "one's live log\n").expect("seeded");
        let fake = Fake::new(
            "max_size = \"1K\"\nnaming = \"numeric\"\n",
            vec![
                sheep("web", Some(&grown), None),
                sheep("one", Some(&live), None),
            ],
        );

        let (_config, report) = run(&fake, SystemTime::now()).await.expect("ticked");

        assert_eq!(report.rotated, 0, "neither log may be renamed");
        assert_eq!(report.skipped_collision, 1, "the collision is reported");
        assert_eq!(
            fs::read_to_string(&live).expect("still there"),
            "one's live log\n",
            "renaming a live log out from under an open descriptor is worse than deleting it"
        );
        assert!(grown.exists(), "the grown log is left for a human to sort");
        assert!(fake.reopened.borrow().is_empty());
    }

    #[tokio::test]
    async fn a_generation_slot_reached_through_a_symlinked_directory_is_still_left_alone() {
        // The same collision, except the two sheep were handed the same
        // directory under two spellings: one through a symlink and one not.
        // Path equality reads those as different directories, so a guard
        // comparing `candidate.parent()` to `base.dir` as written sees no
        // collision at all and renames a file another sheep has open.
        //
        // Measured against a real shepherd before this resolved the
        // directory: the rename went through on the first tick, and the
        // sheep holding the open descriptor went on writing into an inode
        // that no longer had a name.
        let dir = tempfile::tempdir().expect("tempdir");
        let real = dir.path().join("real");
        fs::create_dir(&real).expect("real");
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).expect("symlink");

        let grown = link.join("web");
        let live = real.join("web.1");
        fs::write(&grown, "x".repeat(2048)).expect("seeded");
        fs::write(&live, "one's live log\n").expect("seeded");
        let fake = Fake::new(
            "max_size = \"1K\"\nnaming = \"numeric\"\n",
            vec![
                sheep("web", Some(&grown), None),
                sheep("one", Some(&live), None),
            ],
        );

        let (_config, report) = run(&fake, SystemTime::now()).await.expect("ticked");

        assert_eq!(report.rotated, 0, "neither log may be renamed");
        assert_eq!(
            report.skipped_collision, 1,
            "a symlinked directory is the same directory"
        );
        assert_eq!(
            fs::read_to_string(&live).expect("still there"),
            "one's live log\n"
        );
        assert!(fake.reopened.borrow().is_empty());
    }

    #[tokio::test]
    async fn a_generation_slot_claimed_by_a_sheep_that_never_started_is_left_alone() {
        // Same collision, except the other sheep is registered and stopped,
        // so its log file does not exist yet. Rotating into its name would
        // hand it a file full of somebody else's log the moment it starts.
        let dir = tempfile::tempdir().expect("tempdir");
        let grown = dir.path().join("web");
        let claimed = dir.path().join("web.1");
        fs::write(&grown, "x".repeat(2048)).expect("seeded");
        let fake = Fake::new(
            "max_size = \"1K\"\nnaming = \"numeric\"\n",
            vec![
                sheep("web", Some(&grown), None),
                sheep("one", Some(&claimed), None),
            ],
        );

        let (_config, report) = run(&fake, SystemTime::now()).await.expect("ticked");

        assert_eq!(report.rotated, 0);
        assert_eq!(report.skipped_collision, 1);
        assert!(!claimed.exists(), "the stopped sheep's name is still free");
        assert_eq!(report.skipped, 1, "the stopped sheep's own log is skipped");
    }

    #[tokio::test]
    async fn an_empty_log_is_never_rotated_however_old_it_is() {
        // Rotating an empty file frees nothing and produces an empty
        // generation, which then pushes a real one past `keep`.
        let dir = tempfile::tempdir().expect("tempdir");
        let out = dir.path().join("web-0-out.log");
        fs::write(&out, "").expect("seeded");
        let fake = Fake::new(
            "max_size = \"1G\"\nmax_age = \"1s\"\n",
            vec![sheep("web", Some(&out), None)],
        );
        let (_config, report) = run(&fake, a_year_on()).await.expect("ticked");

        assert_eq!(report.rotated, 0);
        assert!(out.exists());
        assert!(fake.reopened.borrow().is_empty());
    }

    #[tokio::test]
    async fn max_age_counts_from_the_last_rotation_not_the_last_write() {
        // A log written every second never ages by its mtime. What max_age
        // means is "this long since the last rotation", which for a dated
        // base is in the newest generation's name.
        let dir = tempfile::tempdir().expect("tempdir");
        let out = dir.path().join("web-0-out.log");
        fs::write(&out, "written just now\n").expect("seeded");
        let eight_days_ago = SystemTime::now() - core::time::Duration::from_secs(8 * 86_400);
        fs::write(
            dir.path().join(format!(
                "web-0-out.{}.log",
                crate::naming::stamp_utc(eight_days_ago)
            )),
            "the last rotation\n",
        )
        .expect("seeded");
        let fake = Fake::new(
            "max_size = \"1G\"\nmax_age = \"168h\"\n",
            vec![sheep("web", Some(&out), None)],
        );

        let (_config, report) = run(&fake, SystemTime::now()).await.expect("ticked");

        assert_eq!(report.rotated, 1, "eight days since the last rotation");
    }

    #[tokio::test]
    async fn a_log_rotated_recently_is_not_rotated_again_for_age() {
        let dir = tempfile::tempdir().expect("tempdir");
        let out = dir.path().join("web-0-out.log");
        fs::write(&out, "written just now\n").expect("seeded");
        let yesterday = SystemTime::now() - core::time::Duration::from_secs(86_400);
        fs::write(
            dir.path().join(format!(
                "web-0-out.{}.log",
                crate::naming::stamp_utc(yesterday)
            )),
            "the last rotation\n",
        )
        .expect("seeded");
        let fake = Fake::new(
            "max_size = \"1G\"\nmax_age = \"168h\"\n",
            vec![sheep("web", Some(&out), None)],
        );

        let (_config, report) = run(&fake, SystemTime::now()).await.expect("ticked");

        assert_eq!(report.rotated, 0, "one day since the last rotation");
        assert!(out.exists());
    }

    #[tokio::test]
    async fn a_log_never_rotated_ages_from_when_it_appeared() {
        // No generation to read a rotation from, and the file was created
        // moments ago, so it has not aged whatever its mtime says.
        let dir = tempfile::tempdir().expect("tempdir");
        let out = dir.path().join("web-0-out.log");
        fs::write(&out, "fresh\n").expect("seeded");
        let fake = Fake::new(
            "max_size = \"1G\"\nmax_age = \"1s\"\n",
            vec![sheep("web", Some(&out), None)],
        );

        let (_config, report) = run(&fake, SystemTime::now()).await.expect("ticked");

        assert_eq!(report.rotated, 0);
    }
}
