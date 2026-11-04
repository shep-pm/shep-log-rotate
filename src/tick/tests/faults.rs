//! What a tick does when the disk, not the daemon, is what fails.
//!
//! A child of `tick::tests` for the same reason `stop` and `sheep` are: it
//! shares `Fake` and `sheep`, and `tick.rs` is past its file-size baseline.
//! A rename, a compression, a deletion or a directory listing that fails is
//! that one log's problem, reported on the summary line, and the next log is
//! still handled. Every test here is one edge of that rule.

use super::*;

#[cfg(unix)]
#[tokio::test]
async fn a_log_directory_that_cannot_be_listed_is_skipped_and_said() {
    // api's directory lost its read bit, so its age cannot be read off
    // its generations. That is api's problem, reported as such, and not
    // a reason to leave web unrotated.
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().expect("tempdir");
    let apis = dir.path().join("api");
    fs::create_dir(&apis).expect("created");
    let api = apis.join("api-0-out.log");
    let web = dir.path().join("web-0-out.log");
    fs::write(&api, "small\n").expect("seeded");
    fs::write(&web, "x".repeat(2048)).expect("seeded");
    fs::set_permissions(&apis, fs::Permissions::from_mode(0o311)).expect("chmod");
    if fs::read_dir(&apis).is_ok() {
        // Root, or anything with CAP_DAC_OVERRIDE, is not refused by a
        // mode bit, so the precondition this test is about is absent.
        fs::set_permissions(&apis, fs::Permissions::from_mode(0o755)).expect("chmod back");
        eprintln!("skipped: this process can list a directory without its read bit");
        return;
    }
    let fake = Fake::new(
        "max_size = \"1K\"\nmax_age = \"168h\"\n",
        vec![
            sheep("api", Some(&api), None),
            sheep("web", Some(&web), None),
        ],
    );

    let outcome = run(&fake, SystemTime::now()).await;
    fs::set_permissions(&apis, fs::Permissions::from_mode(0o755)).expect("chmod back");
    let (_config, report) = outcome.expect("one unlistable directory does not fail the tick");

    assert_eq!(report.rotated, 1, "web rotated");
    assert_eq!(report.unlistable.len(), 1, "{:?}", report.unlistable);
    assert!(
        report.unlistable[0].contains("api"),
        "{:?}",
        report.unlistable
    );
    let line = report.summary().expect("a line");
    assert!(line.contains("to list a log directory"), "{line}");
}

#[tokio::test]
async fn a_rename_fault_on_one_sheep_does_not_stop_the_next() {
    // api's directory has run out of generation numbers. That is api's
    // problem, reported as such; web, in its own directory and over
    // max_size, still rotates on the same tick.
    let dir = tempfile::tempdir().expect("tempdir");
    let apis = dir.path().join("api");
    fs::create_dir(&apis).expect("created");
    let api = apis.join("api-0-out.log");
    let web = dir.path().join("web-0-out.log");
    fs::write(&api, "x".repeat(2048)).expect("seeded");
    fs::write(&web, "x".repeat(2048)).expect("seeded");
    fs::write(apis.join("api-0-out.log.4294967295"), "oldest").expect("seeded");
    let fake = Fake::new(
        "max_size = \"1K\"\nnaming = \"numeric\"\n",
        vec![
            sheep("api", Some(&api), None),
            sheep("web", Some(&web), None),
        ],
    );

    let (_config, report) = run(&fake, SystemTime::now()).await.expect("ticked");

    assert_eq!(report.rename_failed.len(), 1, "{:?}", report.rename_failed);
    assert_eq!(report.rotated, 1, "web rotated regardless");
    assert_eq!(*fake.reopened.borrow(), vec!["web".to_owned()]);
    assert!(api.exists(), "api's live log is where it was");
}

#[tokio::test]
async fn a_tidy_fault_is_reported_and_the_tick_goes_on() {
    // The .gz that compressing the older generation would create is a
    // directory, so File::create fails. That is that generation's
    // problem, on the summary line; the rotation and the reopen stand,
    // and the report comes back whole.
    let dir = tempfile::tempdir().expect("tempdir");
    let out = dir.path().join("web-0-out.log");
    fs::write(&out, "x".repeat(2048)).expect("seeded");
    let older = dir.path().join("web-0-out.2026-08-20T15-04-01.log");
    fs::write(&older, "older\n").expect("seeded");
    fs::create_dir(dir.path().join("web-0-out.2026-08-20T15-04-01.log.gz")).expect("blocker");
    let fake = Fake::new("max_size = \"1K\"\n", vec![sheep("web", Some(&out), None)]);

    let (_config, report) = run(&fake, SystemTime::now())
        .await
        .expect("a tidy fault is reported, not returned");

    assert_eq!(report.rotated, 1);
    assert_eq!(*fake.reopened.borrow(), vec!["web".to_owned()]);
    assert_eq!(report.tidy_failed.len(), 1, "{:?}", report.tidy_failed);
    assert!(
        report.tidy_failed[0].contains("15-04-01.log.gz"),
        "{:?}",
        report.tidy_failed
    );
    assert!(
        older.exists(),
        "the generation that could not be compressed is still plain"
    );
    let line = report.summary().expect("a line");
    assert!(line.contains("to compress or delete"), "{line}");
}

