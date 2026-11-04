//! One poll of the rotator's loop: read the config and each sheep's own
//! table, list the flock, rename what has grown, ask the shepherd to reopen,
//! then compress and prune.
//!
//! This is the only module that speaks to the daemon, and it is where the
//! two guards no earlier module could enforce live.
//!
//! The first is a rename guard. A base with no extension and a base whose
//! extension is all digits generate byte-identical names: under `numeric`
//! naming `/var/log/web` rotates into `/var/log/web.1`, which may be the
//! live log of a different sheep whose configured path is literally
//! `/var/log/web.1`. Both readings of that name are correct, so
//! `naming::match_generation` cannot tell them apart and it is not its job
//! to. Only the shepherd knows which paths are being written into, so only a
//! tick can refuse, and it refuses by leaving the whole base alone.
//! Renaming a live log out from under an open descriptor is worse than
//! deleting it: shep keeps writing into a path that no longer exists, and
//! the operator's log simply stops appearing.
//!
//! The second is the `protected` [`FileSet`] [`prune::tidy`] takes, built
//! here from every log path the whole flock reported. The collision is
//! between DIFFERENT sheep, so a per-sheep set would miss exactly the case
//! that matters.
//!
//! # This dog rotates its own logs, on purpose
//!
//! A dog is an ordinary supervised entry with a marker, so `ListFlock`
//! reports the metrics dog, the bark dog and this rotator right alongside
//! the flock. Rotating them is the decision rather than an accident that
//! happens to work: dog logs grow like any other, and "your log directory is
//! bounded, except for these four files" is the surprising behaviour. There
//! is no loop, because reopening its own log does not itself cause a
//! rotation.
//!
//! It does mean this dog has to be quiet. A line per file per interval would
//! make its own log the busiest file in the directory it exists to keep
//! small, so [`Report::summary`] returns one line for a tick that rotated
//! something and nothing at all for a tick that did not.
//!
//! [`prune::tidy`]: crate::prune::tidy

use core::fmt;
use std::{collections::BTreeMap, path::Path, sync::Arc, time::SystemTime};

use shep_client::{
    LinkState, RequestError,
    dogs::DogRuntime,
    shep_core::{
        config::DogTable,
        protocol::{ProcessInfo, Request, Response, RpcErrorCode, SelectorSpec},
    },
};

use crate::{
    config::Config, error::Error, file_set::FileSet, naming::LogPath, prune::tidy, rotate::rotate,
    stop::Stop,
};

mod judge;
mod report;

use judge::{Rules, Survey, collides_with_a_live_log, log_paths, sheep_rules, survey};
use report::Report;

/// The three things one tick asks the shepherd for.
///
/// Narrow on purpose: with the socket behind three methods, the whole of the
/// orchestration below is testable without a daemon. There is one real
/// implementation, [`Live`], and one fake in this module's tests.
///
/// The `async fn` here is used through a generic bound only, never behind
/// `dyn`. That is a decision rather than an accident: an `async fn` in a
/// trait cannot spell an auto-trait bound on the future it returns, so a
/// caller needing `Send` could not ask for one. This binary runs a single
/// current-thread runtime and has no such caller. The `async_fn_in_trait`
/// lint stays quiet here because a binary's trait is not a public surface;
/// lift this into a library and it starts firing, at which point the
/// trade-off wants deciding rather than silencing.
pub trait Daemon {
    /// The dog's own `[<name>]` section of `dogs.toml`, as TOML text.
    ///
    /// Empty when `dogs.toml` has no such section, which is the ordinary
    /// case for a dog running on its defaults.
    ///
    /// # Errors
    /// [`Error::Shepherd`] if the shepherd cannot be reached or refuses, and
    /// [`Error::Protocol`] if it answers with something other than a dog
    /// section.
    async fn dog_config(&self, name: &str) -> Result<String, Error>;

    /// Each sheep's own `[app.dogs.<name>]` table, by sheep name. A sheep
    /// with no such table is absent.
    ///
    /// # Errors
    /// As [`Self::dog_config`].
    async fn sheep_tables(&self, name: &str) -> Result<BTreeMap<String, DogTable>, Error>;

    /// Every supervised entry, dogs included.
    ///
    /// # Errors
    /// As [`Self::dog_config`].
    async fn list_flock(&self) -> Result<Vec<ProcessInfo>, Error>;

    /// Ask one sheep, by name, to reopen its log files.
    ///
    /// # Errors
    /// As [`Self::dog_config`].
    async fn reopen(&self, name: &str) -> Result<(), Error>;
}

