//! What a stop request does to a tick, at each point the tick consults it.
//!
//! A child of `tick::tests` so it shares that module's `Fake` daemon and
//! `sheep` builder, and in its own file so `tick.rs` stays under its file-size
//! baseline.

use super::*;

#[tokio::test]
async fn a_stop_requested_before_the_tidy_skips_it() {
    // The renames and the reopen are quick and must not be interrupted:
    // a sheep left between the two writes into a file with the wrong
    // name. The gzip afterwards is the slow part, and a stop already
    // requested when the tidy loop starts means none of it runs. The
    // stop lands during the reopen, the latest point a sheep's own
    // rename and reopen can see it.
    let dir = tempfile::tempdir().expect("tempdir");
    let out = dir.path().join("web-0-out.log");
    fs::write(&out, "x".repeat(2048)).expect("seeded");
    let older = dir.path().join("web-0-out.2026-08-20T15-04-01.log");
    fs::write(&older, "would be gzipped\n").expect("seeded");
    let (mut stop, request) = Stop::new();
    let request = RefCell::new(Some(request));
    let fake = Fake {
        on_reopen: Some(Box::new(move |_name| {
            if let Some(request) = request.borrow_mut().take() {
                request.request();
            }
        })),
        ..Fake::new("max_size = \"1K\"\n", vec![sheep("web", Some(&out), None)])
    };

    let (_config, report) = tick(&fake, "log-rotate", SystemTime::now(), &mut stop)
        .await
        .expect("ticked");

    assert_eq!(report.rotated, 1, "the rename still happened");
    assert_eq!(
        *fake.reopened.borrow(),
        vec!["web".to_owned()],
        "and the reopen"
    );
    assert_eq!(report.compressed, 0, "but nothing was compressed");
    assert!(older.exists(), "the older generation is still plain");
    assert!(
        !dir.path()
            .join("web-0-out.2026-08-20T15-04-01.log.gz")
            .exists()
    );
}

#[tokio::test]
async fn a_stop_requested_between_two_sheep_leaves_the_second_unrotated() {
    // The stop lands while the first sheep is being reopened, so that
    // sheep's rename and reopen are done and nothing is waiting for a
    // reopen. The second sheep qualifies too, but starting its rename
    // now would open a window the shepherd's SIGKILL could cut, with no
    // way to know how long the stop has left. It waits for the next
    // dog, and its log is still where shep is writing it.
    let dir = tempfile::tempdir().expect("tempdir");
    let first = dir.path().join("api-0-out.log");
    let second = dir.path().join("web-0-out.log");
    fs::write(&first, "x".repeat(2048)).expect("seeded");
    fs::write(&second, "x".repeat(2048)).expect("seeded");
    let (mut stop, request) = Stop::new();
    let request = RefCell::new(Some(request));
    let fake = Fake {
        on_reopen: Some(Box::new(move |_name| {
            if let Some(request) = request.borrow_mut().take() {
                request.request();
            }
        })),
        ..Fake::new(
            "max_size = \"1K\"\n",
            vec![
                sheep("api", Some(&first), None),
                sheep("web", Some(&second), None),
            ],
        )
    };

    let (_config, report) = tick(&fake, "log-rotate", SystemTime::now(), &mut stop)
        .await
        .expect("ticked");

    assert_eq!(report.rotated, 1, "only the first sheep was rotated");
    assert!(!first.exists(), "api was renamed and reopened in full");
    assert_eq!(*fake.reopened.borrow(), vec!["api".to_owned()]);
    assert!(second.exists(), "web was never renamed, so never orphaned");
}

#[test]
fn a_stop_requested_during_a_gzip_abandons_it() {
    // 16 MiB of noise takes a release build the better part of a
    // hundred milliseconds to deflate and a debug build far longer. The
    // stop arrives after twenty, and the tick must come back on the
    // stop rather than on the gzip. The runtime is built by hand so the
    // directory outlives the abandoned thread: the runtime's drop waits
    // for it, and the tempdir goes after that.
    let dir = tempfile::tempdir().expect("tempdir");
    let out = dir.path().join("web-0-out.log");
    fs::write(&out, "x".repeat(2048)).expect("seeded");
    let mut noise = vec![0u8; 16 << 20];
    let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
    for byte in &mut noise {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        *byte = state as u8;
    }
    let older = dir.path().join("web-0-out.2026-08-20T15-04-01.log");
    fs::write(&older, &noise).expect("seeded");
    drop(noise);
    let fake = Fake::new("max_size = \"1K\"\n", vec![sheep("web", Some(&out), None)]);

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime");
    let (report, took) = runtime.block_on(async {
        let (mut stop, request) = Stop::new();
        tokio::spawn(async move {
            tokio::time::sleep(core::time::Duration::from_millis(20)).await;
            request.request();
        });
        let started = std::time::Instant::now();
        let (_config, report) = tick(&fake, "log-rotate", SystemTime::now(), &mut stop)
            .await
            .expect("ticked");
        (report, started.elapsed())
    });

    assert_eq!(report.rotated, 1);
    assert_eq!(report.compressed, 0, "the gzip was abandoned, not counted");
    assert!(
        dir.path()
            .join("web-0-out.2026-08-20T15-04-01.log.gz")
            .exists(),
        "the gzip had started: its target was created before the stop arrived"
    );
    assert!(
        took < core::time::Duration::from_millis(1500),
        "the tick waited for the gzip rather than the stop: {took:?}"
    );
    // Dropping the runtime waits for the abandoned thread, which in
    // this process nothing cuts off: it finishes the gzip it was
    // running and removes the plain file, as it would have unabandoned.
    // The real binary shuts the runtime down in the background instead
    // and exits, and that is where the plain file survives.
    drop(runtime);
    assert!(!older.exists(), "left to run, the thread finished its job");
}