#[cfg(unix)]
#[tokio::test]
async fn an_unlistable_directory_is_listed_once_and_reported_once() {
    // merge_logs points out_file and err_file at one path, and a second
    // sheep logs in the same directory. One directory, one attempt, one
    // line on the summary.
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().expect("tempdir");
    let apis = dir.path().join("api");
    fs::create_dir(&apis).expect("created");
    let api = apis.join("api-0-out.log");
    let other = apis.join("api-1-out.log");
    fs::write(&api, "small\n").expect("seeded");
    fs::write(&other, "small\n").expect("seeded");
    fs::set_permissions(&apis, fs::Permissions::from_mode(0o311)).expect("chmod");
    if fs::read_dir(&apis).is_ok() {
        fs::set_permissions(&apis, fs::Permissions::from_mode(0o755)).expect("chmod back");
        eprintln!("skipped: this process can list a directory without its read bit");
        return;
    }
    let fake = Fake::new(
        "max_size = \"1K\"\nmax_age = \"168h\"\n",
        vec![
            sheep("api", Some(&api), Some(&api)),
            sheep("api-1", Some(&other), None),
        ],
    );

    let outcome = run(&fake, SystemTime::now()).await;
    fs::set_permissions(&apis, fs::Permissions::from_mode(0o755)).expect("chmod back");
    let (_config, report) = outcome.expect("ticked");

    assert_eq!(report.unlistable.len(), 1, "{:?}", report.unlistable);
    assert_eq!(report.rotated, 0);
}

#[tokio::test]
async fn a_file_one_sheep_failed_to_rename_is_held_back_when_another_renames_it() {
    // alpha and gamma share `shared.log`. alpha's own log rotates, its
    // rename of the shared log fails on a directory sitting where `.1`
    // goes, and alpha is reopened for its own log. The reopen hook then
    // clears the blocker, so gamma's rename of the shared log succeeds
    // and gamma is reopened. alpha, reopened before the file moved,
    // writes into the renamed generation through its old descriptor,
    // and that file is not safe to tidy however cleanly gamma reopened.
    let dir = tempfile::tempdir().expect("tempdir");
    let alphas_own = dir.path().join("alpha-0-err.log");
    let shared = dir.path().join("shared.log");
    fs::write(&alphas_own, "x".repeat(2048)).expect("seeded");
    fs::write(&shared, "x".repeat(2048)).expect("seeded");
    fs::write(dir.path().join("shared.log.2"), "old\n").expect("seeded");
    let blocker = dir.path().join("shared.log.1");
    fs::create_dir(&blocker).expect("blocker");
    let cleared = blocker.clone();
    let fake = Fake {
        on_reopen: Some(Box::new(move |_| {
            let _ = fs::remove_dir(&cleared);
        })),
        ..Fake::new(
            "max_size = \"1K\"\nnaming = \"numeric\"\nkeep = 1\ncompress = false\n",
            vec![
                sheep("alpha", Some(&alphas_own), Some(&shared)),
                sheep("gamma", Some(&shared), None),
            ],
        )
    };

    let (_config, report) = run(&fake, SystemTime::now()).await.expect("ticked");

    assert_eq!(report.rename_failed.len(), 1, "{:?}", report.rename_failed);
    assert_eq!(
        *fake.reopened.borrow(),
        vec!["alpha".to_owned(), "gamma".to_owned()]
    );
    assert_eq!(
        report.rotated, 2,
        "alpha's own log, then gamma's rename of the shared one"
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("shared.log.1")).expect("gamma's rename"),
        "x".repeat(2048),
        "gamma renamed the shared log once the blocker was gone"
    );
    assert_eq!(report.deleted, 0, "the shared log is held back from tidy");
    assert!(
        dir.path().join("shared.log.4").exists(),
        "the old generation, shifted twice, is past keep = 1 and still there"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn an_unlistable_directory_under_two_spellings_is_reported_once() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().expect("tempdir");
    let apis = dir.path().join("api");
    fs::create_dir(&apis).expect("created");
    fs::create_dir(apis.join("sub")).expect("created");
    let api = apis.join("api-0-out.log");
    let other = apis.join("api-1-out.log");
    fs::write(&api, "small\n").expect("seeded");
    fs::write(&other, "small\n").expect("seeded");
    let long_way = apis.join("sub").join("..").join("api-1-out.log");
    fs::set_permissions(&apis, fs::Permissions::from_mode(0o311)).expect("chmod");
    if fs::read_dir(&apis).is_ok() {
        fs::set_permissions(&apis, fs::Permissions::from_mode(0o755)).expect("chmod back");
        eprintln!("skipped: this process can list a directory without its read bit");
        return;
    }
    let fake = Fake::new(
        "max_size = \"1K\"\nmax_age = \"168h\"\n",
        vec![
            sheep("api", Some(&api), None),
            sheep("api-1", Some(&long_way), None),
        ],
    );

    let outcome = run(&fake, SystemTime::now()).await;
    fs::set_permissions(&apis, fs::Permissions::from_mode(0o755)).expect("chmod back");
    let (_config, report) = outcome.expect("ticked");

    assert_eq!(report.unlistable.len(), 1, "{:?}", report.unlistable);
}

#[test]
fn the_summary_counts_a_single_fault_as_once() {
    let report = Report {
        rotated: 1,
        rename_failed: vec!["web-0-out.log: no generation numbers left to rotate into".into()],
        skipped_protected: 2,
        ..Report::default()
    };
    let line = report.summary().expect("a line");
    assert!(line.contains("failed once to rename"), "{line}");
    assert!(line.contains("refused twice to touch"), "{line}");
    assert!(!line.contains("1 times"), "{line}");
}