/// One full rotation pass.
///
/// Returns the [`Config`] it read as well as the report, because the caller
/// needs `interval` to decide how long to sleep and re-reading it separately
/// would be a second request for a value already in hand.
///
/// The order is load-bearing:
///
/// 1. Read the config, and every sheep's own table over it. Never cached:
///    the daemon re-reads its side per request, and a dog that cached this
///    would undo that, so changing `max_size` would need a `shep disable` and
///    a `shep enable` rather than taking effect on the next tick. A sheep
///    whose table does not parse is left alone this tick, and so is every log
///    it shares with another sheep, because the sheep that does not parse
///    would never be reopened after a rename.
/// 2. List the flock, and build the protected set from all of it, left-alone
///    sheep included.
/// 3. Stat and qualify every log path, before renaming any of them, so that
///    two sheep sharing one path both see it as it was.
/// 4. Per sheep: rename its qualifying files, then reopen that sheep alone.
/// 5. Only once the reopens are done, compress and prune, and only the logs
///    whose every sheep reopened. Compressing a large log is not quick, and
///    doing it first would widen the stretch during which shep is still
///    writing through its old descriptor. Compressing one whose sheep never
///    reopened would read a file mid-append and then delete what shep is
///    writing to.
///
/// `stop` is consulted at two places: before each sheep's turn in the
/// rename loop, and in the tidy loop. Never between a rename and its reopen.
/// Those are quick, and a tick interrupted between them leaves shep writing
/// into a file with the wrong name, which is worse than the wait. Between
/// sheep nothing is waiting on a reopen, so a stop there ends the renaming:
/// the shepherd's `SIGKILL` follows a stop by `kill_timeout`, and a flock
/// with many qualifying sheep could otherwise still be renaming when it
/// lands. The remaining sheep still get their turn, but a stopping turn
/// renames nothing and only reopens a sheep whose file an earlier sheep
/// already renamed, because that sheep would otherwise write into the
/// renamed generation for good. The gzip of a large generation is the slow
/// part: it
/// runs on a blocking thread, a stop already requested when its turn comes
/// skips it, and a stop arriving while it runs abandons it. The half-written
/// `.gz` an abandoned gzip leaves is the state the next pass recovers from.
///
/// # Errors
/// - [`Error::Config`] if the `[<name>]` section cannot be understood.
///   Nothing is touched in that case; the tick reads the config first for
///   exactly that reason. A sheep's own table that cannot be understood is
///   not this error: it is reported in [`Report::left_alone`].
/// - [`Error::Shepherd`] or [`Error::Protocol`] if the config, the sheep's
///   tables or the flock listing cannot be had. A refused *reopen* is not an error: it is
///   reported in [`Report::reopen_failed`], because shep is still writing into
///   the renamed file through its existing descriptor, so nothing is lost and
///   the next successful reopen recovers.
///
/// A fault on disk is never an error from here: a rename, a compression, a
/// deletion or a directory listing that fails is that one log's problem,
/// reported in the [`Report`] beside everything else the tick did, and the
/// next log is still handled. Returning it instead would throw the rest
/// of the report away with it, and let one directory's permission bit stop
/// every other sheep's rotation. Only the daemon can stop a tick: a config
/// it cannot read, a request it cannot make, or a reopen it is refused.
///
/// # Panics
/// If `prune::tidy` panics on its blocking thread. The panic is re-raised
/// here with its original payload, so the process fails the way it would
/// have with tidy running in place; the location reported is the tidy
/// thread's, not this function's caller. No `#[track_caller]`: on an async
/// fn the attribute is a no-op, and clippy says so.
pub async fn tick<D: Daemon>(
    daemon: &D,
    dog_name: &str,
    now: SystemTime,
    stop: &mut Stop,
) -> Result<(Config, Report), Error> {
    // Named at the point of failure rather than through a `From` impl: the
    // section is `[<name>]` for whatever this dog was adopted as, and an
    // error naming some other block sends the reader to a section they never
    // wrote.
    let in_section = |source| Error::Config {
        section: dog_name.to_owned(),
        source,
    };
    let text = daemon.dog_config(dog_name).await?;
    let config = Config::from_toml(&text).map_err(in_section)?;
    let section = Config::section_table(&text).map_err(in_section)?;
    let tables = daemon.sheep_tables(dog_name).await?;
    let flock = daemon.list_flock().await?;
    let mut report = Report::default();
    let Rules {
        settings,
        left_alone,
    } = sheep_rules(&config, &section, &tables, &flock, &mut report);
    // Every live log path the flock reported, in one set, because the
    // collision it exists to catch is between different sheep.
    let protected = Arc::new(FileSet::from_paths(
        flock.iter().flat_map(log_paths).map(Path::new),
    ));
    // Every path a sheep left alone writes to. Another sheep sharing one would
    // rename it out from under a sheep that is never reopened, so the file is
    // left alone for everyone.
    let held = FileSet::from_paths(
        flock
            .iter()
            .filter(|sheep| left_alone.contains(&sheep.name))
            .flat_map(log_paths)
            .map(Path::new),
    );

    let Survey {
        order,
        groups,
        owners,
    } = survey(
        &flock,
        &settings,
        &config,
        &protected,
        &held,
        now,
        &mut report,
    );

    // Rename, then reopen, one sheep at a time rather than one reopen for
    // the batch: `Request::Reopen` takes a single selector and there is no
    // multi-name variant. Per sheep is the better shape anyway, because the
    // rename-to-reopen window is then one sheep wide rather than the whole
    // tick.
    // A `FileSet` rather than a set of paths, for the same reason `protected`
    // is one: two sheep can be handed one file under two spellings, and a
    // second rename of a file that has already moved fails with NotFound and
    // leaves the second sheep never reopened.
    let mut renamed = FileSet::default();
    // Bases whose sheep has reopened, so shep has stopped writing into what
    // was renamed. Only those are safe to compress or delete.
    let mut to_tidy: Vec<(LogPath, Config)> = Vec::new();
    // Files some sheep failed to rename this tick. A later sheep sharing
    // the file can succeed where that one failed, and the first sheep,
    // reopened before the file moved or never reopened at all, then writes
    // into the renamed generation through its old descriptor. Such a file
    // is held back from tidy however many sheep did reopen it.
    let mut faulted = FileSet::default();
    // Positions in `order` of the sheep whose reopen was refused and every
    // sheep after it, none of which was reopened this tick.
    let mut not_reopened: Vec<usize> = Vec::new();

    for (position, name) in order.iter().enumerate() {
        // The one stop check outside the tidy loop, and only here: between
        // sheep, where the last one's turn is complete and this one's has
        // not begun, so nothing is waiting on a reopen. A stop is the first
        // half of the shepherd's kill ladder, and its `SIGKILL` follows at
        // `kill_timeout` wherever the tick is. Starting another rename then
        // would risk that cut landing between the rename and its reopen, so
        // a stopping turn renames nothing. Leaving a log alone costs one
        // interval, since the next tick finds it still over the limit.
        //
        // The turn is not skipped, though. A sheep sharing a path that an
        // earlier sheep already renamed is writing into that renamed file
        // through its own descriptor, and only this dog ever asks shep to
        // reopen it, so it still gets the reopen it needs. That costs one
        // request and starts no rename.
        let stopping = stop.requested();
        let rules = settings.get(name).unwrap_or(&config);
        let mut rotated_any = false;
        let mut rotated_here: Vec<(LogPath, Config)> = Vec::new();
        for base in &groups[name] {
            // Through `protected` rather than `ResolvedDir::of`, so this
            // base is resolved the way the protected set was, at the moment
            // the set was built. See the `file_set` module docs.
            let dir = protected.resolve(&base.dir);
            let live_name = base.live_name();
            if renamed.contains(&dir, &live_name) {
                // Another sheep shares this path and has already rotated it.
                // This one is still writing through its own descriptor, so
                // it still needs the reopen below.
                rotated_any = true;
                continue;
            }
            if stopping {
                continue;
            }
            if collides_with_a_live_log(base, &dir, rules.naming, &protected) {
                report.skipped_collision += 1;
                continue;
            }
            match rotate(base, rules.naming, now) {
                Ok(_generation) => {
                    // The generations are pruned to the most any owner of
                    // the file asked to keep, so no sheep sharing it loses
                    // history it asked for. The renaming sheep's own rules
                    // otherwise stand: it names and compresses.
                    let keep = owners
                        .get(&(dir.clone(), live_name.clone()))
                        .and_then(|owned| owned.iter().map(|owner| owner.keep).max())
                        .unwrap_or(rules.keep);
                    rotated_here.push((
                        base.clone(),
                        Config {
                            keep,
                            ..rules.clone()
                        },
                    ));
                    renamed.insert(dir, live_name);
                    report.rotated += 1;
                    rotated_any = true;
                }
                // A rename that fails leaves the live file where it was, so
                // this log is simply not rotated this tick. That is this
                // log's problem, said so, and no reason to leave the next
                // one alone.
                Err(err) => {
                    report.rename_failed.push(err.to_string());
                    faulted.insert(dir, live_name);
                }
            }
        }

        if rotated_any {
            if let Err(err) = daemon.reopen(name).await {
                // Rotating further sheep while the reopen path is broken
                // turns one recoverable state into several confusing ones.
                // This sheep and every one after it goes un-reopened.
                report.reopen_failed = Some(format!("{name}: {err}"));
                not_reopened.extend(position..order.len());
                break;
            }
            to_tidy.extend(rotated_here);
        }
    }

    // A sheep not reopened this tick is still writing into whatever was
    // renamed out from under it, through its old descriptor. Two sheep can
    // share one log path, so a base a reopened sheep rotated may be that
    // same file: compressing it would read it mid-append, and removing the
    // plain copy afterwards would delete what shep is writing to. Anything
    // such a sheep writes to, and any file a rename failed on, is left for
    // a later tick.
    let mut still_writing = faulted;
    for &position in &not_reopened {
        for base in &groups[&order[position]] {
            still_writing.insert(protected.resolve(&base.dir), base.live_name());
        }
    }
    to_tidy.retain(|(base, _)| {
        !still_writing.contains(&protected.resolve(&base.dir), &base.live_name())
    });

    tidy_all(&to_tidy, &protected, &mut report, stop).await;
    Ok((config, report))
}

