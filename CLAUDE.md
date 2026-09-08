# shep-log-rotate

A log-rotation dog for shep. One binary, one poll loop, no library target.

## Commands

- `cargo test --locked` is the test shape. There is no lib target, so `cargo test --lib` errors out with "no library targets found".
- The four lint gates CI runs, all required: `cargo fmt --all -- --check`, `cargo clippy --all-targets --all-features -- -D warnings`, `RUSTDOCFLAGS='-D warnings' cargo doc --no-deps --all-features`, and `cargo +1.88 check --all-targets --all-features --locked`.
- `SHEP_BIN=/path/to/shep cargo test --features integration --locked` drives a real shepherd in a temporary `$SHEP_HOME`. Off by default because a fresh clone has no shep binary. A plain `cargo test` runs a test named `heads_up_the_real_shepherd_tier_is_not_running` whose only job is to say so.
- `cargo llvm-cov --locked --summary-only` for coverage. The unit tier sits above 92% of lines; `main.rs` is the low file because the poll loop needs a daemon.
- The integration tier boots five shepherds in parallel and races shell loops, so it is load sensitive. A test that fails only in the full run and passes alone is a timing race in the test, not in the dog. Measure it: `--test integration <name>` ten times alone, then the full tier ten times, before blaming a change.

## Rules the tests already enforce

- Never build `Request::Flush`. It truncates the recorded paths. `this_dog_never_sends_flush` in `src/main.rs` scans every source file for it.
- No em dash or en dash in anything printed for a person. `test_support::assert_no_dashes` is the check; use it on any new user-facing string.
- `Debug` on a type holding a socket path or a secret is written by hand and pinned by an exact-string test. `Live` in `src/tick.rs` is the example.
- Every fallible `pub fn` has a `# Errors` section. A `# Panics` section needs `#[track_caller]`, except on an async fn, where the attribute is a no-op and clippy rejects it; say in the section where the location comes from instead. Errors implement `core::error::Error`, never `std::error::Error`.
- `#![forbid(unsafe_code)]` at the crate root.

## Where things live

- `src/tick.rs` is the only module that talks to the daemon. `src/prune.rs` is the only one that deletes a file. `src/naming.rs` is the only place a generation name is built or recognised, and a false match there deletes an operator's data.
- Directory comparisons go through `file_set::ResolvedDir`, never `PathBuf` equality: `..` and a symlinked directory read as different files otherwise. Both guards and the renamed set share one `FileSet`.
- `stop::Stop` carries ctrl-c. `tick` consults it only from its tidy loop, where each gzip runs on a blocking thread; the renames and reopens always run to completion. A tick interrupted between a rename and its reopen would leave shep writing into a file with the wrong name, so do not add a stop check there.
- A fault on disk never fails a tick. A rename, a compression, a deletion or a directory listing that fails is that log's problem, reported on the summary line, and the next log is still handled. Only the daemon stops a tick: a config it cannot read, a request it cannot make, a reopen it refuses.
- `max_age` counts from the last rotation, read off the newest generation (`rotate::last_rotation`), then from the file's birth time, then from its last write. Never the last write first: an mtime can predate the rotation it followed.
- `shep-client` comes by version from crates.io, and is the only path to shep-core: `shep_client::shep_core`, never a second direct dependency. The floor is 0.7.3, for two reasons at once. A dog announces the `PROTOCOL_VERSION` of the shep-core it was built against and the shepherd refuses a handshake below its own `MIN_SUPPORTED`, both 8 today; protocol 8 first shipped in 0.6.2, but a `0.x` requirement cannot span a minor. And 0.7.3 is where shep-client's `schema` feature started forwarding to shep-core's, which is what puts the `JsonSchema` impls on `MemSize` and `UpDuration`. Below it `src/config.rs` does not compile. The lockfile pins the newest published and is refreshed deliberately.
- `main` calls `shep_client::dogs::probe::<config::Section>` on its first line, before the argument parser. That answers the two questions `shep adopt` spawns this binary to ask: `--version` for the build and the protocol number, `--schema` for a JSON Schema of the `[log-rotate]` section. `Action::parse` refuses every flag it does not know, so a probe flag that reached it would exit 1 with a usage message, which shep reads as no answer at all. `tests/probe.rs` spawns the binary to pin the order; a test that called `probe` would pass on a `main` that never calls it.
- Config lives in `[<name>]` of `dogs.toml`, not `[dog.<name>]` of `shep.toml`. The daemon serves the table body without its header either way, so the parser never saw the move, but `PRINT_CONFIG`, the README and every error message name the new place. Writing the old header back after a shepherd has migrated it names one dog in two files, which the daemon refuses to boot on.
- The schema is the third thing that has to agree about what a setting is, alongside `Section`'s fields and `PRINT_CONFIG`. `the_schema_and_the_printed_block_name_the_same_settings` is the edge between them. `max_size`, `max_age` and `interval` reach the schema through `#[schemars(with = ...)]`, so the pattern stays shep-core's and lookout can read the `$ref` name to tell bytes from milliseconds.

## Style

- Doc comments here are long on purpose and explain the decision, not the syntax. Match that for new items rather than trimming to a one-liner.
- `.coderabbit.yaml` restates the Rust rules reviewers hold this crate to. Read its `path_instructions` before touching `src/prune.rs` or `src/rotate.rs`.
- Terminology: a `sheep` is one managed process, the plural is `flock`, dogs are plugin processes, the daemon is only ever "the shepherd".
