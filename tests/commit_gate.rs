//! The commit gate, run the way git and CI run it.
//!
//! # Why a shell script gets a Rust test
//!
//! The gate is the only check in this repo whose failure mode is silence. A
//! compile error says what broke; a subject release-plz cannot parse under
//! `filter_unconventional = true` produces a release with no line for that
//! change, and CI is green while it happens. The gate is also the only check
//! that runs in two places at once, a hook a contributor may not have
//! installed and a workflow that always runs, so the two can drift apart
//! without either going red on its own.
//!
//! So this spawns the real scripts rather than reimplementing the rule in
//! Rust. A test that carried its own copy of the regex would pass on a gate
//! that had lost it, which is the shape of every failure here.
//!
//! # What the two halves are handed
//!
//! The hook gets the message file as git wrote it, before git cleans the
//! message up. The workflow gets `git log --format=%s`, which is the cleaned
//! subject. Both have to land on the same answer, and the first has to read
//! past the blank lines and comment lines an editor leaves above the subject.

use std::fs;
use std::io::Write;
use std::process::{Command, Output, Stdio};

/// The local gate, as git would run it.
const HOOK: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/.githooks/commit-msg");

/// The rule both gates read.
const GATE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/.githooks/subject-gate.sh");

/// The CI gate, read as text: this test checks what it calls rather than
/// replaying a GitHub Actions job.
const WORKFLOW: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/.github/workflows/commits.yml");

/// Run the hook over a message file written exactly as an editor would leave
/// it, and hand back what it said.
fn hook(message: &str) -> Output {
    let mut file = tempfile::NamedTempFile::new().expect("a message file to hand the hook");
    file.write_all(message.as_bytes())
        .expect("the message is written");

    // `sh` rather than executing the file: the mode bit is what git uses, and
    // a test that relied on it would pass for the wrong reason on a filesystem
    // that lost it.
    Command::new("sh")
        .arg(HOOK)
        .arg(file.path())
        .output()
        .expect("the hook ran")
}

/// Run the shared rule over subjects, one per line, as the workflow feeds it.
fn gate(subjects: &str) -> Output {
    // All three piped: a stream the child inherits goes to the test harness
    // instead of into `Output`, and a test that reads an empty `Output` passes
    // on a gate that printed nothing at all.
    let mut child = Command::new("sh")
        .arg(GATE)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the gate ran");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(subjects.as_bytes())
        .expect("the subjects are written");
    child.wait_with_output().expect("the gate finished")
}