/// Compress and prune each of `bases`, adding what happened to `report`.
///
/// Each base is tidied on a blocking thread rather than on the runtime's
/// own. This binary runs one current-thread runtime, and the client's
/// supervisor lives on it: a gzip run in place would hold that thread for
/// as long as the gzip takes, so a shepherd restarting underneath a large
/// compression would not be reconnected to until it finished, and the next
/// tick would fail for it. Off the thread, the supervisor reconnects while
/// the gzip runs.
///
/// A stop already requested when a base's turn comes skips it and every
/// base after it. A stop arriving while a gzip runs abandons that gzip: the
/// thread finishes or is cut off when the process exits, and either way
/// the plain file is still there, since it is removed only after the `.gz`
/// is complete and synced. Whatever was already counted stays counted; it
/// describes work that was done.
///
/// A generation whose compression or deletion fails is reported in
/// [`Report::tidy_failed`], and so is a base whose directory cannot be
/// listed; the next generation and the next base are still tidied. The
/// fault is that generation's, and the generation is left as it was.
async fn tidy_all(
    bases: &[(LogPath, Config)],
    protected: &Arc<FileSet>,
    report: &mut Report,
    stop: &mut Stop,
) {
    for (base, config) in bases {
        if stop.requested() {
            return;
        }
        let gzip = tokio::task::spawn_blocking({
            let (base, config, protected) = (base.clone(), config.clone(), Arc::clone(protected));
            move || tidy(&base, &config, &protected)
        });
        let joined = tokio::select! {
            // Biased, gzip first: a tidy that has already finished has
            // counts and possibly an error in hand, and a stop that landed
            // in the same instant must not throw them away.
            biased;
            joined = gzip => joined,
            () = stop.wait() => return,
        };
        let mut tidied = match joined {
            Ok(Ok(tidied)) => tidied,
            Ok(Err(err)) => {
                report.tidy_failed.push(err.to_string());
                continue;
            }
            Err(join) => match join.try_into_panic() {
                // A panic in tidy is a bug, and it is re-raised here so the
                // process fails the way it would have with tidy in place.
                Ok(payload) => std::panic::resume_unwind(payload),
                // A blocking task is cancelled only by a runtime shutting
                // down, and this future has been dropped by then.
                Err(join) => {
                    unreachable!("a blocking task was cancelled under a live runtime: {join}")
                }
            },
        };
        report.compressed += tidied.compressed;
        report.deleted += tidied.deleted;
        report.skipped_protected += tidied.skipped_protected;
        report.tidy_failed.append(&mut tidied.faults);
    }
}

