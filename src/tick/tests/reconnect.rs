//! What a tick does when the shepherd underneath it is replaced.
//!
//! `shep upgrade` hands over: the daemon `execve`s into its successor, which
//! inherits the listening socket and none of the accepted connections. The
//! dog is still running, holding a connection that no longer answers, and
//! `shep dogs` still lists it online. Two things have to survive that, and
//! only one of them is automatic:
//!
//! - the connection, which the client's own supervisor re-establishes on its
//!   backoff, and
//! - the handshake, which has to name this dog again on the new connection or
//!   the successor counts nothing as a handshake and renders the dog `silent`,
//!   restarts it once, and stops restarting it.
//!
//! A child of `tick::tests` so it shares that module's `sheep` builder, and in
//! its own file so `tick.rs` stays under its file-size baseline.

use super::*;
use shep_client::testing::{
    Handovers, Handshake, control_address, fake_daemon_across_handovers, sample_ack,
};

/// A [`Live`] connected to `socket` as `name`, which is the one place this
/// file touches how a session is opened. Everything below it is asked through
/// [`Daemon`], so it reads the same whichever client is underneath.
async fn live_over(socket: &Path, name: &str) -> Live {
    Live::new(
        ReconnectingClient::connect_as_dog(socket, name)
            .await
            .expect("connected to the fake shepherd"),
    )
}

/// Wait until `handovers` has accepted `wanted` connections, which is the
/// fake's own account of the client having dialled again.
///
/// Asked of the fake rather than of the link, because the cut is not
/// instantaneous: the serving task breaks its loop when it next reads the
/// channel, and until the client sees that EOF its link still reads
/// `Connected`. A test that polled only the link could return before the
/// old connection was known to be dead, and then tick into the gap.
async fn accepted(handovers: &Handovers, wanted: u32) {
    let deadline = std::time::Instant::now() + core::time::Duration::from_secs(10);
    while handovers.accepted() < wanted {
        assert!(
            std::time::Instant::now() < deadline,
            "the fake shepherd accepted {} connection(s), not {wanted}",
            handovers.accepted()
        );
        tokio::time::sleep(core::time::Duration::from_millis(25)).await;
    }
}

/// Wait out the client's own reconnect, and fail rather than tick on a link
/// that is still down: a request issued mid-backoff answers
/// `RequestError::Closed`, and a test that ticked into that gap would be
/// asserting the fake's timing rather than the dog's.
async fn reconnected(live: &Live) {
    let deadline = std::time::Instant::now() + core::time::Duration::from_secs(10);
    while !matches!(live.link(), LinkState::Connected) {
        assert!(
            std::time::Instant::now() < deadline,
            "the client's supervisor never re-established the connection: {:?}",
            live.link()
        );
        tokio::time::sleep(core::time::Duration::from_millis(25)).await;
    }
}

#[tokio::test]
async fn a_shepherd_replaced_underneath_the_dog_is_rotated_for_again() {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket = control_address(dir.path());
    let logs = dir.path();
    let handovers = fake_daemon_across_handovers(
        &socket,
        vec![
            Handshake::Accept(sample_ack()),
            Handshake::Accept(sample_ack()),
        ],
    );
    handovers.reply_to_dog_config("max_size = \"1K\"\n");

    let out = logs.join("web-0-out.log");
    fs::write(&out, "x".repeat(2048)).expect("seeded");
    handovers.reply_to_list(vec![sheep("web", Some(&out), None)]);

    let live = live_over(&socket, "weathervane").await;
    let mut stop = Stop::never();

    let (_config, first) = tick(&live, "weathervane", SystemTime::now(), &mut stop)
        .await
        .expect("the first tick ran");
    assert_eq!(first.rotated, 1, "the seeded log rotated");

    // The old shepherd is gone and the successor is answering on the same
    // socket. The dog's connection died with it.
    handovers.cut().await;
    accepted(&handovers, 2).await;
    reconnected(&live).await;

    fs::write(&out, "y".repeat(2048)).expect("re-seeded");
    let (_config, second) = tick(&live, "weathervane", SystemTime::now(), &mut stop)
        .await
        .expect("a tick after the handover ran");

    assert_eq!(
        second.rotated, 1,
        "the dog rotated for the successor, not just for the shepherd it started against"
    );
    assert!(
        !out.exists(),
        "the re-seeded live log is renamed: {}",
        out.display()
    );

    // Which shepherd it asked. The reopen is the request that only a live
    // session makes, and the generation that received it is the fake's own
    // count of the connections it accepted.
    let reopened_on: Vec<u32> = handovers
        .envelopes()
        .iter()
        .filter(|(_, envelope)| matches!(envelope.body, Request::Reopen { .. }))
        .map(|(generation, _)| *generation)
        .collect();
    assert!(
        reopened_on.iter().any(|generation| *generation >= 2),
        "no request reached the successor, so the dog is still talking to a \
         shepherd that no longer exists: {reopened_on:?}"
    );

    // And what it said when it got there. A connection that names no dog is
    // recorded as nothing: the successor renders this dog `silent` and stops
    // restarting it, which is the failure a passing rotation above cannot see.
    let hellos = handovers.hellos();
    assert!(
        hellos.len() >= 2,
        "the successor never accepted a second handshake: {} hello(s)",
        hellos.len()
    );
    for hello in &hellos {
        assert_eq!(
            hello.dog_name.as_deref(),
            Some("weathervane"),
            "a reconnect that does not re-announce the dog name is a dog the \
             shepherd calls silent"
        );
    }
}
