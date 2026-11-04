//! Recognising a generation name as one this dog wrote.
//!
//! A child of `naming`, and the half a delete path runs through: a name that
//! matches here is a file this dog wrote and may prune, and a name that
//! matches here wrongly is an operator's data. The shapes it reads against are
//! the parent's constants, shared with the `build` half that writes them.

use core::cmp::Ordering;
use std::time::SystemTime;

use crate::config::Naming;

use super::{GZ_SUFFIX, LogPath, STAMP_FORMAT, STAMP_SHAPE, is_stamp};

#[cfg(doc)]
use super::stamp_utc;

/// Where one generation sits in its scheme's ordering.
///
/// The [`Ord`] implementation is **newest first**, not the field order a
/// derive would give: `sort` and [`Order::newest_first`] therefore agree, and
/// a caller on the pruning path cannot get the direction wrong by reaching
/// for the shorter one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Order {
    /// A dated generation. `counter` is 0 when the name carried none.
    Dated {
        /// The 19-character UTC stamp, exactly as it appears in the name.
        stamp: String,
        /// The same-second collision counter, or 0 for a name without one.
        counter: u32,
    },
    /// A numeric generation. `n` is 1 for the newest, following logrotate.
    Numeric {
        /// The generation number, always 1 or more.
        n: u32,
    },
}

impl Order {
    /// Compare two generations newest first, for `sort_by`.
    ///
    /// Named rather than written as a closure so the direction is legible at
    /// the call site, which is a delete path.
    ///
    /// Dated generations order by `(stamp, counter)` descending. That the
    /// counter is part of the key is not a detail: sorting the file names
    /// lexicographically puts `...T15-04-05.1.log` *before*
    /// `...T15-04-05.log`, because `'1' < 'l'`, which is backwards. Numeric
    /// generations order by `n` ascending, because `.1` is the newest.
    pub fn newest_first(a: &Self, b: &Self) -> Ordering {
        a.cmp(b)
    }
}

impl Ord for Order {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (
                Self::Dated { stamp, counter },
                Self::Dated {
                    stamp: other_stamp,
                    counter: other_counter,
                },
            ) => other_stamp
                .cmp(stamp)
                .then_with(|| other_counter.cmp(counter)),
            (Self::Numeric { n }, Self::Numeric { n: other_n }) => n.cmp(other_n),
            // The two schemes never share a directory's worth of candidates,
            // so this arm exists only to make the ordering total.
            (Self::Dated { .. }, Self::Numeric { .. }) => Ordering::Less,
            (Self::Numeric { .. }, Self::Dated { .. }) => Ordering::Greater,
        }
    }
}

impl PartialOrd for Order {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Read a stamp [`stamp_utc`] produced back into the instant it names.
///
/// `None` for anything that is not exactly that shape. The one caller
/// hands it stamps [`match_generation`] already accepted, so `None` there
/// is a bug rather than a case, and it is treated as "no rotation on
/// record" rather than panicked on.
pub fn unstamp_utc(stamp: &str) -> Option<SystemTime> {
    if !is_stamp(stamp.as_bytes()) {
        return None;
    }
    let civil = jiff::civil::DateTime::strptime(STAMP_FORMAT, stamp).ok()?;
    let zoned = civil.to_zoned(jiff::tz::TimeZone::UTC).ok()?;
    Some(SystemTime::from(zoned.timestamp()))
}

/// Recognise `file_name` as a generation of `base` under `naming`, reporting
/// where it sits in the ordering and whether it is already compressed.
///
/// `None` means "this dog did not write that file", and the pruning path must
/// treat it as untouchable. Matching works by stripping known pieces off the
/// ends, never by searching for a date somewhere in the middle, so a name
/// that merely contains a timestamp is not a match.
pub fn match_generation(base: &LogPath, naming: Naming, file_name: &str) -> Option<(Order, bool)> {
    let attempt = |rest: &str| match naming {
        Naming::Dated => match_dated(base, rest),
        Naming::Numeric => match_numeric(base, rest),
    };
    // A trailing `.gz` is normally the marker this dog adds when it
    // compresses. It is ambiguous only for a log whose own extension is
    // `gz`, where `web-0-out.gz` rotates to `web-0-out.<stamp>.gz` with
    // nothing compressed at all. So read the `.gz` as the marker first, and
    // fall back to reading it as the extension. Both readings accept only an
    // exactly-generated name, so the fallback cannot widen what matches.
    if let Some(rest) = file_name.strip_suffix(GZ_SUFFIX)
        && let Some(order) = attempt(rest)
    {
        return Some((order, true));
    }
    Some((attempt(file_name)?, false))
}

/// Match `rest` (already `.gz`-stripped) as `{stem}.{stamp}[.{counter}][.{ext}]`.
fn match_dated(base: &LogPath, rest: &str) -> Option<Order> {
    let rest = strip_ext(base, rest)?;
    let middle = rest.strip_prefix(&base.stem)?.strip_prefix('.')?;

    let bytes = middle.as_bytes();
    let (stamp, tail) = bytes.split_at_checked(STAMP_SHAPE.len())?;
    if !is_stamp(stamp) {
        return None;
    }
    let counter = match tail {
        [] => 0,
        [b'.', digits @ ..] => parse_index(digits)?,
        _ => return None,
    };
    Some(Order::Dated {
        // `is_stamp` accepted it, so the first 19 bytes are ASCII and this
        // slice is on a character boundary.
        stamp: middle[..STAMP_SHAPE.len()].to_owned(),
        counter,
    })
}

/// Match `rest` (already `.gz`-stripped) as `{stem}.{ext}.{n}`.
fn match_numeric(base: &LogPath, rest: &str) -> Option<Order> {
    let live = base.live_name();
    let digits = rest.strip_prefix(&live)?.strip_prefix('.')?;
    Some(Order::Numeric {
        n: parse_index(digits.as_bytes())?,
    })
}

/// Strip the base's final extension off `rest`, or `None` if it is not there.
///
/// A base with no extension has nothing to strip and always succeeds.
fn strip_ext<'a>(base: &LogPath, rest: &'a str) -> Option<&'a str> {
    match &base.ext {
        Some(ext) => rest.strip_suffix(ext)?.strip_suffix('.'),
        None => Some(rest),
    }
}