/// [`Daemon`] over a live session with the shepherd.
///
/// A [`DogRuntime`] rather than a bare [`Client`](shep_client::Client), because
/// a dog outlives the
/// daemon it connected to: shep hands over to a successor and the dog is still
/// running with a dead socket in its hand. `DogRuntime` holds the
/// [`ReconnectingClient`](shep_client::ReconnectingClient) that survives that,
/// and the reconnect is the client's own business, so nothing above this type
/// reconnects. See `tick::tests::reconnect` for the tick that proves it.
///
/// The section `DogRuntime` read on the way in is not what a tick runs on.
/// [`Daemon::dog_config`] asks again on every tick, because the daemon re-reads
/// its own side per request and a dog that served the startup answer out of
/// memory would undo that: changing `max_size` would need a `shep disable` and
/// a `shep enable` rather than taking effect on the next tick.
///
/// `Debug` is written rather than derived, and it prints nothing about the
/// session. [`ReconnectingClient`](shep_client::ReconnectingClient)'s own
/// `Debug` carries its socket path, which
/// is `$SHEP_HOME/run/shep.sock` and so usually sits under somebody's home
/// directory, and `DogRuntime`'s carries the identity and the section's length.
/// Neither is worth printing here: this is a `pub` type, so a consumer that
/// logged one, or a `#[derive(Debug)]` on a struct that held one, would put
/// that path somewhere an operator later pastes into a bug report. The socket
/// comes from the environment, the announced name is already in every message
/// this dog writes, and the link state and the section are both readable from
/// the runtime itself when they are wanted.
///
/// Pinned by `debug_says_nothing_about_the_connection`, because a later
/// `#[derive(Debug)]` here would be a silent regression.
pub struct Live(DogRuntime);

impl fmt::Debug for Live {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Live(<shepherd session>)")
    }
}

impl Live {
    /// Wrap a session opened by [`DogRuntime::start`](shep_client::dogs::DogRuntime::start).
    #[must_use]
    pub fn new(runtime: DogRuntime) -> Self {
        Self(runtime)
    }

    /// What the client's supervisor is doing right now.
    ///
    /// On [`Daemon`] deliberately not: a fake has no connection to report
    /// on, and the one caller that asks is the poll loop, which only ever
    /// asks a real one. See `main::poll` for what it does with the answer.
    #[must_use]
    pub fn link(&self) -> LinkState {
        self.0.client().link()
    }
}

impl Daemon for Live {
    async fn dog_config(&self, name: &str) -> Result<String, Error> {
        // `Client::dog_config` rather than the plain `Request::DogConfig`:
        // it asks `Request::DogConfigResolved`, which is what lets an
        // operator write a `{{secret:...}}` reference into this dog's section
        // and have the shepherd expand it before handing the section over. A
        // shepherd too old for that request is answered the plain one by the
        // client itself, so this dog still runs against one.
        Ok(self.0.client().dog_config(name).await?.as_str().to_owned())
    }

    async fn sheep_tables(&self, name: &str) -> Result<BTreeMap<String, DogTable>, Error> {
        let asked = Request::DogSheepSettings {
            dog: name.to_owned(),
        };
        tables_from(self.0.client().request(asked).await)
    }

    async fn list_flock(&self) -> Result<Vec<ProcessInfo>, Error> {
        match self.0.client().request(Request::ListFlock).await? {
            Response::Flock(flock) => Ok(flock),
            other => Err(unexpected("ListFlock", &other)),
        }
    }

