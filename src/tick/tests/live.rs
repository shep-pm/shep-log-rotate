//! `Live`, the [`Daemon`] that talks to a real shepherd, tested over a real
//! socket.
//!
//! A child of `tick::tests` so it shares that module's `sheep` builder, and in
//! its own file so `tick.rs` stays under its file-size baseline. Everything
//! here needs a socket: the two things worth pinning about `Live` are what it
//! puts on the wire and what it prints about itself, and neither is visible
//! through a fake daemon.

use super::*;
use shep_client::{
    dogs::{DogIdentity, DogRuntime},
    shep_core::{
        paths::ShepPaths,
        protocol::{RpcError, RpcErrorCode},
    },
    testing::{
        Handshake, control_address, fake_daemon_accepting_repeatedly, fake_daemon_across_handovers,
        sample_ack,
    },
};

/// A [`ShepPaths`] rooted at `dir` with its socket where a fake bound `socket`.
///
/// `DogRuntime::start` takes the layout rather than the socket, and the fake
/// binds wherever the test asked, so the two have to be joined by hand.
/// `pub(super)` so `reconnect` opens a session the same way rather than
/// spelling this again.
pub(super) fn paths_with_socket(dir: &Path, socket: &Path) -> ShepPaths {
    let mut paths = ShepPaths::resolve(&|_| None, dir);
    paths.socket = socket.to_path_buf();
    paths
}

/// A session with the fake shepherd bound at `socket`, opened the way `main`
/// opens one: through [`DogRuntime::start`].
pub(super) async fn session(dir: &Path, socket: &Path, name: &str) -> Live {
    let runtime = DogRuntime::start(DogIdentity::named(name), paths_with_socket(dir, socket))
        .await
        .expect("the fake shepherd answered");
    Live::new(runtime)
}

/// The requests `Live` has put on the wire so far, in order.
fn asked(handovers: &shep_client::testing::Handovers) -> Vec<Request> {
    handovers
        .envelopes()
        .iter()
        .map(|(_, envelope)| envelope.body.clone())
        .collect()
}

/// `Live`'s `Debug` is written rather than derived so a socket path under
/// somebody's home directory cannot reach a log or a bug report. Proven
/// against a real session over a real socket, because the thing being guarded
/// against is the DERIVED impl, and a fake that did not hold a client could
/// not tell the two apart.
#[tokio::test]
async fn debug_says_nothing_about_the_connection() {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket = control_address(dir.path());
    let (_fake, _served) = fake_daemon_accepting_repeatedly(
        &socket,
        Response::DogSection {
            toml: String::new().into(),
        },
    );
    let shown = format!("{:?}", session(dir.path(), &socket, "weathervane").await);

    assert_eq!(shown, "Live(<shepherd session>)");
    assert!(
        !shown.contains("weathervane"),
        "the announced name leaked: {shown}"
    );
    assert!(
        !shown.contains(&socket.display().to_string()),
        "the socket path leaked: {shown}"
    );
}

#[tokio::test]
async fn the_section_is_asked_for_already_resolved() {
    // A `[<name>]` section is an operator's file, and shep lets it name a
    // secret instead of holding one. Only `Request::DogConfigResolved` hands
    // the section over with that reference expanded, and this dog reads its
    // settings off that section, so the ask is the whole difference between a
    // credential and a string that looks like one.
    let dir = tempfile::tempdir().expect("tempdir");
    let socket = control_address(dir.path());
    let handovers = fake_daemon_across_handovers(&socket, vec![Handshake::Accept(sample_ack())]);
    handovers.reply_to_dog_config("max_size = \"1K\"\n");
    handovers.reply_to_list(Vec::new());

    let live = session(dir.path(), &socket, "weathervane").await;
    let (config, _report) = tick(&live, "weathervane", SystemTime::now(), &mut Stop::never())
        .await
        .expect("ticked");
    assert_eq!(config.max_size.bytes(), 1024, "the armed section was used");

    let asked = asked(&handovers);
    let resolved: Vec<&Request> = asked
        .iter()
        .filter(|body| matches!(body, Request::DogConfigResolved { .. }))
        .collect();
    assert!(
        resolved.len() >= 2,
        "the section is re-read on every tick rather than served out of memory, so a \
         `max_size` change takes effect on the next tick: {asked:?}"
    );
    assert!(
        !asked
            .iter()
            .any(|body| matches!(body, Request::DogConfig { .. })),
        "the plain request is the fallback for a shepherd too old to resolve, not the \
         first ask: {asked:?}"
    );
    for body in resolved {
        match body {
            Request::DogConfigResolved { name } => assert_eq!(name, "weathervane"),
            other => unreachable!("{other:?} is not the request under test"),
        }
    }
}

#[tokio::test]
async fn a_shepherd_too_old_to_resolve_still_gets_the_plain_ask() {
    // The other half of the same call. A dog built against a new shep-client
    // still runs against an old shepherd: the client asks the resolved request,
    // is refused with `Unsupported`, and asks again the plain way. Refusing to
    // fall back would leave this dog unable to rotate for a shepherd that is
    // running perfectly well.
    let dir = tempfile::tempdir().expect("tempdir");
    let socket = control_address(dir.path());
    let handovers = fake_daemon_across_handovers(&socket, vec![Handshake::Accept(sample_ack())]);
    handovers.reply_to_dog_config("max_size = \"1K\"\n");
    handovers.refuse_dog_config_resolved(RpcError {
        code: RpcErrorCode::Unsupported,
        message: "this shepherd has no DogConfigResolved".to_owned(),
        daemon_version: None,
    });

    let out = dir.path().join("web-0-out.log");
    fs::write(&out, "x".repeat(2048)).expect("seeded");
    handovers.reply_to_list(vec![sheep("web", Some(&out), None)]);

    let live = session(dir.path(), &socket, "weathervane").await;
    let (_config, report) = tick(&live, "weathervane", SystemTime::now(), &mut Stop::never())
        .await
        .expect("an old shepherd is a supported shepherd, not an error");

    assert_eq!(
        report.rotated, 1,
        "the section arrived through the fallback and the log rotated"
    );
    assert!(
        asked(&handovers)
            .iter()
            .any(|body| matches!(body, Request::DogConfig { .. })),
        "nothing fell back to the plain request: {:?}",
        asked(&handovers)
    );
}
