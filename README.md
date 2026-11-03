# shep-log-rotate

[![Crates.io Version](https://img.shields.io/crates/v/shep-log-rotate.svg)](https://crates.io/crates/shep-log-rotate)
[![License](https://img.shields.io/crates/l/shep-log-rotate.svg)](https://github.com/shep-pm/shep-log-rotate#license)
[![MSRV](https://img.shields.io/crates/msrv/shep-log-rotate.svg)](https://crates.io/crates/shep-log-rotate)
[![CI](https://github.com/shep-pm/shep-log-rotate/actions/workflows/test.yml/badge.svg)](https://github.com/shep-pm/shep-log-rotate/actions/workflows/test.yml)

A log-rotation dog for [shep](https://github.com/shep-pm/shep).

shep writes each sheep's stdout and stderr to a file and appends forever.
This dog watches those files, renames the ones that have grown too big or
got too old, asks the shepherd to reopen them, and then gzips and deletes
older generations so the log directory stays bounded.

It is an external dog. Nothing here is built into shep: it is an ordinary
binary you adopt, and it talks to the daemon over the same socket the CLI
uses. [shep-deploy](https://github.com/shep-pm/shep-deploy) is the other
one, and has the same shape.

## Install

```sh
cargo install shep-log-rotate
shep adopt shep-log-rotate
```

Upgrading later wants `--force`:

```sh
cargo install shep-log-rotate --force
shep restart log-rotate
```

That flag is not politeness. This dog links `shep-client`, and its own version
does not change when that does, so after you upgrade shep the rebuild you need
is exactly the one cargo declines to do: it prints `already installed, use
--force to override`, builds nothing, and exits 0. It looks like it worked.

`shep adopt` records the binary in `shep.toml` and starts it. From then on
the shepherd supervises it like anything else in the flock, and `shep dogs`
lists it.

Adopting also asks the binary two questions and reads the answers off its
stdout. `--version` gives the build and the wire protocol it was compiled
against. A dog below the shepherd's floor is refused right there, before
anything is written down. `--schema` gives a JSON Schema for the settings below,
which is what `shep lookout` draws the config pane from. The binary answers
both before it opens a socket, so neither question costs a connection.

The name it is adopted under is the name shep hands it in `$SHEP_DOG_NAME`, and
it is also the config key. `--name` sets it, and defaults to the binary's file
stem with a leading `shep-` stripped, so the command above lands on
`log-rotate` and reads `[log-rotate]`. Pass `--name rotator` and it reads
`[rotator]`. Any name works, as long as the section matches.

Be careful with that second command, though. `shep adopt` checks that a binary
is runnable by actually running it, for about fifty milliseconds, with your
shell's environment. If `$SHEP_HOME` is not set in that shell,
the copy it starts connects to your default shepherd at `~/.shep` and may
get a rotation pass in there before it is killed. That is usually what you
want. If it is not, set `$SHEP_HOME` for the `adopt` command as well as
passing `--home`.

## Configuration

The dog's own settings live in one table in `dogs.toml`, next to `shep.toml`
in your shep home, and they are the value for every sheep. A sheep can set
its own on top of them, in its own config: see "Setting a rule for one
sheep" below. Ask the binary for a starting point:

```sh
shep-log-rotate --print-config >> ~/.shep/dogs.toml
```

Every line it prints is commented, so appending it changes nothing until you
uncomment something.

Older versions of this dog printed `[dog.log-rotate]` for `shep.toml`. Shep
has since moved a dog's settings into a file of their own, and a shepherd
carrying that move migrates the old section on its first boot, then strikes
it from `shep.toml`. Write it back there afterwards and the same dog is
named in both files, which the shepherd refuses to boot on. Check
`dogs.toml` first if you are following an old note.

```toml
[log-rotate]
max_size = "10M"    # rotate once a log reaches this size
max_age  = "168h"   # optionally also rotate this long after the last rotation
keep     = 5        # rotated generations to keep, at least 1
naming   = "dated"  # "dated" or "numeric", see below
compress = true     # gzip all but the newest generation
interval = "60s"    # how often to look
```

| Option | Default | Notes |
| --- | --- | --- |
| `max_size` | `"10M"` | shep's size grammar: `K`, `M`, `G`, uppercase, binary. `10M` is 10 MiB. `10MB` is refused. |
| `max_age` | unset | Rotates a log this long after its last rotation, whatever its size. A log never rotated counts from when it appeared, or from its last write on a filesystem that keeps no birth time. Unset means size only. |
| `keep` | `5` | Counts rotated generations, never the live file. `keep = 0` is refused, since it would delete each rotation as it was made. |
| `naming` | `"dated"` | See below. |
| `compress` | `true` | The newest generation is left plain so it still greps without a decompression step. |
| `interval` | `"60s"` | How long between passes. `0` is refused. |

`max_age` and `interval` use shep's duration grammar, which has days,
hours, minutes, seconds and milliseconds in lowercase and no week unit. A week
is `"7d"` or `"168h"`, not `"1w"`. A bare number is milliseconds, so `max_age = "7"` means seven
milliseconds and not seven of anything else.

The dog re-reads this table on every pass, so editing `dogs.toml` takes
effect on the next interval without a restart. That is this dog's own
choice rather than something shep does for it: the shepherd serves the
section fresh on every request and never pushes one, so a dog that asked
only at startup would need bouncing.

A value the dog cannot use is the exception, and it stops the dog rather
than the pass. Nothing about a rejected `max_age` changes until somebody
edits the file, so a dog that went back round the loop would write the same
complaint into its own log every interval while `shep list` called it online
and it rotated nothing at all. It prints the value it refused and exits
instead, which the shepherd shows as a dog that is down. Fix the value and
start it again with `shep restart log-rotate`, under whatever name you
adopted it.

`shep lookout` can edit the same table. Press `s` for the dog list, then `e`
on this dog's row, and you get a form built from the schema `--schema`
prints: one row per setting, with the two grammars above resolved, so a
bare `interval = "10"` reads as ten milliseconds rather than as ten of
something you have to guess at. Writing needs `--allow-control`.

## Setting a rule for one sheep

`[log-rotate]` is the value for every sheep. A sheep that needs something
else gets a table of its own, `[app.dogs.log-rotate]` in its config, and
every key it sets there wins over the section for that sheep alone. Keys it
leaves out come from `[log-rotate]`, so a table that sets only `keep` rotates
on the section's `max_size`, `naming` and `compress`.

```sh
# the chatty one rotates early and keeps more
shep set api.dogs.log-rotate.max_size 50M
shep set api.dogs.log-rotate.keep 10

# a rule for every sheep, in the dog's own section of dogs.toml
shep set log-rotate.keep 3
```

If you adopted the dog under another name, that name replaces `log-rotate`
in both addresses.

- `max_size`, `max_age`, `keep`, `naming` and `compress` can be set per sheep.
- `interval` cannot: there is one loop, so it is set in `[log-rotate]` only.
  A sheep whose table sets it is left alone, as below.
- A sheep with no table rotates on `[log-rotate]`. There is no switch to turn
  rotation off for one sheep; a `max_size` larger than the log will ever grow
  does that.
- Both are re-read on every pass, like the section.
- A sheep's table that the dog cannot use leaves that sheep alone for the
  pass: its logs are neither rotated nor pruned, and the pass's one summary
  line says which sheep and which key. The other sheep rotate as usual. A log
  that sheep shares with another sheep is left alone for both, since a sheep
  that is not reopened would go on writing into the renamed file.
- Two sheep sharing one log path are one log to this dog. The first sheep in
  the flock's order decides whether it has grown enough to rotate, with its
  own `max_size` and `max_age`. The sheep that then renames it, normally that
  same sheep, names the new generation and prunes and compresses the
  generations by its own `naming`, `keep` and `compress`. The other sheep's
  settings for that path are not consulted.

## The two naming schemes

`dated` names each generation after the moment it was rotated:

```
web-0-out.log                              the live log
web-0-out.2026-08-20T15-04-05.log          rotated at that time
web-0-out.2026-08-20T15-04-05.1.log        a second rotation in the same second
```

The timestamp is UTC, always, whatever the machine's timezone is set to.
Sorting the directory by name puts the generations in order, and a
generation is never renamed once it has been written. The names still end
in `.log`, so a glob or a log shipper watching `*.log` keeps working.

`numeric` is the logrotate convention:

```
web-0-out.log       the live log
web-0-out.log.1     the newest generation
web-0-out.log.2     older
web-0-out.log.3     older still
```

Every rotation shifts the whole series down by one before renaming the live
file into `.1`. The names are short and predictable, which is what makes
them easy to write a script against. Note that `.1` is the newest here.
macOS `newsyslog` numbers the same idea from `.0`, so a script carried over
from a Mac is off by one.

The cost of `numeric` is renames. `dated` does one per rotation; `numeric`
does one per generation it is keeping, every time. With `keep = 5` that is
six renames instead of one, and each of them is a moment where a file
briefly does not have its usual name.

`numeric` also has a name collision `dated` cannot have. A log with no
extension and a log whose extension is a number produce the same names: a
sheep writing to `/var/log/web` rotates into `/var/log/web.1`, and that may
already be a different sheep's live log. Both readings are correct, and no
amount of care in the matcher can separate them, so the dog asks the
shepherd instead. If a name it would rotate into is a live log belonging to
anything in the flock, it leaves that log alone entirely and says so:

```
rotated 0, compressed 0, deleted 0, left 1 alone whose rotated name is a live log
```

That is not something the dog can fix. Give the two logs different names, or
use `dated`.

### Switching between them

Switching `naming` does not migrate anything. The old scheme's files stop
matching, which means they also stop being counted against `keep` and stop
being deleted. They sit there until you move them or remove them yourself.

That is the safe direction to fail in. The alternative is a rotator that
deletes files it does not recognise, and a rotator that deletes files it
does not recognise will eventually delete something that was not its.

## What it does not rotate

`shepd.out.log` and `shepd.err.log` are left alone, and that is not an
oversight. The daemon is not a member of its own flock, so the dog never
sees those paths, and there would be nothing safe to do with them if it
did: shep has no way to tell the daemon to reopen its own log files.
Renaming one would leave the daemon writing into a file with no name, and
its output would stop appearing. On a shepherd that runs for months those
two files grow without limit. Rotating them is a job for whatever manages
the shepherd, or for a `shep kill` and a restart.

## What it does rotate, including itself

Dogs are ordinary supervised processes with a marker on them, so `shep dogs`
and the flock listing both report them, and this dog rotates their logs
along with everybody else's. That includes its own. Seeing
`log-rotate-0-out.log.1` next to your services' generations is expected: a
log directory that is bounded except for four files is the surprising
behaviour, not this.

It does mean the dog has to be quiet, and it is. Only a pass that renamed
something, refused to, or was refused a reopen prints its one line. If this
dog logged a line per file per interval, its own log would be the busiest
file in the directory it exists to keep small.

## The window between rename and reopen

Rotating a file that something has open is not atomic and cannot be made
atomic. The dog renames `web-0-out.log` to its new name, then asks the
shepherd to reopen it. In between, shep is still holding a descriptor that
now points at the renamed file, and anything the sheep writes goes there.

Those lines are not lost. They land at the end of the previous generation
instead of at the start of the new one, which is a slightly wrong boundary
rather than a hole. shep reopens with create-and-append, so the reopen never
truncates anything either.

This is the honest outcome rather than the ideal one, and it matters if you
are counting on a generation boundary meaning something exact. The
integration tests drive a real shepherd, with a sheep printing a counter as
fast as it can through about twenty-five rotations per run. They then
concatenate every generation and look for holes in the counter. There are
none, in either scheme.

## Building from source

```sh
git clone https://github.com/shep-pm/shep-log-rotate
cd shep-log-rotate
cargo test
```

The real-shepherd tests are behind a feature, because they need a `shep`
binary a fresh clone does not have:

```sh
SHEP_BIN=/path/to/shep cargo test --features integration
```

Cargo skips a target whose required features are off without saying so, so a
plain `cargo test` runs a test called
`heads_up_the_real_shepherd_tier_is_not_running` whose whole job is to put
that name in the output.

## A note on the dependency

`Cargo.toml` reads `shep-client = "0.24.0"`, by version alone, from crates.io.
It carried a git URL alongside until shep published on 2026-08-26, which was the
only thing blocking this crate from being published itself.

The floor is the shep this dog is built and tested against. `DogRuntime` opens
its session and `Client::dog_config` reads its section, so what this dog leans on
is shep's own shape for a dog, and that shape moves with shep. `Cargo.toml` names
the older release each piece arrived in and why each still holds: the
`dog_config` attribute from 0.10.0, `DogRuntime` from 0.11.0, the resolved config
request from 0.13.0, the permission manifest from 0.17.0, and `ReconnectingClient`'s dog-named handshake from 0.1.23,
which is still what keeps this dog from being rendered `silent` after a handover.

The floor and the lockfile answer different questions. `^0.24.0` is the oldest
client this code compiles against. `Cargo.lock` pins whichever release was
newest when it was last refreshed, and that is what a reproducible build of
this crate ships: `cargo install` honours a packaged lockfile under `--locked`,
so that file decides what an installer compiles rather than what merely could
compile.

Depending on the published surface rather than on a path is the point, not an
accident. This project exists partly to find out whether somebody outside
shep's own workspace can build a dog with what shep publishes, and a path
dependency would quietly paper over anything missing from it.

CI tests that pairing rather than assuming it. The `integration` job installs
`shep` from crates.io and drives a real shepherd with it, so a red job is a
signal against shep's current published surface rather than against whatever
`main` looks like on a given day. A change in a shep release turns this
repository red rather than reaching an operator first.

## License

MIT OR Apache-2.0, at your option.
