//! The two questions `shep adopt` asks this binary before it records
//! anything, asked the way shep asks them.
//!
//! # Why this spawns the binary instead of calling `probe`
//!
//! What can regress here is not the answer, it is the order. `probe` has to
//! run before the argument parser, and that parser refuses every flag it
//! does not know: this dog answered `--version` with a usage message and a
//! failing exit status right up until the call was added. A test that called
//! `probe` directly would pass on a `main` that never calls it at all, which
//! is the only interesting way for this to break.
//!
//! # Why this tier is not gated
//!
//! `integration.rs` needs a built `shep` at `$SHEP_BIN` and boots a real
//! daemon, so it is behind a feature. Nothing here needs either. The
//! contract is a text format and a JSON document, the parser that reads the
//! text format ships in `shep-core`, and the only process involved is this
//! crate's own binary, which `cargo test` has already built.

use std::process::{Command, Output};

use shep_client::shep_core::{
    dogs::parse_version_answer,
    protocol::{MIN_SUPPORTED, PROTOCOL_VERSION},
};

/// The binary under test, as cargo built it for this run.
const DOG_BIN: &str = env!("CARGO_BIN_EXE_shep-log-rotate");

/// Run the dog with one argument and hand back what it said.
///
/// The environment is left as this process has it, deliberately. A probe
/// must not need `$SHEP_HOME` or `$SHEP_DOG_NAME`, because `shep adopt` asks
/// these questions of a candidate it has not registered yet and so cannot
/// name.
fn probe(flag: &str) -> Output {
    Command::new(DOG_BIN)
        .arg(flag)
        .output()
        .expect("the dog binary ran")
}

/// Whatever the dog printed to stdout.
fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).expect("stdout is UTF-8")
}

/// Assert a probe answered cleanly: status 0 and nothing on stderr.
///
/// Both halves matter and they fail differently. A non-zero status is how
/// `adopt` reads "no answer", and it adopts the dog with its protocol
/// unknown rather than saying anything. Output on stderr is what the old
/// usage message came out on, and shep does not read that stream at all, so
/// a regression there is invisible from the shepherd's side.
fn answered_cleanly(output: &Output, flag: &str) {
    assert!(
        output.status.success(),
        "{flag} exited {:?}: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stderr.is_empty(),
        "{flag} wrote to stderr, which shep never reads: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn the_version_answer_is_one_the_shepherds_own_parser_reads() {
    // Round trip rather than a string comparison, and through shep-core's
    // parser rather than a second one written here. The format is only ever
    // interesting to the code that reads it, and that code is the code
    // below.
    let output = probe("--version");
    answered_cleanly(&output, "--version");

    let answer = stdout(&output);
    let parsed = parse_version_answer(&answer)
        .expect("an answer the shepherd's own parser cannot read is the bug this pins");

    assert_eq!(parsed.version, env!("CARGO_PKG_VERSION"));
    assert_eq!(parsed.protocol, Some(PROTOCOL_VERSION));
}

#[test]
fn the_protocol_this_dog_announces_is_one_a_shepherd_still_accepts() {
    // A dog below the floor is refused at the handshake, and a refused
    // handshake is the failure this crate cannot retry its way out of: the
    // daemon that refused is the only party that can, and `main` exits so
    // the shepherd can restart the binary from disk. The number is what
    // decides that, so it is worth an assertion rather than an assumption.
    let answer = stdout(&probe("--version"));
    let parsed = parse_version_answer(&answer).expect("a readable answer");
    let announced = parsed.protocol.expect("a protocol line");

    // A literal, because the comparison below cannot fail on its own. Both
    // constants come from the one shep-core this crate links, and shep-core's
    // own `the_floor_never_outruns_the_ceiling` already pins
    // `MIN_SUPPORTED <= PROTOCOL_VERSION`, so a bump that moved both would
    // slide past a purely relative check. shep-core pins its own number the
    // same way for the same reason.
    //
    // So this failing is not a defect, it is the prompt: shep has moved the
    // protocol, and somebody has to run the integration tier against a
    // shepherd built from that release before changing the number here.
    assert_eq!(
        announced, 8,
        "this dog announces protocol {announced}, and this crate was last verified against a \
         shepherd speaking 8. Run the integration tier against the new shep before moving this."
    );
    assert!(
        announced >= MIN_SUPPORTED,
        "this dog announces protocol {announced} and the shepherd accepts nothing below \
         {MIN_SUPPORTED}"
    );
}

#[test]
fn the_schema_answer_is_json_naming_this_dogs_own_settings() {
    // shep adopts a dog whose `--schema` answer is not JSON anyway, with one
    // notice, and gives it no settings pane. So the failure this guards is
    // quiet on both sides: the dog runs, the operator finds no form, and
    // nothing says why.
    let output = probe("--schema");
    answered_cleanly(&output, "--schema");

    let schema: serde_json::Value =
        serde_json::from_str(&stdout(&output)).expect("the answer is JSON");
    let properties = schema
        .get("properties")
        .and_then(serde_json::Value::as_object)
        .expect("a form is built out of properties");

    let mut named: Vec<&str> = properties.keys().map(String::as_str).collect();
    named.sort_unstable();
    assert_eq!(
        named,
        [
            "compress", "interval", "keep", "max_age", "max_size", "naming"
        ]
    );
}

#[test]
fn the_dogs_own_flag_still_reaches_the_dogs_own_parser() {
    // The other side of the order. `probe` reads the first argument and
    // returns for anything that is not one of its two, so a flag this binary
    // owns has to survive the call rather than being swallowed by it.
    let output = probe("--print-config");
    answered_cleanly(&output, "--print-config");
    assert!(
        stdout(&output).starts_with("[log-rotate]"),
        "--print-config printed: {}",
        stdout(&output)
    );
}

#[test]
fn a_flag_this_binary_has_never_had_is_still_refused() {
    // The probe must not have turned the argument surface open. A rotator
    // that ignored a `--dry-run` it did not understand would rotate for
    // real.
    let output = probe("--rotate-now");
    assert!(!output.status.success(), "an unknown flag is refused");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("--rotate-now"),
        "the refusal names the flag: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