    async fn reopen(&self, name: &str) -> Result<(), Error> {
        let asked = Request::Reopen {
            selector: SelectorSpec::Name(name.to_owned()),
        };
        match self.0.client().request(asked).await? {
            Response::Reopened(_) => Ok(()),
            other => Err(unexpected("Reopen", &other)),
        }
    }
}

/// The sheep tables an answer to `DogSheepSettings` stands for.
///
/// A shepherd that does not know the request refuses it with `Unsupported`,
/// or with `Unrecognized` when the refusal's own code is one this build was
/// not taught. Such a shepherd has no `[app.dogs.<name>]` table to serve, so
/// no sheep has a rule of its own and every one rotates on the section, which
/// is what no tables at all means. Failing the tick instead would leave this
/// dog unable to rotate anything on a shepherd that runs perfectly well,
/// the same reason the client's own `dog_config` falls back for a section.
/// Any other refusal still stops the tick: a `Forbidden` or a dropped link is
/// a fault the operator has to hear about, not a missing feature.
fn tables_from(
    answer: Result<Response, RequestError>,
) -> Result<BTreeMap<String, DogTable>, Error> {
    match answer {
        Ok(Response::DogSheepSettings { tables }) => Ok(tables),
        Ok(other) => Err(unexpected("DogSheepSettings", &other)),
        Err(RequestError::Rpc(refusal))
            if matches!(
                refusal.code,
                RpcErrorCode::Unsupported | RpcErrorCode::Unrecognized
            ) =>
        {
            Ok(BTreeMap::new())
        }
        Err(err) => Err(err.into()),
    }
}

/// The shepherd answered something this dog cannot use.
fn unexpected(asked: &str, got: &Response) -> Error {
    Error::Protocol(format!("{} in answer to {asked}", named(got)))
}