/// Parse a generation index: ASCII digits, no sign, no leading zero, 1 or
/// more, and within `u32`.
///
/// Every rejection here is a shape this dog never writes. `.0` is
/// `newsyslog`'s newest and not ours, and a counter of 0 is written as no
/// counter at all; `.01` is nobody's.
fn parse_index(digits: &[u8]) -> Option<u32> {
    if digits.first() == Some(&b'0') || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    // `from_utf8` cannot fail: every byte was just checked to be a digit.
    core::str::from_utf8(digits).ok()?.parse::<u32>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::naming::{assert_no_match, base, dated};
    use std::path::Path;

    #[test]
    fn a_gz_suffix_matches_and_is_reported() {
        let (order, compressed) = match_generation(
            &base(),
            Naming::Dated,
            "web-0-out.2026-08-20T15-04-05.log.gz",
        )
        .expect("matches");
        assert!(compressed);
        assert_eq!(order, dated("2026-08-20T15-04-05", 0));
    }

    #[test]
    fn near_misses_are_not_matches() {
        // Each of these has a date in the name, or nearly the right shape.
        // None of them was written by this dog, so none may ever be deleted.
        assert_no_match(
            &base(),
            Naming::Dated,
            &[
                "web-0-out.2026-08-20.log",                 // no time
                "web-0-out.2026-08-20T15-04.log",           // no seconds
                "web-0-out.2026-8-20T15-04-05.log",         // not zero padded
                "web-0-out.backup-2026-08-20T15-04-05.log", // prefixed
                "web-0-out.2026-08-20T15-04-05.log.bak",    // wrong trailing suffix
                "web-0-out.2026-08-20T15-04-05",            // extension dropped
                "web-1-out.2026-08-20T15-04-05.log",        // a different sheep
                "web-0-err.2026-08-20T15-04-05.log",        // the other stream
                "web-0-out.log",                            // the live file itself
            ],
        );
    }

    #[test]
    fn numeric_near_misses_are_not_matches() {
        assert_no_match(
            &base(),
            Naming::Numeric,
            &[
                "web-0-out.log.0", // logrotate starts at 1; .0 is newsyslog's
                "web-0-out.log.x", // not a number
                "web-0-out.log",   // the live file itself
                "web-0-out.1.log", // dated-scheme shape
                "web-1-out.log.1", // a different sheep
            ],
        );
    }

    #[test]
    fn a_scheme_does_not_match_the_other_schemes_files() {
        assert!(
            match_generation(
                &base(),
                Naming::Numeric,
                "web-0-out.2026-08-20T15-04-05.log"
            )
            .is_none()
        );
        assert!(match_generation(&base(), Naming::Dated, "web-0-out.log.1").is_none());
    }

    #[test]
    fn a_date_that_never_happened_is_not_a_match() {
        // `stamp_utc` renders a real instant, so it can never write any of
        // these. A matcher that accepts them hands Task 5 a file to delete
        // that this dog did not create.
        let impossible = [
            "web-0-out.2026-02-30T00-00-00.log", // February has no 30th
            "web-0-out.2026-02-29T00-00-00.log", // 2026 is not a leap year
            "web-0-out.2100-02-29T00-00-00.log", // a century that is not a leap year
            "web-0-out.2026-04-31T00-00-00.log", // April has 30 days
            "web-0-out.2026-06-31T00-00-00.log",
            "web-0-out.2026-09-31T00-00-00.log",
            "web-0-out.2026-11-31T00-00-00.log",
            "web-0-out.1969-07-20T20-17-40.log", // before the epoch
            "web-0-out.0000-01-01T00-00-00.log",
        ];
        for candidate in impossible {
            assert!(
                match_generation(&base(), Naming::Dated, candidate).is_none(),
                "{candidate} is not a date this dog can have written"
            );
        }
    }

    // ---------------------------------------------------------------------
    // Beyond the near-miss decoys above. Every case below is a file an
    // operator might really have in a log directory, and every one of them
    // is a file this dog must never delete.
    // ---------------------------------------------------------------------

    #[test]
    fn a_name_that_is_only_separators_is_never_a_generation() {
        let junk = ["", ".", "..", "...", ".log", ".gz", ".log.gz", "gz"];
        for naming in [Naming::Dated, Naming::Numeric] {
            for name in junk {
                assert!(
                    match_generation(&base(), naming, name).is_none(),
                    "{name:?} must not match under {naming:?}"
                );
            }
        }
    }

    #[test]
    fn a_stem_that_merely_starts_with_ours_is_a_different_sheep() {
        // "web-0-out" is a prefix of "web-0-outer". Requiring the separator
        // after the stem is what keeps the longer name out.
        assert_no_match(
            &base(),
            Naming::Dated,
            &[
                "web-0-outer.2026-08-20T15-04-05.log",
                "web-0-out2.2026-08-20T15-04-05.log",
            ],
        );
        assert_no_match(
            &base(),
            Naming::Numeric,
            &["web-0-outer.log.1", "web-0-out2.log.1"],
        );
    }

    #[test]
    fn a_counter_this_dog_would_never_write_is_not_a_match() {
        // Counter 0 is written as no counter at all, and counters carry no
        // leading zero, no sign and no padding. Anything else is somebody
        // else's convention.
        assert_no_match(
            &base(),
            Naming::Dated,
            &[
                "web-0-out.2026-08-20T15-04-05.0.log", // 0 is written as absent
                "web-0-out.2026-08-20T15-04-05.00.log", // and padded 0 doubly so
                "web-0-out.2026-08-20T15-04-05.01.log", // leading zero
                "web-0-out.2026-08-20T15-04-05.+1.log", // signed
                "web-0-out.2026-08-20T15-04-05.-1.log", // signed
                "web-0-out.2026-08-20T15-04-05.1x.log", // trailing junk
                "web-0-out.2026-08-20T15-04-05..log",  // empty counter
                "web-0-out.2026-08-20T15-04-05.1.2.log", // two counters
                "web-0-out.2026-08-20T15-04-05.4294967296.log", // one past u32
                "web-0-out.2026-08-20T15-04-05 .log",  // trailing space
                "web-0-out.2026-08-20T15-04-050.log",  // stamp run on
            ],
        );
    }

    #[test]
    fn a_right_length_stamp_of_nonsense_digits_is_not_a_match() {
        // The shape is right, so a matcher that only counted digits would
        // delete these. No real clock produced any of them.
        assert_no_match(
            &base(),
            Naming::Dated,
            &[
                "web-0-out.9999-99-99T99-99-99.log",
                "web-0-out.2026-00-20T15-04-05.log", // month 0
                "web-0-out.2026-13-20T15-04-05.log", // month 13
                "web-0-out.2026-08-00T15-04-05.log", // day 0
                "web-0-out.2026-08-32T15-04-05.log", // day 32
                "web-0-out.2026-08-20T24-04-05.log", // hour 24
                "web-0-out.2026-08-20T15-60-05.log", // minute 60
                "web-0-out.2026-08-20T15-04-60.log", // second 60
            ],
        );
    }

    #[test]
    fn case_and_non_ascii_lookalikes_are_not_matches() {
        assert_no_match(
            &base(),
            Naming::Dated,
            &[
                "WEB-0-OUT.2026-08-20T15-04-05.log",     // shouted stem
                "web-0-out.2026-08-20T15-04-05.LOG",     // shouted extension
                "web-0-out.2026-08-20t15-04-05.log",     // lowercase separator
                "web-0-out.2026-08-20T15-04-05.log.GZ",  // shouted gz
                "web-0-out.２０２６-08-20T15-04-05.log", // full-width digits
            ],
        );
    }

    #[test]
    fn a_gz_is_stripped_exactly_once_and_only_at_the_end() {
        // One .gz is ours. A second, or a .gz with anything after it, is an
        // archive somebody else made.
        assert_no_match(
            &base(),
            Naming::Dated,
            &[
                "web-0-out.2026-08-20T15-04-05.log.gz.gz",
                "web-0-out.2026-08-20T15-04-05.gz.log",
                "web-0-out.2026-08-20T15-04-05.log.tar.gz",
                "web-0-out.log.gz", // the live file, compressed by hand
            ],
        );
        // A counter and a .gz together is a shape this dog does write.
        let (order, compressed) = match_generation(
            &base(),
            Naming::Dated,
            "web-0-out.2026-08-20T15-04-05.7.log.gz",
        )
        .expect("matches");
        assert!(compressed);
        assert_eq!(order, dated("2026-08-20T15-04-05", 7));
    }

    #[test]
    fn a_log_whose_own_extension_is_gz_reads_the_suffix_both_ways() {
        // The one place the grammar is genuinely ambiguous: a log called
        // web-0-out.gz rotates to web-0-out.<stamp>.gz with nothing
        // compressed, and only a second .gz means compression.
        let gz = LogPath::split(Path::new("/var/log/web-0-out.gz")).expect("splits");
        assert_eq!(
            match_generation(&gz, Naming::Dated, "web-0-out.2026-08-20T15-04-05.gz"),
            Some((dated("2026-08-20T15-04-05", 0), false))
        );
        assert_eq!(
            match_generation(&gz, Naming::Dated, "web-0-out.2026-08-20T15-04-05.gz.gz"),
            Some((dated("2026-08-20T15-04-05", 0), true))
        );
        assert_eq!(
            match_generation(&gz, Naming::Numeric, "web-0-out.gz.1.gz"),
            Some((Order::Numeric { n: 1 }, true))
        );
        // The live file, and the live file compressed by hand, are still not
        // generations of themselves.
        for decoy in ["web-0-out.gz", "web-0-out.gz.gz"] {
            for naming in [Naming::Dated, Naming::Numeric] {
                assert!(
                    match_generation(&gz, naming, decoy).is_none(),
                    "{decoy} must not match under {naming:?}"
                );
            }
        }
    }

    #[test]
    fn numeric_generations_compress_and_stop_at_u32() {
        let (order, compressed) =
            match_generation(&base(), Naming::Numeric, "web-0-out.log.2.gz").expect("matches");
        assert!(compressed);
        assert_eq!(order, Order::Numeric { n: 2 });

        let (order, _) = match_generation(&base(), Naming::Numeric, "web-0-out.log.4294967295")
            .expect("u32::MAX still matches");
        assert_eq!(order, Order::Numeric { n: u32::MAX });

        assert_no_match(
            &base(),
            Naming::Numeric,
            &[
                "web-0-out.log.4294967296", // one past u32
                "web-0-out.log.01",         // leading zero
                "web-0-out.log.+1",         // signed
                "web-0-out.log.-1",         // signed
                "web-0-out.log.1x",         // trailing junk
                "web-0-out.log.",           // empty
                "web-0-out.log.1.log",      // dressed up as a log again
                "web-0-out.log.log.1",      // doubled extension
            ],
        );
    }

    #[test]
    fn newest_first_ordering_puts_the_counter_after_the_plain_name() {
        // A plain lexicographic sort gets this backwards, because '1' < 'l'.
        let mut orders = [
            dated("2026-08-20T15-04-05", 0),
            dated("2026-08-20T15-04-05", 2),
            dated("2026-08-20T15-04-06", 0),
        ];
        orders.sort_by(Order::newest_first);
        assert_eq!(orders[0], dated("2026-08-20T15-04-06", 0));
        assert_eq!(orders[1], dated("2026-08-20T15-04-05", 2));
        assert_eq!(orders[2], dated("2026-08-20T15-04-05", 0));
    }

    #[test]
    fn numeric_newest_first_is_one_before_two() {
        let mut orders = vec![
            Order::Numeric { n: 3 },
            Order::Numeric { n: 1 },
            Order::Numeric { n: 2 },
        ];
        orders.sort_by(Order::newest_first);
        assert_eq!(
            orders,
            vec![
                Order::Numeric { n: 1 },
                Order::Numeric { n: 2 },
                Order::Numeric { n: 3 }
            ]
        );
    }

    #[test]
    fn the_natural_order_is_the_same_newest_first_order() {
        // sort() and sort_by(newest_first) must not disagree: the shorter one
        // is on a delete path, and a derived ordering would run it backwards.
        let mut natural = vec![
            dated("2026-08-20T15-04-05", 0),
            dated("2026-08-20T15-04-06", 0),
            dated("2026-08-20T15-04-05", 2),
        ];
        let mut explicit = natural.clone();
        natural.sort();
        explicit.sort_by(Order::newest_first);
        assert_eq!(natural, explicit);
        assert_eq!(natural[0], dated("2026-08-20T15-04-06", 0));
    }
}