/// Whatever the gate printed, on either stream.
fn said(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// The subject a later reader sees for a message, asked of git itself.
///
/// `commit-tree` writes a commit object into this checkout's object database
/// and prints its id: no branch, no ref, no history to undo, and the object is
/// unreachable, so `git gc` prunes it. `git log --format=%s` on that id then
/// reports the subject the way release-plz and every other reader sees it.
/// The identity is passed in rather than read from the clone, because a test
/// must not care who cloned it.
fn subject_a_reader_sees(message: &str) -> String {
    let object = Command::new("git")
        .env("GIT_AUTHOR_NAME", "commit gate test")
        .env("GIT_AUTHOR_EMAIL", "commit-gate@example.invalid")
        .env("GIT_COMMITTER_NAME", "commit gate test")
        .env("GIT_COMMITTER_EMAIL", "commit-gate@example.invalid")
        .arg("commit-tree")
        // git's empty tree, so the commit has no content to write.
        .arg("4b825dc642cb6eb9a060e54bf8d69288fbee4904")
        .arg("-m")
        .arg(message)
        .output()
        .expect("git commit-tree ran");
    assert!(
        object.status.success(),
        "commit-tree failed, which means this test is not running in a checkout: {}",
        String::from_utf8_lossy(&object.stderr)
    );
    let sha = String::from_utf8_lossy(&object.stdout).trim().to_owned();

    let subject = Command::new("git")
        .arg("log")
        .arg("-1")
        .arg("--format=%s")
        .arg(&sha)
        .output()
        .expect("git log ran");
    String::from_utf8_lossy(&subject.stdout)
        .trim_end()
        .to_owned()
}

#[test]
fn git_picks_the_first_non_blank_line_as_the_subject() {
    // The premise the hook rests on, asked of git rather than asserted here.
    // git's cleanup for an edited message is `strip` (leading and trailing
    // blank lines gone, trailing whitespace gone, comment lines gone), and
    // picking a subject skips blank lines besides that. Both halves say the
    // line an editor left at the top of the file is not the line anybody
    // reads, and this is the half that is git's rule rather than this crate's.
    let message = "\n\nfeat: subject below a blank line\n\n# comment line git strips\n   \n\n";
    assert_eq!(
        subject_a_reader_sees(message),
        "feat: subject below a blank line"
    );
}

#[test]
fn a_subject_below_blank_lines_is_the_subject_the_hook_reads() {
    // The trigger: an editor opened with the cursor two lines down, or a
    // commit.template that opens with comment lines. A hook reading line 1 of
    // the file reads a line git throws away, and refuses a commit git records
    // as conventional. `git_picks_the_first_non_blank_line_as_the_subject`
    // above is what says the expectation below is right.
    let message = "\n\nfeat: subject below a blank line\n\n# comment line git strips\n   \n\n";

    let output = hook(message);
    assert!(
        output.status.success(),
        "the hook refused a commit git records as {:?}: {}",
        subject_a_reader_sees(message),
        said(&output)
    );
}

#[test]
fn a_message_with_no_subject_is_refused_and_says_so() {
    // The other half of the same disagreement. git records an empty subject
    // for `git commit --allow-empty-message -m \"\"`, release-plz drops it,
    // and a gate that skipped an empty line reported every subject on that
    // branch conventional. Both gates refuse it.
    let only_comments = "\n# Please enter the commit message for your changes.\n# lines starting with # were stripped\n\n";
    let output = hook(only_comments);
    assert!(
        !output.status.success(),
        "a message of comments only: {only_comments:?}"
    );
    assert!(
        said(&output).contains("<empty>"),
        "the refusal says the subject was empty rather than showing nothing: {}",
        said(&output)
    );

    let empty = hook("");
    assert!(!empty.status.success(), "an empty message file");

    let in_ci = gate("feat: fine\n\nfix: also fine\n");
    assert!(
        !in_ci.status.success(),
        "the empty line in the middle is a subject, not a gap in the input: {}",
        said(&in_ci)
    );
    assert!(
        said(&in_ci).contains("<empty subject>"),
        "the CI line names it: {}",
        said(&in_ci)
    );
}

#[test]
fn the_gate_accepts_what_the_changelog_parses() {
    for subject in [
        "feat: a new surface",
        "fix(tick): stop the flush",
        "chore(deps)!: raise shep-client",
        "refactor: no behaviour change",
        "perf(rotate): read the directory once",
        "docs: name the floor",
        "test(tick): pin a handover",
        "ci: run the gate",
        "style: rewrap a comment",
        "feat(dogs.toml)!: rename interval to poll_interval",
    ] {
        let output = hook(&format!("{subject}\n"));
        assert!(
            output.status.success(),
            "{subject} refused: {}",
            said(&output)
        );
    }
}

#[test]
fn the_gate_refuses_prose_and_types_the_changelog_has_no_parser_for() {
    // `revert` and `build` are conventional types in the wider spec and are
    // refused here on purpose: release-plz-changelog.toml has no parser for
    // either, so a commit carrying one lands with no changelog entry, which is
    // the exact failure the gate exists to catch.
    for subject in [
        "Updated docs",
        "feat: ",
        "feat:no space after the colon",
        "build: bump a dependency",
        "Revert \"feat: x\"",
        "FIX: shouting",
        "   feat: indented below blanks",
    ] {
        let output = hook(&format!("{subject}\n"));
        assert!(!output.status.success(), "{subject} accepted");
    }
}

#[test]
fn the_local_gate_lets_git_own_subjects_through() {
    // A merge commit's subject is git's, and `git rebase --autosquash` eats a
    // marker before the commit ever reaches a changelog. Refusing either locally
    // would push a contributor to `--no-verify`, which skips the check they
    // wanted along with the one they hit.
    for subject in [
        "Merge branch 'main' into side",
        "Merge remote-tracking branch 'origin/main' into side",
        "Merge pull request #9 in shep-pm/shep-log-rotate",
        "squash! feat: thing",
        "fixup! fix(tick): stop the flush",
        "amend! refactor: reword",
    ] {
        let output = hook(&format!("{subject}\n"));
        assert!(
            output.status.success(),
            "{subject} refused: {}",
            said(&output)
        );
    }
}

#[test]
fn the_gate_says_so_when_there_is_nothing_to_check() {
    // A pull request whose only commits are merges has no subject to judge.
    // Reporting "All 0 commit subject(s) are conventional" would read like a
    // verdict on a branch the job never looked at.
    let output = gate("");
    assert!(
        output.status.success(),
        "an empty range failed: {}",
        said(&output)
    );
    assert!(
        said(&output).contains("No commit subject to check"),
        "{}",
        said(&output)
    );
}

#[test]
fn both_gates_read_the_rule_out_of_one_file() {
    // The drift this pins. Two copies of the same regex in two files is how
    // the two gates started disagreeing, and nothing about a drift makes
    // either of them fail on its own.
    let workflow = fs::read_to_string(WORKFLOW).expect("the workflow is readable");
    let hook_script = fs::read_to_string(HOOK).expect("the hook is readable");

    assert!(
        workflow.contains(".githooks/subject-gate.sh"),
        "the workflow runs the shared rule rather than its own copy"
    );
    assert!(
        hook_script.contains("subject-gate.sh"),
        "the hook runs the shared rule rather than its own copy"
    );
    for (name, text) in [("the workflow", &workflow), ("the hook", &hook_script)] {
        assert!(
            !text.contains("feat|fix|perf"),
            "{name} carries a second copy of the rule"
        );
    }
}

#[test]
fn nothing_the_gate_prints_for_a_person_carries_a_dash() {
    // `test_support::assert_no_dashes` is the crate's rule for user-facing
    // text, and it lives in the binary, which an integration test cannot
    // reach. The gate's refusal is the most-read piece of text in this repo.
    let refused = hook("Updated docs\n");
    let text = format!("{}{}", said(&refused), said(&gate("Updated docs\n")));
    for c in ['\u{2014}', '\u{2013}'] {
        assert!(
            !text.contains(c),
            "the gate prints {:?} to a contributor",
            c
        );
    }
}