/// Name a response without printing its body.
///
/// `Debug` alone would do for the last arm only: a listing variant carries
/// its whole listing, and a `DogSection` carries a section that routinely
/// holds webhook credentials. Naming the variants this dog asks for by hand
/// keeps both out of an error message.
fn named(response: &Response) -> String {
    match response {
        Response::Pong => "Pong".to_owned(),
        Response::Flock(flock) => format!("a Flock of {}", flock.len()),
        Response::DogSection { .. } => "a DogSection".to_owned(),
        Response::DogSheepSettings { tables } => {
            format!("a DogSheepSettings of {}", tables.len())
        }
        Response::Reopened(sheep) => format!("a Reopened of {}", sheep.len()),
        // `Response` is `#[non_exhaustive]`, so this arm is not optional. A
        // variant added to the protocol after this dog was written is
        // exactly the one worth naming, and only `Debug` can name it.
        // Truncated, because some of them are listings.
        other => format!("{other:?}").chars().take(60).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::assert_no_dashes;
    use shep_client::shep_core::status::ProcStatus;
    use std::cell::RefCell;
    // The seeding these tests do is now the only use of `fs` in this file.
    use std::fs;

    mod faults;
    mod live;
    mod reconnect;
    mod sheep;
    mod stop;
    /// What a test runs on every reopen, handed the sheep's name.
    ///
    /// `pub(super)` so `judge`'s own tests can reach these fixtures too.
    pub(super) type Hook = Box<dyn Fn(&str)>;

    /// A daemon that answers from a script and records what it was asked.
    pub(super) struct Fake {
        pub(super) config: String,
        /// Each sheep's own table, as TOML text.
        pub(super) tables: Vec<(String, String)>,
        pub(super) flock: Vec<ProcessInfo>,
        pub(super) reopen_fails: Option<String>,
        pub(super) reopened: RefCell<Vec<String>>,
        /// Run on every reopen, before the answer: the one hook a test has
        /// between one sheep's turn and the next.
        pub(super) on_reopen: Option<Hook>,
    }

    impl Fake {
        /// This daemon, with `sheep` carrying `[app.dogs.<name>]` set to `table`.
        pub(super) fn with_table(mut self, sheep: &str, table: &str) -> Self {
            self.tables.push((sheep.to_owned(), table.to_owned()));
            self
        }

        /// A daemon serving `config` and `flock` that grants every reopen.
        pub(super) fn new(config: &str, flock: Vec<ProcessInfo>) -> Self {
            Self {
                config: config.to_owned(),
                tables: Vec::new(),
                flock,
                reopen_fails: None,
                reopened: RefCell::new(Vec::new()),
                on_reopen: None,
            }
        }
    }

    impl Daemon for Fake {
        async fn dog_config(&self, _name: &str) -> Result<String, Error> {
            Ok(self.config.clone())
        }
        async fn sheep_tables(&self, _name: &str) -> Result<BTreeMap<String, DogTable>, Error> {
            Ok(self
                .tables
                .iter()
                .map(|(sheep, text)| {
                    let table: serde_json::Map<String, serde_json::Value> =
                        toml::from_str(text).expect("a table the test wrote is TOML");
                    (sheep.clone(), DogTable::from(table))
                })
                .collect())
        }
        async fn list_flock(&self) -> Result<Vec<ProcessInfo>, Error> {
            Ok(self.flock.clone())
        }
        async fn reopen(&self, name: &str) -> Result<(), Error> {
            self.reopened.borrow_mut().push(name.to_owned());
            if let Some(hook) = &self.on_reopen {
                hook(name);
            }
            match &self.reopen_fails {
                Some(why) if why == name => Err(Error::Protocol(format!("refused for {name}"))),
                _ => Ok(()),
            }
        }
    }

    // `ProcessInfo` is `#[non_exhaustive]`, so an outside crate CANNOT write
    // it as a struct literal. The builder is the only way in, and finding
    // that out is exactly the kind of thing this project exists to find out.
    pub(super) fn sheep(
        name: &str,
        out: Option<&std::path::Path>,
        err: Option<&std::path::Path>,
    ) -> ProcessInfo {
        ProcessInfo::builder(0, name, ProcStatus::Online)
            .out_file(out.map(|path| path.display().to_string()))
            .err_file(err.map(|path| path.display().to_string()))
            .build()
    }

    /// One tick against `fake`, with a stop nothing will ever request.
    pub(super) async fn run(fake: &Fake, now: SystemTime) -> Result<(Config, Report), Error> {
        tick(fake, "log-rotate", now, &mut Stop::never()).await
    }

    /// A `now` a year ahead of the clock, so a file written moments ago is
    /// unambiguously older than any `max_age` a test sets.
    pub(super) fn a_year_on() -> SystemTime {
        SystemTime::now() + core::time::Duration::from_secs(31_536_000)
    }

    #[tokio::test]
    async fn a_file_over_max_size_is_rotated_and_its_sheep_reopened() {
        let dir = tempfile::tempdir().expect("tempdir");
        let out = dir.path().join("web-0-out.log");
        fs::write(&out, "x".repeat(2048)).expect("seeded");
        let fake = Fake::new("max_size = \"1K\"\n", vec![sheep("web", Some(&out), None)]);

        let (_config, report) = run(&fake, SystemTime::now()).await.expect("ticked");

        assert_eq!(report.rotated, 1);
        assert!(!out.exists(), "the live path is free for shep to reopen");
        assert_eq!(*fake.reopened.borrow(), vec!["web".to_owned()]);
    }

    #[tokio::test]
    async fn a_file_under_max_size_is_left_alone_and_nothing_is_reopened() {
        let dir = tempfile::tempdir().expect("tempdir");
        let out = dir.path().join("web-0-out.log");
        fs::write(&out, "small\n").expect("seeded");
        let fake = Fake::new("max_size = \"1M\"\n", vec![sheep("web", Some(&out), None)]);

        let (_config, report) = run(&fake, SystemTime::now()).await.expect("ticked");

        assert_eq!(report.rotated, 0);
        assert!(out.exists());
        assert!(
            fake.reopened.borrow().is_empty(),
            "no rotation means no reopen"
        );
    }

    #[tokio::test]
    async fn a_sheep_with_no_log_file_yet_is_skipped_not_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let fake = Fake::new(
            "",
            vec![sheep(
                "web",
                Some(&dir.path().join("never-started.log")),
                None,
            )],
        );

        let (_config, report) = run(&fake, SystemTime::now())
            .await
            .expect("a missing log file is ordinary");

        assert_eq!(report.skipped, 1);
        assert_eq!(report.rotated, 0);
    }

    #[tokio::test]
    async fn a_refused_reopen_stops_the_tick_rather_than_rotating_on() {
        let dir = tempfile::tempdir().expect("tempdir");
        let first = dir.path().join("api-0-out.log");
        let second = dir.path().join("web-0-out.log");
        fs::write(&first, "x".repeat(2048)).expect("seeded");
        fs::write(&second, "x".repeat(2048)).expect("seeded");
        let fake = Fake {
            reopen_fails: Some("api".into()),
            ..Fake::new(
                "max_size = \"1K\"\n",
                vec![
                    sheep("api", Some(&first), None),
                    sheep("web", Some(&second), None),
                ],
            )
        };

        let (_config, report) = run(&fake, SystemTime::now())
            .await
            .expect("a refused reopen is reported, not returned as an error");

        assert!(report.reopen_failed.is_some(), "the failure is reported");
        assert_eq!(
            *fake.reopened.borrow(),
            vec!["api".to_owned()],
            "web was never reached"
        );
        assert!(
            second.exists(),
            "rotating on through a broken reopen multiplies a recoverable state"
        );
    }

    #[tokio::test]
    async fn two_sheep_sharing_one_log_path_rotate_it_once_and_both_reopen() {
        let dir = tempfile::tempdir().expect("tempdir");
        let shared = dir.path().join("web-out.log");
        fs::write(&shared, "x".repeat(2048)).expect("seeded");
        let fake = Fake::new(
            "max_size = \"1K\"\n",
            vec![
                sheep("web-0", Some(&shared), None),
                sheep("web-1", Some(&shared), None),
            ],
        );

        let (_config, report) = run(&fake, SystemTime::now()).await.expect("ticked");

        assert_eq!(report.rotated, 1, "one path, one rename");
        let reopened = fake.reopened.borrow().clone();
        assert!(reopened.contains(&"web-0".to_owned()));
        assert!(reopened.contains(&"web-1".to_owned()));
    }

    #[tokio::test]
    async fn a_bad_config_section_fails_the_tick_without_touching_a_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let out = dir.path().join("web-0-out.log");
        fs::write(&out, "x".repeat(4096)).expect("seeded");
        let fake = Fake::new(
            "max_size = \"10MB\"\n",
            vec![sheep("web", Some(&out), None)],
        );

        run(&fake, SystemTime::now())
            .await
            .expect_err("10MB is not a spelling shep accepts");

        assert!(
            out.exists(),
            "a config this dog cannot read means it touches nothing"
        );
    }

    #[tokio::test]
    async fn a_rename_that_fails_still_reopens_the_sheep_it_half_rotated() {
        // Propagating the failure straight out would leave the file that DID
        // rotate with no reopen behind it, and shep writing into a renamed
        // file that no later tick ever revisits.
        let dir = tempfile::tempdir().expect("tempdir");
        let out = dir.path().join("api-0-out.log");
        let err = dir.path().join("api-0-err.log");
        fs::write(&out, "x".repeat(2048)).expect("seeded");
        fs::write(&err, "x".repeat(2048)).expect("seeded");
        // The last generation number there is, so the shift has nowhere to
        // go and `rotate` refuses rather than wrapping.
        fs::write(dir.path().join("api-0-err.log.4294967295"), "oldest").expect("seeded");
        let fake = Fake::new(
            "max_size = \"1K\"\nnaming = \"numeric\"\n",
            vec![sheep("api", Some(&out), Some(&err))],
        );

        let (_config, report) = run(&fake, SystemTime::now())
            .await
            .expect("a rename fault is reported, not returned as an error");

        let why = report
            .rename_failed
            .first()
            .expect("the fault is in the report");
        assert!(why.contains("api-0-err.log"), "{why}");
        assert!(why.contains("no generation numbers left"), "{why}");
        assert_eq!(
            *fake.reopened.borrow(),
            vec!["api".to_owned()],
            "the half that rotated still needs its reopen"
        );
        assert!(dir.path().join("api-0-out.log.1").exists());
        assert!(
            report.summary().expect("a line").contains("to rename"),
            "the fault reaches the summary line"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn two_sheep_sharing_one_log_under_two_spellings_rotate_it_once_and_both_reopen() {
        // The same file, reached through a symlinked directory by one sheep
        // and directly by the other. Path equality reads those as two files,
        // so the second rename ran against a path the first had already
        // moved, failed NotFound, and the second sheep was never reopened:
        // it went on writing into the renamed generation for good.
        let dir = tempfile::tempdir().expect("tempdir");
        let real = dir.path().join("real");
        fs::create_dir(&real).expect("real");
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).expect("symlink");
        let through_link = link.join("web-out.log");
        let direct = real.join("web-out.log");
        fs::write(&direct, "x".repeat(2048)).expect("seeded");
        let fake = Fake::new(
            "max_size = \"1K\"\n",
            vec![
                sheep("web-0", Some(&through_link), None),
                sheep("web-1", Some(&direct), None),
            ],
        );

        let (_config, report) = run(&fake, SystemTime::now())
            .await
            .expect("one file under two spellings is one rotation, not an error");

        assert_eq!(report.rotated, 1, "one file, one rename");
        let reopened = fake.reopened.borrow().clone();
        assert!(reopened.contains(&"web-0".to_owned()));
        assert!(
            reopened.contains(&"web-1".to_owned()),
            "the second sheep still holds the old descriptor and needs its reopen"
        );
    }

    #[tokio::test]
    async fn a_refused_reopen_still_tidies_the_sheep_that_did_reopen() {
        // api rotates and reopens, then web's reopen is refused. api's
        // generations are safe to tidy: shep has stopped writing into the
        // one that was renamed. Leaving them for api's next rotation is
        // what used to happen, and a sheep that grows slowly waited weeks.
        let dir = tempfile::tempdir().expect("tempdir");
        let api = dir.path().join("api-0-out.log");
        let web = dir.path().join("web-0-out.log");
        fs::write(&api, "x".repeat(2048)).expect("seeded");
        fs::write(&web, "x".repeat(2048)).expect("seeded");
        for second in [1, 2] {
            fs::write(
                dir.path()
                    .join(format!("api-0-out.2026-08-20T15-04-0{second}.log")),
                "old\n",
            )
            .expect("seeded");
        }
        let webs_old = dir.path().join("web-0-out.2026-08-20T15-04-01.log");
        fs::write(&webs_old, "old\n").expect("seeded");
        let fake = Fake {
            reopen_fails: Some("web".into()),
            ..Fake::new(
                "max_size = \"1K\"\nkeep = 1\ncompress = false\n",
                vec![
                    sheep("api", Some(&api), None),
                    sheep("web", Some(&web), None),
                ],
            )
        };

        let (_config, report) = run(&fake, SystemTime::now())
            .await
            .expect("a refused reopen is reported, not returned as an error");

        assert!(report.reopen_failed.is_some());
        assert_eq!(report.rotated, 2, "both rotated before the refusal");
        assert_eq!(
            report.deleted, 2,
            "api's two old generations went, and only those"
        );
        assert!(
            webs_old.exists(),
            "web never reopened, so nothing of web's may be touched"
        );
    }

    #[tokio::test]
    async fn a_refused_reopen_leaves_a_shared_path_alone() {
        // web-0 and web-1 write to one file. web-0 reopens, web-1 is
        // refused, on two ticks running. After the first tick web-1 is
        // still writing into that tick's generation through its old
        // descriptor. After the second, that generation is no longer the
        // newest, which is the point at which keep = 1 would delete it and
        // compress = true would gzip it, if the guard did not hold.
        let dir = tempfile::tempdir().expect("tempdir");
        let shared = dir.path().join("web-out.log");
        let fake = Fake {
            reopen_fails: Some("web-1".into()),
            ..Fake::new(
                "max_size = \"1K\"\nkeep = 1\n",
                vec![
                    sheep("web-0", Some(&shared), None),
                    sheep("web-1", Some(&shared), None),
                ],
            )
        };
        let first_tick = SystemTime::UNIX_EPOCH + core::time::Duration::from_secs(1_787_324_645);
        let second_tick = first_tick + core::time::Duration::from_secs(60);

        fs::write(&shared, "x".repeat(2048)).expect("seeded");
        let (_config, first) = run(&fake, first_tick).await.expect("ticked");
        assert_eq!(first.rotated, 1);
        assert!(first.reopen_failed.is_some());
        let held_open = dir.path().join("web-out.2026-08-21T15-04-05.log");
        assert!(held_open.exists(), "the first tick's generation");

        fs::write(&shared, "y".repeat(2048)).expect("grown again");
        let (_config, second) = run(&fake, second_tick).await.expect("ticked");

        assert_eq!(second.rotated, 1);
        assert_eq!(
            second.deleted, 0,
            "web-1 still writes into the older generation"
        );
        assert_eq!(second.compressed, 0);
        assert!(held_open.exists());
        assert!(
            !dir.path()
                .join("web-out.2026-08-21T15-04-05.log.gz")
                .exists()
        );
    }

    #[tokio::test]
    async fn a_rename_fault_does_not_stop_the_sheeps_other_logs() {
        // beta's out log faults. Its err log is alpha's out log, which alpha
        // rotated: beta still reaches it, counts as rotated through it, and
        // is reopened, so the shared log is safe to tidy after all.
        let dir = tempfile::tempdir().expect("tempdir");
        let shared = dir.path().join("shared.log");
        let betas_out = dir.path().join("beta-0-out.log");
        fs::write(&shared, "x".repeat(2048)).expect("seeded");
        fs::write(&betas_out, "x".repeat(2048)).expect("seeded");
        fs::write(dir.path().join("shared.log.1"), "old\n").expect("seeded");
        fs::write(dir.path().join("beta-0-out.log.4294967295"), "oldest").expect("seeded");
        let fake = Fake::new(
            "max_size = \"1K\"\nnaming = \"numeric\"\nkeep = 1\ncompress = false\n",
            vec![
                sheep("alpha", Some(&shared), None),
                sheep("beta", Some(&betas_out), Some(&shared)),
            ],
        );

        let (_config, report) = run(&fake, SystemTime::now()).await.expect("ticked");

        assert_eq!(
            report.rename_failed.len(),
            1,
            "beta's out log could not rotate"
        );
        assert_eq!(
            *fake.reopened.borrow(),
            vec!["alpha".to_owned(), "beta".to_owned()],
            "beta shares the rotated log, so it is reopened for it"
        );
        assert_eq!(
            report.deleted, 1,
            "both sheep reopened, so shared.log.2 went"
        );
        assert!(
            betas_out.exists(),
            "the log that could not rotate is where it was"
        );
    }

    #[tokio::test]
    async fn a_rename_that_fails_still_tidies_what_did_reopen() {
        // api's out log rotates, its err log cannot (no generation numbers
        // left), api is reopened for the half that moved. The out log's
        // generations are then safe to tidy, and the rename fault is still
        // the error the tick returns.
        let dir = tempfile::tempdir().expect("tempdir");
        let out = dir.path().join("api-0-out.log");
        let err = dir.path().join("api-0-err.log");
        fs::write(&out, "x".repeat(2048)).expect("seeded");
        fs::write(&err, "x".repeat(2048)).expect("seeded");
        fs::write(dir.path().join("api-0-out.log.1"), "old\n").expect("seeded");
        fs::write(dir.path().join("api-0-err.log.4294967295"), "oldest").expect("seeded");
        let fake = Fake::new(
            "max_size = \"1K\"\nnaming = \"numeric\"\nkeep = 1\ncompress = false\n",
            vec![sheep("api", Some(&out), Some(&err))],
        );

        let (_config, report) = run(&fake, SystemTime::now())
            .await
            .expect("a rename fault is reported, not returned as an error");

        assert_eq!(report.rename_failed.len(), 1);
        assert_eq!(*fake.reopened.borrow(), vec!["api".to_owned()]);
        assert!(
            dir.path().join("api-0-out.log.1").exists(),
            "the live file moved to .1"
        );
        assert!(
            !dir.path().join("api-0-out.log.2").exists(),
            "the old generation, shifted to .2, was past keep = 1 and went"
        );
    }
}
