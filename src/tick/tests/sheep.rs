//! A sheep's own `[app.dogs.log-rotate]` table, over `[log-rotate]`.
//!
//! A child of `tick::tests` for the same reason `stop` is: it shares `Fake`
//! and `sheep`, and `tick.rs` is past its file-size baseline already.

use super::*;

/// A section that rotates nothing under 4K, keeps three generations, and
/// names them the way a test can predict.
const SECTION: &str = "max_size = \"4K\"\nnaming = \"numeric\"\nkeep = 3\ncompress = false\n";

/// Writes `len` bytes at `dir/name`, with an older generation `.1` beside it.
fn seed(dir: &Path, name: &str, len: usize) -> std::path::PathBuf {
    let live = dir.join(name);
    fs::write(&live, "x".repeat(len)).expect("seeded");
    fs::write(dir.join(format!("{name}.1")), "old\n").expect("seeded");
    live
}

#[tokio::test]
async fn a_sheeps_own_table_decides_when_it_rotates_and_how_much_it_keeps() {
    let dir = tempfile::tempdir().expect("tempdir");
    let tight = seed(dir.path(), "tight.log", 2048);
    let plain = seed(dir.path(), "plain.log", 2048);
    let loud = seed(dir.path(), "loud.log", 5000);
    let fake = Fake::new(
        SECTION,
        vec![
            sheep("tight", Some(&tight), None),
            sheep("plain", Some(&plain), None),
            sheep("loud", Some(&loud), None),
        ],
    )
    .with_table("tight", "max_size = \"1K\"\nkeep = 1\n");

    let (_config, report) = run(&fake, SystemTime::now()).await.expect("ticked");

    assert_eq!(report.rotated, 2, "tight on its own 1K and loud on the 4K");
    assert!(!tight.exists(), "2K is over tight's own max_size");
    assert!(dir.path().join("tight.log.1").exists());
    assert!(
        !dir.path().join("tight.log.2").exists(),
        "tight keeps one, so the old generation went"
    );
    assert!(plain.exists(), "2K is under the section's 4K");
    assert!(!loud.exists(), "5000 bytes is over the section's 4K");
    assert!(
        dir.path().join("loud.log.2").exists(),
        "loud has no table, so it keeps the section's three"
    );
    assert_eq!(
        *fake.reopened.borrow(),
        vec!["tight".to_owned(), "loud".to_owned()]
    );
}

#[tokio::test]
async fn a_table_that_sets_only_keep_takes_every_other_key_from_the_section() {
    let dir = tempfile::tempdir().expect("tempdir");
    let web = seed(dir.path(), "web.log", 2048);
    let fake = Fake::new(
        "max_size = \"1K\"\nnaming = \"numeric\"\nkeep = 3\ncompress = false\n",
        vec![sheep("web", Some(&web), None)],
    )
    .with_table("web", "keep = 1\n");

    let (_config, report) = run(&fake, SystemTime::now()).await.expect("ticked");

    assert_eq!(report.rotated, 1, "max_size is the section's 1K");
    assert!(
        dir.path().join("web.log.1").exists(),
        "naming is the section's numeric"
    );
    assert!(
        !dir.path().join("web.log.2").exists(),
        "keep is the table's one"
    );
    assert_eq!(report.compressed, 0, "compress is the section's false");
}

#[tokio::test]
async fn a_table_that_does_not_parse_leaves_that_sheep_alone_and_the_rest_rotate() {
    let dir = tempfile::tempdir().expect("tempdir");
    let web = seed(dir.path(), "web.log", 5000);
    let api = seed(dir.path(), "api.log", 5000);
    let fake = Fake::new(
        SECTION,
        vec![
            sheep("web", Some(&web), None),
            sheep("api", Some(&api), None),
        ],
    )
    .with_table("web", "max_size = \"10MB\"\n");

    let (_config, report) = run(&fake, SystemTime::now())
        .await
        .expect("one sheep's table is not the tick's failure");

    assert!(web.exists(), "web is left where it was");
    assert!(
        dir.path().join("web.log.1").exists() && !dir.path().join("web.log.2").exists(),
        "and its generations are not touched"
    );
    assert!(!api.exists(), "api still rotates");
    assert_eq!(*fake.reopened.borrow(), vec!["api".to_owned()]);

    let line = report
        .summary()
        .expect("a left-alone sheep is worth a line");
    assert_eq!(line.matches("web").count(), 1, "{line}");
    assert!(line.contains("max_size"), "names the key: {line}");
    assert!(!line.contains("api"), "{line}");
    assert_no_dashes(&line);
}

#[tokio::test]
async fn a_table_that_sets_the_dog_wide_interval_is_a_table_that_does_not_parse() {
    let dir = tempfile::tempdir().expect("tempdir");
    let web = seed(dir.path(), "web.log", 5000);
    let fake = Fake::new(SECTION, vec![sheep("web", Some(&web), None)])
        .with_table("web", "interval = \"1s\"\n");

    let (config, report) = run(&fake, SystemTime::now()).await.expect("ticked");

    assert!(web.exists());
    assert_eq!(
        config.interval.as_duration().as_secs(),
        60,
        "one loop, one interval, the section's"
    );
    let line = report.summary().expect("a line");
    assert!(line.contains("web") && line.contains("interval"), "{line}");
}

#[tokio::test]
async fn a_sheep_sharing_a_log_with_one_left_alone_does_not_rotate_it_either() {
    // The left-alone sheep would never be reopened, and would go on writing
    // into the renamed file for good.
    let dir = tempfile::tempdir().expect("tempdir");
    let shared = seed(dir.path(), "shared.log", 5000);
    let fake = Fake::new(
        SECTION,
        vec![
            sheep("bad", Some(&shared), None),
            sheep("good", Some(&shared), None),
        ],
    )
    .with_table("bad", "keep = 0\n");

    let (_config, report) = run(&fake, SystemTime::now()).await.expect("ticked");

    assert!(shared.exists());
    assert_eq!(report.rotated, 0);
    assert!(fake.reopened.borrow().is_empty());
}

#[tokio::test]
async fn a_table_on_a_sheep_the_flock_does_not_hold_changes_nothing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let web = seed(dir.path(), "web.log", 5000);
    let fake =
        Fake::new(SECTION, vec![sheep("web", Some(&web), None)]).with_table("ghost", "keep = 0\n");

    let (_config, report) = run(&fake, SystemTime::now()).await.expect("ticked");

    assert_eq!(report.rotated, 1);
    assert_eq!(
        report.summary().map(|line| line.contains("ghost")),
        Some(false)
    );
}
