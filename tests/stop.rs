//! What a supervised stop does to this dog, asked of the real binary.
//!
//! # Why this spawns the binary
//!
//! The shepherd stops a dog with `SIGTERM`, and what matters is what the
//! process does with it, which is decided by `main` and the signal
//! disposition it installs. A unit test of `Stop` could pass on a `poll` that
//! never installs it. So this runs the built binary with no shepherd to talk
//! to, where `poll` prints the connect error and keeps retrying, and sends it
//! the signal the shepherd's kill ladder sends first.
//!
//! The exit status is the whole assertion. With no handler `SIGTERM` takes
//! its default disposition and the process dies by signal, so `code()` is
//! `None`. With one, `poll` finishes whatever tick is in flight, including
//! any rename and its reopen, and returns success.

#![cfg(unix)]

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

/// The binary under test, as cargo built it for this run.
const DOG_BIN: &str = env!("CARGO_BIN_EXE_shep-log-rotate");

/// How long a stopped dog gets to exit. Generous against a loaded machine,
/// and far under what a hung test would cost: the shepherd itself gives a dog
/// 1600 ms by default before it sends `SIGKILL`.
const EXIT_WITHIN: Duration = Duration::from_secs(20);

/// A running dog that is killed if the test ends without reaping it, so a
/// failed assertion does not leave a poll loop behind.
struct Running(Child);

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Start the dog against a `$SHEP_HOME` with no shepherd in it, and return
/// once it has printed its first connect error.
///
/// That line is the signal that `poll` is running, which is after the stop
/// handlers are installed. Sending earlier would race the install and test
/// nothing.
fn start(home: &std::path::Path) -> Running {
    let mut child = Command::new(DOG_BIN)
        .env("SHEP_HOME", home)
        .env("SHEP_DOG_NAME", "log-rotate")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the dog binary ran");
    let stderr = child.stderr.take().expect("stderr was piped");
    let running = Running(child);

    let mut line = String::new();
    BufReader::new(stderr)
        .read_line(&mut line)
        .expect("the dog printed a line");
    assert!(
        line.starts_with("shep-log-rotate:"),
        "expected the connect error, got: {line:?}"
    );
    running
}

/// Send `signal` to the dog by name, since this crate forbids unsafe code and
/// so cannot call `kill(2)` itself.
fn send(dog: &Running, signal: &str) {
    let status = Command::new("kill")
        .args([&format!("-{signal}"), &dog.0.id().to_string()])
        .status()
        .expect("kill ran");
    assert!(status.success(), "kill -{signal} failed: {status}");
}

/// Wait for the dog to exit, or fail the test if it outlasts [`EXIT_WITHIN`].
fn exit_status(dog: &mut Running) -> ExitStatus {
    let deadline = Instant::now() + EXIT_WITHIN;
    loop {
        if let Some(status) = dog.0.try_wait().expect("polled the dog") {
            return status;
        }
        assert!(
            Instant::now() < deadline,
            "the dog was still running {EXIT_WITHIN:?} after the signal"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_supervised_stop_ends_the_dog_with_a_clean_exit() {
    let home = tempfile::tempdir().expect("a temp dir");
    let mut dog = start(home.path());

    send(&dog, "TERM");

    let status = exit_status(&mut dog);
    assert_eq!(
        status.code(),
        Some(0),
        "SIGTERM must reach the stop request, not the default disposition: {status}"
    );
}

#[test]
fn ctrl_c_still_ends_the_dog_with_a_clean_exit() {
    let home = tempfile::tempdir().expect("a temp dir");
    let mut dog = start(home.path());

    send(&dog, "INT");

    let status = exit_status(&mut dog);
    assert_eq!(status.code(), Some(0), "SIGINT ended the dog as: {status}");
}
